//! Deletion sweep: cleans up all object-store data for a deleted topic.
//!
//! Spawned as a `tokio::spawn` task per pending delete. Idempotent: reads
//! the snapshot manifests, deletes each segment key, deletes the manifest,
//! then does a one-shot LIST of the topic UUID prefix to catch orphan
//! segments (Uploader PUT-succeeded-but-manifest-update-crashed cases that
//! retention's manifest-only sweep can never find).
//!
//! The LIST here is the ONLY place in the broker that lists the object
//! store. Deletion is not a hot path.
//!
//! DELETE calls tolerate `object_store::Error::NotFound`: a sweep that
//! partially completes (e.g. crashes after deleting some segments but
//! before removing the `pending_deletes.json` entry) gets replayed at the
//! next broker startup against the *same* snapshot manifest. Without
//! NotFound tolerance, that replay would immediately fail on the
//! already-deleted keys and the pending record could never be cleared.
//! Treating "already gone" as success makes the sweep genuinely idempotent
//! on replay, which is the property `pending_deletes` replay depends on.
//!
//! See `docs/superpowers/specs/2026-09-22-delete-topic-design.md`.

use crate::metrics::{
    DELETE_BYTES_REMOVED, DELETE_DURATION_MS, DELETE_ERRORS, DELETE_PENDING_TOPICS,
    DELETE_SEGMENTS_REMOVED, LABEL_TOPIC,
};
use crate::object_store::{manifest_key, segment_key};
use crate::pending_deletes::{self, PendingDelete};
use anyhow::Result;
use futures::stream::StreamExt;
use object_store::path::Path as ObjPath;
use object_store::{Error as ObjStoreError, ObjectStore};
use std::sync::Arc;
use std::time::Instant;

