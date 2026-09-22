//! Durable pending-delete state for the DeleteTopic sweep.
//!
//! One JSON file at `<data_dir>/pending_deletes.json` holds a list of
//! in-flight deletion sweeps. Writes go through `.tmp` + rename + fsync
//! so the file is always parseable. Only the registry actor writes; only
//! `main.rs` startup reads.
//!
//! See `docs/superpowers/specs/2026-09-22-delete-topic-design.md`.

use anyhow::{Context, Result};
use kafkrs_models::manifest::Manifest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tokio::sync::Mutex as AsyncMutex;

const FILE_NAME: &str = "pending_deletes.json";

/// Serializes the load-modify-write cycle in `append`/`remove` so concurrent
/// `handle_delete_topic` handlers (each spawned per-RPC) can't interleave
/// and silently drop each other's records. One file, one lock — acceptable
/// because there's one broker per process.
static FILE_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PendingDelete {
    pub topic: String,
    pub uuid: String,
    pub manifests_by_partition: BTreeMap<u32, Manifest>,
    pub created_ns: i64,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct PendingDeletesFile {
    #[serde(default)]
    pending: Vec<PendingDelete>,
}

fn path(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join(FILE_NAME)
}

pub async fn load_all(data_dir: &str) -> Result<Vec<PendingDelete>> {
    let p = path(data_dir);
    if !p.exists() {
        return Ok(Vec::new());
    }
    let raw = tokio::fs::read_to_string(&p)
        .await
        .with_context(|| format!("read {}", p.display()))?;
    let file: PendingDeletesFile =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", p.display()))?;
    Ok(file.pending)
}

pub async fn append(data_dir: &str, record: PendingDelete) -> Result<()> {
    let _guard = FILE_LOCK.lock().await;
    let mut current = load_all(data_dir).await?;
    current.push(record);
    write_all(data_dir, &current).await
}

pub async fn remove(data_dir: &str, topic_uuid: &str) -> Result<()> {
    let _guard = FILE_LOCK.lock().await;
    let mut current = load_all(data_dir).await?;
    current.retain(|r| r.uuid != topic_uuid);
    write_all(data_dir, &current).await
}

async fn write_all(data_dir: &str, pending: &[PendingDelete]) -> Result<()> {
    let target = path(data_dir);
    let tmp = target.with_extension("json.tmp");
    let file = PendingDeletesFile {
        pending: pending.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&file)?;

    // Ensure parent directory exists.
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }

    tokio::fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;

    // fsync the tmp file before rename.
    let tmp_std = tmp.clone();
    tokio::task::spawn_blocking(move || {
        use std::fs::OpenOptions;
        let f = OpenOptions::new().write(true).open(&tmp_std)?;
        f.sync_all()
    })
    .await??;

    tokio::fs::rename(&tmp, &target)
        .await
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kafkrs_models::manifest::{Manifest, SegmentEntry};

    fn sample() -> PendingDelete {
        let mut m = Manifest::empty("t", 0);
        m.segments.push(SegmentEntry {
            base_offset: 0,
            last_offset: 9,
            base_timestamp_ns: 0,
            last_timestamp_ns: 10,
            record_count: 10,
            byte_size: 42,
            object_key: "segment-00000000000000000000.parquet".into(),
        });
        let mut by_p = BTreeMap::new();
        by_p.insert(0, m);
        PendingDelete {
            topic: "orders".into(),
            uuid: "01936a80-0000-7000-8000-000000000000".into(),
            manifests_by_partition: by_p,
            created_ns: 1_700_000_000_000_000_000,
        }
    }

    #[tokio::test]
    async fn append_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        append(dd, sample()).await.unwrap();
        let back = load_all(dd).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0], sample());
    }

    #[tokio::test]
    async fn remove_removes_only_targeted_entry() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        let mut a = sample();
        a.uuid = "a-uuid".into();
        let mut b = sample();
        b.uuid = "b-uuid".into();
        append(dd, a.clone()).await.unwrap();
        append(dd, b.clone()).await.unwrap();
        remove(dd, "a-uuid").await.unwrap();
        let back = load_all(dd).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].uuid, "b-uuid");
    }

    #[tokio::test]
    async fn load_all_returns_empty_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        let back = load_all(dd).await.unwrap();
        assert!(back.is_empty());
    }

    #[tokio::test]
    async fn concurrent_appends_do_not_lose_records() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();

        let mut handles = Vec::new();
        for i in 0..10u32 {
            let dd = dd.clone();
            handles.push(tokio::spawn(async move {
                let mut r = sample();
                r.uuid = format!("uuid-{i}");
                r.topic = format!("topic-{i}");
                append(&dd, r).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let back = load_all(&dd).await.unwrap();
        assert_eq!(back.len(), 10, "concurrent appends should not lose records");
        let mut uuids: Vec<String> = back.iter().map(|r| r.uuid.clone()).collect();
        uuids.sort();
        for i in 0..10 {
            assert_eq!(uuids[i as usize], format!("uuid-{i}"));
        }
    }
}
