use crate::object_store::{get, manifest_key};
use crate::wal_writer::recover_wal_file;
use anyhow::Result;
use bytes::Bytes;
use kafkrs_models::manifest::Manifest;
use kafkrs_models::record::Record;
use object_store::ObjectStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Result of recovering one partition.
pub struct PartitionRecovery {
    /// Records that belong to the (not-yet-uploaded) active segment, in order.
    pub active_records: Vec<Record>,
    /// Sealed-but-not-uploaded segments to re-queue (base_offset → records).
    pub orphan_segments: Vec<(i64, Vec<Record>)>,
    /// Next offset to assign.
    pub next_offset: i64,
}

pub async fn recover_partition(
    data_dir: &str,
    topic: &str,
    partition: u32,
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> Result<PartitionRecovery> {
    let wal_dir: PathBuf = Path::new(data_dir)
        .join("wal")
        .join(topic)
        .join(partition.to_string());
    let mut wal_bases: Vec<i64> = Vec::new();
    if wal_dir.exists() {
        for entry in std::fs::read_dir(&wal_dir)? {
            let entry: std::fs::DirEntry = entry?;
            let name: String = entry.file_name().to_string_lossy().to_string();
            if let Some(base) = name
                .strip_suffix(".wal")
                .and_then(|s| s.parse::<i64>().ok())
            {
                wal_bases.push(base);
            }
        }
    }
    wal_bases.sort();

    // single GET of the manifest (spec: no object-store LIST).
    let raw: Bytes = get(store, &manifest_key(prefix, topic, partition)).await?;
    let manifest: Manifest = serde_json::from_slice(&raw)?;
    let last_uploaded: Option<i64> = manifest.last_uploaded_offset();

    let mut active_records: Vec<Record> = Vec::new();
    let mut orphan_segments: Vec<(i64, Vec<Record>)> = Vec::new();
    let mut next_offset: i64 = last_uploaded.map(|o| o + 1).unwrap_or(0);

    for base in wal_bases {
        let path: PathBuf = wal_dir.join(format!("{base}.wal"));
        let covered: bool = last_uploaded.map(|lu| base <= lu).unwrap_or(false)
            && manifest.segments.iter().any(|s| s.base_offset == base);
        if covered {
            // crash between manifest update and WAL delete → clean up
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let records: Vec<Record> = recover_wal_file(&path)?;
        if records.is_empty() {
            continue;
        }
        if let Some(last) = records.last() {
            next_offset = next_offset.max(last.offset + 1);
        }
        // Highest-base WAL is the active segment; earlier ones are orphan
        // sealed segments that never uploaded.
        orphan_segments.push((base, records));
    }
    // The orphan with the largest base is the active segment.
    if let Some((_, recs)) = orphan_segments.pop() {
        active_records = recs;
    }

    Ok(PartitionRecovery {
        active_records,
        orphan_segments,
        next_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{build_store, put};
    use crate::wal_writer::WalFile;
    use kafkrs_models::config::ObjectStoreConfig;
    use kafkrs_models::manifest::{Manifest, SegmentEntry};

    fn rec(o: i64) -> Record {
        Record {
            offset: o,
            timestamp_ns: o,
            schema_id: 0,
            key: vec![],
            value: vec![o as u8],
        }
    }

    #[tokio::test]
    async fn replays_wal_above_last_uploaded() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let store = build_store(
            &ObjectStoreConfig {
                backend: "filesystem".into(),
                bucket: "b".into(),
                prefix: "".into(),
                endpoint: "".into(),
                region: "us-east-1".into(),
            },
            &dd,
        )
        .unwrap();
        // uploaded segment 0..=4
        let mut m = Manifest::empty("t", 0);
        m.segments.push(SegmentEntry {
            base_offset: 0,
            last_offset: 4,
            base_timestamp_ns: 0,
            last_timestamp_ns: 4,
            record_count: 5,
            byte_size: 1,
            object_key: "segment-00000000000000000000.parquet".into(),
        });
        put(
            &store,
            &manifest_key("", "t", 0),
            bytes::Bytes::from(serde_json::to_vec(&m).unwrap()),
        )
        .await
        .unwrap();
        // covered WAL 0 + active WAL 5
        let mut w0 = WalFile::open(&dd, "t", 0, 0).unwrap();
        w0.append_and_sync(&[rec(0), rec(1)]).unwrap();
        let mut w5 = WalFile::open(&dd, "t", 0, 5).unwrap();
        w5.append_and_sync(&[rec(5), rec(6)]).unwrap();

        let r = recover_partition(&dd, "t", 0, &store, "").await.unwrap();
        assert_eq!(
            r.active_records
                .iter()
                .map(|x| x.offset)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert_eq!(r.next_offset, 7);
        // covered WAL 0 deleted
        assert!(!WalFile::wal_path(&dd, "t", 0, 0).exists());
    }
}