/// Delete `key`, treating "already gone" as success. Returns `Ok(true)` if
/// this call actually removed an object, `Ok(false)` if the key was already
/// absent, and `Err` for any other object-store failure.
async fn delete_lenient(store: &Arc<dyn ObjectStore>, key: &ObjPath) -> Result<bool> {
    match store.delete(key).await {
        Ok(()) => Ok(true),
        Err(ObjStoreError::NotFound { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub async fn sweep_deletion(
    record: PendingDelete,
    store: Arc<dyn ObjectStore>,
    prefix: String,
    data_dir: String,
) -> Result<()> {
    let start = Instant::now();
    let topic_label = record.topic.clone();
    let mut segments_removed: u64 = 0;
    let mut bytes_removed: u64 = 0;

    // 1. Delete every segment listed in the snapshot manifests.
    for (partition, manifest) in &record.manifests_by_partition {
        for seg in &manifest.segments {
            let key: ObjPath = segment_key(
                &prefix,
                &record.topic,
                &record.uuid,
                *partition,
                seg.base_offset,
            );
            match delete_lenient(&store, &key).await {
                Ok(removed) => {
                    if removed {
                        segments_removed += 1;
                        bytes_removed += seg.byte_size;
                    }
                }
                Err(e) => {
                    metrics::counter!(DELETE_ERRORS, LABEL_TOPIC => topic_label.clone())
                        .increment(1);
                    log::warn!(
                        "sweep_deletion: delete failed for {key:?}: {e:?}; will retry on next replay"
                    );
                    return Err(e);
                }
            }
        }
        let mkey: ObjPath = manifest_key(&prefix, &record.topic, &record.uuid, *partition);
        if let Err(e) = delete_lenient(&store, &mkey).await {
            metrics::counter!(DELETE_ERRORS, LABEL_TOPIC => topic_label.clone()).increment(1);
            log::warn!(
                "sweep_deletion: delete failed for {mkey:?}: {e:?}; will retry on next replay"
            );
            return Err(e);
        }
    }

    // 2. One-shot LIST the topic UUID prefix and delete any orphan objects
    //    not present in the snapshot manifests.
    let list_prefix = build_list_prefix(&prefix, &record.topic, &record.uuid);
    let mut stream = store.list(Some(&ObjPath::from(list_prefix)));
    while let Some(meta) = stream.next().await {
        match meta {
            Ok(m) => {
                if let Err(e) = delete_lenient(&store, &m.location).await {
                    metrics::counter!(DELETE_ERRORS, LABEL_TOPIC => topic_label.clone())
                        .increment(1);
                    log::warn!(
                        "sweep_deletion: orphan delete failed for {:?}: {e:?}",
                        m.location
                    );
                }
            }
            Err(e) => {
                metrics::counter!(DELETE_ERRORS, LABEL_TOPIC => topic_label.clone()).increment(1);
                log::warn!("sweep_deletion: list stream error: {e:?}");
                return Err(e.into());
            }
        }
    }

    // 3. Remove this record from pending_deletes.json.
    pending_deletes::remove(&data_dir, &record.uuid).await?;

    metrics::histogram!(DELETE_DURATION_MS, LABEL_TOPIC => topic_label.clone())
        .record(start.elapsed().as_secs_f64() * 1000.0);
    metrics::counter!(DELETE_SEGMENTS_REMOVED, LABEL_TOPIC => topic_label.clone())
        .increment(segments_removed);
    metrics::counter!(DELETE_BYTES_REMOVED, LABEL_TOPIC => topic_label.clone())
        .increment(bytes_removed);
    metrics::gauge!(DELETE_PENDING_TOPICS).decrement(1.0);

    Ok(())
}

fn build_list_prefix(prefix: &str, topic: &str, topic_uuid: &str) -> String {
    let mut s = String::new();
    if !prefix.is_empty() {
        s.push_str(prefix.trim_end_matches('/'));
        s.push('/');
    }
    s.push_str(&format!("{topic}/v={topic_uuid}/"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{build_store, put};
    use bytes::Bytes;
    use kafkrs_models::config::ObjectStoreConfig;
    use kafkrs_models::manifest::{Manifest, SegmentEntry};
    use std::collections::BTreeMap;

    fn fs_cfg() -> ObjectStoreConfig {
        ObjectStoreConfig {
            backend: "filesystem".into(),
            bucket: "b".into(),
            prefix: "".into(),
            endpoint: "".into(),
            region: "us-east-1".into(),
        }
    }

    fn seg(base: i64) -> SegmentEntry {
        SegmentEntry {
            base_offset: base,
            last_offset: base + 9,
            base_timestamp_ns: 0,
            last_timestamp_ns: 10,
            record_count: 10,
            byte_size: 100,
            object_key: format!("segment-{:020}.parquet", base),
        }
    }

    #[tokio::test]
    async fn sweep_removes_all_manifest_segments() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        let store = build_store(&fs_cfg(), dd).unwrap();
        let topic = "orders";
        let uuid = "01936a80-0000-7000-8000-000000000000";

        // Pre-place two segments + a manifest.
        for base in [0, 10] {
            let k = segment_key("", topic, uuid, 0, base);
            put(&store, &k, Bytes::from_static(b"seg")).await.unwrap();
        }
        let mut m = Manifest::empty(topic, 0);
        m.segments = vec![seg(0), seg(10)];
        let mk = manifest_key("", topic, uuid, 0);
        put(&store, &mk, Bytes::from(serde_json::to_vec(&m).unwrap()))
            .await
            .unwrap();

        let mut by_p = BTreeMap::new();
        by_p.insert(0u32, m);
        let record = PendingDelete {
            topic: topic.into(),
            uuid: uuid.into(),
            manifests_by_partition: by_p,
            created_ns: 0,
        };
        pending_deletes::append(dd, record.clone()).await.unwrap();

        sweep_deletion(record, store.clone(), "".into(), dd.into())
            .await
            .unwrap();

        // Both segment keys and the manifest key should be gone.
        for base in [0, 10] {
            let k = segment_key("", topic, uuid, 0, base);
            assert!(crate::object_store::get(&store, &k).await.is_err(), "{k}");
        }
        assert!(crate::object_store::get(&store, &mk).await.is_err());

        // Pending record was removed.
        let remaining = pending_deletes::load_all(dd).await.unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn sweep_removes_orphan_not_in_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        let store = build_store(&fs_cfg(), dd).unwrap();
        let topic = "orders";
        let uuid = "01936a80-0000-7000-8000-000000000001";

        // Place an orphan object under the topic UUID prefix (not in manifest).
        let orphan_key = segment_key("", topic, uuid, 0, 999);
        put(&store, &orphan_key, Bytes::from_static(b"orphan"))
            .await
            .unwrap();

        // Empty snapshot manifest.
        let m = Manifest::empty(topic, 0);
        let mut by_p = BTreeMap::new();
        by_p.insert(0u32, m);
        let record = PendingDelete {
            topic: topic.into(),
            uuid: uuid.into(),
            manifests_by_partition: by_p,
            created_ns: 0,
        };
        pending_deletes::append(dd, record.clone()).await.unwrap();

        sweep_deletion(record, store.clone(), "".into(), dd.into())
            .await
            .unwrap();

        assert!(
            crate::object_store::get(&store, &orphan_key).await.is_err(),
            "orphan should be gone"
        );
    }

    #[tokio::test]
    async fn sweep_is_idempotent_on_replay() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        let store = build_store(&fs_cfg(), dd).unwrap();
        let topic = "orders";
        let uuid = "01936a80-0000-7000-8000-000000000002";

        // Single manifest with one segment.
        let mut m = Manifest::empty(topic, 0);
        m.segments = vec![seg(0)];
        let mk = manifest_key("", topic, uuid, 0);
        put(&store, &mk, Bytes::from(serde_json::to_vec(&m).unwrap()))
            .await
            .unwrap();

        let mut by_p = BTreeMap::new();
        by_p.insert(0u32, m);
        let record = PendingDelete {
            topic: topic.into(),
            uuid: uuid.into(),
            manifests_by_partition: by_p,
            created_ns: 0,
        };
        pending_deletes::append(dd, record.clone()).await.unwrap();

        put(
            &store,
            &segment_key("", topic, uuid, 0, 0),
            Bytes::from_static(b"x"),
        )
        .await
        .unwrap();
        sweep_deletion(record.clone(), store.clone(), "".into(), dd.into())
            .await
            .unwrap();

        // Second invocation: manifest and segments already gone from object
        // storage. Because `sweep_deletion` tolerates NotFound on DELETE,
        // replaying against the same snapshot manifest is a clean no-op
        // that still succeeds and clears the (re-appended) pending record.
        pending_deletes::append(dd, record.clone()).await.unwrap();
        sweep_deletion(record, store, "".into(), dd.into())
            .await
            .unwrap();

        let remaining = pending_deletes::load_all(dd).await.unwrap();
        assert!(remaining.is_empty());
    }
}
