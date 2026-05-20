use anyhow::Result;
use kafkrs_models::record::Record;
use kafkrs_models::wal::{encode_record, scan_wal};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Owns one append-only WAL file (one per segment). The PartitionWriter is the
/// sole writer (spec invariant 3).
pub struct WalFile {
    path: PathBuf,
    file: File,
}

impl WalFile {
    pub fn wal_path(data_dir: &str, topic: &str, partition: u32, base_offset: i64) -> PathBuf {
        Path::new(data_dir)
            .join("wal")
            .join(topic)
            .join(partition.to_string())
            .join(format!("{base_offset}.wal"))
    }

    /// Opens (creating parent dirs) the WAL file for a segment, append mode.
    pub fn open(data_dir: &str, topic: &str, partition: u32, base_offset: i64) -> Result<WalFile> {
        let path: PathBuf = Self::wal_path(data_dir, topic, partition, base_offset);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let file: File = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        Ok(WalFile { path, file })
    }

    /// Group commit: encode every record, one `write_all`, then `fsync`.
    /// Returns only after the data is durable (spec §"Group commit").
    pub fn append_and_sync(&mut self, records: &[Record]) -> Result<()> {
        let mut buf: Vec<u8> = Vec::new();
        for r in records {
            encode_record(r, &mut buf);
        }
        self.file.write_all(&buf)?;
        self.file.sync_all()?;
        Ok(())
    }

    pub fn delete(self) -> Result<()> {
        drop(self.file);
        std::fs::remove_file(&self.path)?;
        Ok(())
    }
}

/// Recovery: scan a WAL file, validate, truncate the file at the first invalid
/// record (spec §"Recovery on startup" step 2). Returns recovered records.
pub fn recover_wal_file(path: &Path) -> Result<Vec<Record>> {
    let bytes: Vec<u8> = std::fs::read(path)?;
    let (records, valid_len): (Vec<Record>, usize) = scan_wal(&bytes);
    if valid_len < bytes.len() as usize {
        let f: File = OpenOptions::new().write(true).open(path)?;
        f.set_len(valid_len as u64)?;
        f.sync_all()?;
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(o: i64) -> Record {
        Record {
            offset: o,
            timestamp_ns: 1_000 + o,
            schema_id: 0,
            key: vec![],
            value: vec![o as u8],
        }
    }

    #[test]
    fn append_then_recover_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        let mut w = WalFile::open(dd, "t", 0, 0).unwrap();
        w.append_and_sync(&[rec(0), rec(1)]).unwrap();
        w.append_and_sync(&[rec(2)]).unwrap();
        let path = WalFile::wal_path(dd, "t", 0, 0);
        let recovered = recover_wal_file(&path).unwrap();
        assert_eq!(
            recovered.iter().map(|r| r.offset).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn torn_tail_is_truncated_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        let mut w = WalFile::open(dd, "t", 0, 0).unwrap();
        w.append_and_sync(&[rec(0), rec(1)]).unwrap();
        let path = WalFile::wal_path(dd, "t", 0, 0);
        // simulate torn write: append garbage tail
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0xFF; 7]).unwrap();
        }
        let recovered = recover_wal_file(&path).unwrap();
        assert_eq!(recovered.len(), 2);
        // file truncated back to the 2-record length
        let again = recover_wal_file(&path).unwrap();
        assert_eq!(again.len(), 2);
    }
}
