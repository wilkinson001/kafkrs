use crate::object_store::{get, manifest_key, put, segment_key};
use crate::segment::write_segment;
use anyhow::Result;
use bytes::Bytes;
use kafkrs_models::manifest::{Manifest, SegmentEntry};
use kafkrs_models::record::Record;
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// A sealed batch handed from the PartitionWriter to the Uploader.
pub struct SealedBatch {
    pub records: Vec<Record>,
    pub base_offset: i64,
    pub last_offset: i64,
    pub base_timestamp_ns: i64,
    pub last_timestamp_ns: i64,
}

pub enum UploaderMsg {
    Upload(SealedBatch),
}

/// Notification sent back when a segment is durable in the object store
/// (manifest updated). The PartitionWriter deletes the WAL file on receipt
/// (spec invariant 4).
#[derive(Debug, Clone)]
pub struct SegmentDurable {
    pub base_offset: i64,
}

pub struct Uploader {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    topic: String,
    partition: u32,
    rx: mpsc::Receiver<UploaderMsg>,
    durable_tx: mpsc::Sender<SegmentDurable>,
}

impl Uploader {
    pub fn new(
        store: Arc<dyn ObjectStore>,
        prefix: String,
        topic: String,
        partition: u32,
        rx: mpsc::Receiver<UploaderMsg>,
        durable_tx: mpsc::Sender<SegmentDurable>,
    ) -> Uploader {
        Uploader {
            store,
            prefix,
            topic,
            partition,
            rx,
            durable_tx,
        }
    }

    pub async fn run(mut self) {
        while let Some(UploaderMsg::Upload(batch)) = self.rx.recv().await {
            // Retry indefinitely: WAL retains the data (spec risk note).
            loop {
                match self.upload_once(&batch).await {
                    Ok(()) => break,
                    Err(e) => {
                        log::error!(
                            "upload failed for base_offset={}: {e:?}; retrying",
                            batch.base_offset
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
            let _ = self
                .durable_tx
                .send(SegmentDurable {
                    base_offset: batch.base_offset,
                })
                .await;
        }
    }

    async fn upload_once(&self, batch: &SealedBatch) -> Result<()> {
        let bytes: Bytes = write_segment(&batch.records)?;
        let byte_size: u64 = bytes.len() as u64;
        let seg_key: ObjPath =
            segment_key(&self.prefix, &self.topic, self.partition, batch.base_offset);
        // Idempotent: deterministic key, bit-identical content on re-upload.
        put(&self.store, &seg_key, bytes).await?;

        let m_key: ObjPath = manifest_key(&self.prefix, &self.topic, self.partition);
        let raw: Bytes = get(&self.store, &m_key).await?;
        let mut manifest: Manifest = serde_json::from_slice(&raw)?;
        let object_key: String = format!("segment-{:020}.parquet", batch.base_offset);
        if !manifest
            .segments
            .iter()
            .any(|s| s.base_offset == batch.base_offset)
        {
            manifest.segments.push(SegmentEntry {
                base_offset: batch.base_offset,
                last_offset: batch.last_offset,
                base_timestamp_ns: batch.base_timestamp_ns,
                last_timestamp_ns: batch.last_timestamp_ns,
                record_count: batch.records.len() as u64,
                byte_size,
                object_key,
            });
            manifest.segments.sort_by_key(|s| s.base_offset);
            let body: Vec<u8> = serde_json::to_vec(&manifest)?;
            put(&self.store, &m_key, Bytes::from(body)).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::build_store;
    use kafkrs_models::config::ObjectStoreConfig;

    fn fs_cfg() -> ObjectStoreConfig {
        ObjectStoreConfig {
            backend: "filesystem".into(),
            bucket: "b".into(),
            prefix: "".into(),
            endpoint: "".into(),
            region: "us-east-1".into(),
        }
    }

    fn rec(o: i64) -> Record {
        Record {
            offset: o,
            timestamp_ns: 1000 + o,
            schema_id: 0,
            key: vec![],
            value: vec![o as u8],
        }
    }

    #[tokio::test]
    async fn upload_writes_segment_and_appends_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(&fs_cfg(), dir.path().to_str().unwrap()).unwrap();
        // empty manifest precondition
        put(
            &store,
            &manifest_key("", "t", 0),
            bytes::Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap()),
        )
        .await
        .unwrap();

        let (tx, rx) = mpsc::channel(4);
        let (dtx, mut drx) = mpsc::channel(4);
        let up = Uploader::new(store.clone(), "".into(), "t".into(), 0, rx, dtx);
        let h = tokio::spawn(up.run());

        tx.send(UploaderMsg::Upload(SealedBatch {
            records: vec![rec(0), rec(1)],
            base_offset: 0,
            last_offset: 1,
            base_timestamp_ns: 1000,
            last_timestamp_ns: 1001,
        }))
        .await
        .unwrap();

        let durable = drx.recv().await.unwrap();
        assert_eq!(durable.base_offset, 0);

        let raw = get(&store, &manifest_key("", "t", 0)).await.unwrap();
        let m: Manifest = serde_json::from_slice(&raw).unwrap();
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].last_offset, 1);

        drop(tx);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn re_upload_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(&fs_cfg(), dir.path().to_str().unwrap()).unwrap();
        put(
            &store,
            &manifest_key("", "t", 0),
            bytes::Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap()),
        )
        .await
        .unwrap();
        let (tx, rx) = mpsc::channel(4);
        let (dtx, mut drx) = mpsc::channel(4);
        tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, rx, dtx).run());
        let batch = || SealedBatch {
            records: vec![rec(0)],
            base_offset: 0,
            last_offset: 0,
            base_timestamp_ns: 1,
            last_timestamp_ns: 1,
        };
        tx.send(UploaderMsg::Upload(batch())).await.unwrap();
        drx.recv().await.unwrap();
        tx.send(UploaderMsg::Upload(batch())).await.unwrap();
        drx.recv().await.unwrap();
        let m: Manifest =
            serde_json::from_slice(&get(&store, &manifest_key("", "t", 0)).await.unwrap()).unwrap();
        assert_eq!(
            m.segments.len(),
            1,
            "duplicate base_offset must not double-append"
        );
    }
}
