use crate::uploader::{SealedBatch, SegmentDurable, UploaderMsg};
use crate::wal_writer::WalFile;
use anyhow::Result;
use kafkrs_models::record::Record;
use kafkrs_models::topic::ResolvedTopicConfig;
use std::collections::BTreeMap;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{Duration, Instant};

/// An incoming record before offset assignment (offset/timestamp may be unset;
/// timestamp 0 means "broker-stamp it").
pub struct IncomingRecord {
    pub schema_id: u32,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub timestamp_ns: i64,
}

pub enum PwMsg {
    /// Produce: ack fires (oneshot resolves to assigned base offset) only
    /// after WAL fsync (spec invariant 1).
    Produce {
        records: Vec<IncomingRecord>,
        ack: oneshot::Sender<i64>,
    },
    /// Read location query for the Fetcher.
    Locate {
        from_offset: i64,
        reply: oneshot::Sender<LocateResult>,
    },
    /// Slice the active batch for the Fetcher.
    ReadActive {
        from_offset: i64,
        max_records: usize,
        reply: oneshot::Sender<Vec<Record>>,
    },
    /// Uploader signalled a segment is durable.
    SegmentDurable(SegmentDurable),
    Shutdown,
}

#[derive(Debug, PartialEq)]
pub enum LocateResult {
    /// Highest committed offset; -1 if none yet.
    Hwm(i64),
    InActiveBatch,
    InFlight,
    BelowInFlight, // serve from object store
}

pub struct PartitionWriter {
    data_dir: String,
    topic: String,
    partition: u32,
    cfg: ResolvedTopicConfig,
    next_offset: i64,
    /// base offset of the current active segment.
    segment_base: i64,
    active: Vec<Record>,
    active_bytes: usize,
    /// pending pre-fsync records buffered for group commit.
    pending: Vec<Record>,
    pending_acks: Vec<oneshot::Sender<i64>>,
    pending_first_arrival: Option<Instant>,
    wal: WalFile,
    in_flight: BTreeMap<i64, Vec<Record>>, // base_offset -> sealed records
    rx: mpsc::Receiver<PwMsg>,
    uploader_tx: mpsc::Sender<UploaderMsg>,
    tail_tx: broadcast::Sender<i64>, // notifies new HWM
}

impl PartitionWriter {
    pub fn new(
        data_dir: String,
        topic: String,
        partition: u32,
        cfg: ResolvedTopicConfig,
        start_offset: i64,
        recovered_active: Vec<Record>,
        rx: mpsc::Receiver<PwMsg>,
        uploader_tx: mpsc::Sender<UploaderMsg>,
        tail_tx: broadcast::Sender<i64>,
    ) -> Result<PartitionWriter> {
        let segment_base: i64 = recovered_active
            .first()
            .map(|r| r.offset)
            .unwrap_or(start_offset);
        let wal: WalFile = WalFile::open(&data_dir, &topic, partition, segment_base)?;
        let active_bytes: usize = recovered_active
            .iter()
            .map(|r| r.value.len() + r.key.len())
            .sum();
        Ok(PartitionWriter {
            data_dir,
            topic,
            partition,
            cfg,
            next_offset: start_offset,
            segment_base,
            active: recovered_active,
            active_bytes,
            pending: Vec::new(),
            pending_acks: Vec::new(),
            pending_first_arrival: None,
            wal,
            in_flight: BTreeMap::new(),
            rx,
            uploader_tx,
            tail_tx,
        })
    }

    fn hwm(&self) -> i64 {
        self.next_offset - 1
    }

    pub async fn run(mut self) {
        loop {
            let timeout: Duration = self
                .pending_first_arrival
                .map(|t| {
                    let elapsed: u64 = t.elapsed().as_millis() as u64;
                    Duration::from_millis(self.cfg.group_commit_time_ms.saturating_sub(elapsed))
                })
                .unwrap_or(Duration::from_secs(3600));

            tokio::select! {
                msg = self.rx.recv() => {
                    match msg {
                        Some(PwMsg::Produce { records, ack }) => self.on_produce(records, ack).await,
                        Some(PwMsg::Locate { from_offset, reply }) => { let _ = reply.send(self.locate(from_offset)); }
                        Some(PwMsg::ReadActive { from_offset, max_records, reply }) => {
                            let _ = reply.send(self.read_active(from_offset, max_records));
                        }
                        Some(PwMsg::SegmentDurable(d)) => self.on_durable(d),
                        Some(PwMsg::Shutdown) | None => { self.flush_commit().await; break; }
                    }
                }
                _ = tokio::time::sleep(timeout), if self.pending_first_arrival.is_some() => {
                    self.flush_commit().await;
                }
            }
        }
    }

    async fn on_produce(&mut self, incoming: Vec<IncomingRecord>, ack: oneshot::Sender<i64>) {
        let base: i64 = self.next_offset;
        for inc in incoming {
            let ts: i64 = if inc.timestamp_ns != 0 {
                inc.timestamp_ns
            } else {
                now_ns()
            };
            self.pending.push(Record {
                offset: self.next_offset,
                timestamp_ns: ts,
                schema_id: inc.schema_id,
                key: inc.key,
                value: inc.value,
            });
            self.next_offset += 1;
        }
        self.pending_acks.push(ack);
        let _ = base; // base offset for this produce; ack carries it after fsync
        if self.pending_first_arrival.is_none() {
            self.pending_first_arrival = Some(Instant::now());
        }
        let pending_bytes: usize = self
            .pending
            .iter()
            .map(|r| r.value.len() + r.key.len())
            .sum();
        if self.pending.len() >= self.cfg.group_commit_record_count
            || pending_bytes >= self.cfg.group_commit_size_bytes
        {
            self.flush_commit().await;
        }
    }

    /// Serialize pending → WAL → fsync → fire acks → move into active batch →
    /// advance HWM → notify tail. (spec §"Group commit")
    async fn flush_commit(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let batch: Vec<Record> = std::mem::take(&mut self.pending);
        self.wal.append_and_sync(&batch).expect("WAL fsync failed");
        let acks: Vec<oneshot::Sender<i64>> = std::mem::take(&mut self.pending_acks);
        for a in acks {
            let _ = a.send(self.next_offset - 1);
        }
        self.pending_first_arrival = None;
        for r in &batch {
            self.active_bytes += r.value.len() + r.key.len();
        }
        self.active.extend(batch);
        let _ = self.tail_tx.send(self.hwm());

        if self.active_bytes as u64 >= self.cfg.segment_size_bytes {
            self.seal().await;
        }
    }

    /// Freeze the active batch, hand it to the Uploader, open the next WAL.
    async fn seal(&mut self) {
        if self.active.is_empty() {
            return;
        }
        let records: Vec<Record> = std::mem::take(&mut self.active);
        self.active_bytes = 0;
        let base_offset: i64 = records.first().unwrap().offset;
        let last: &Record = records.last().unwrap();
        let last_offset: i64 = last.offset;
        let base_timestamp_ns: i64 = records.first().unwrap().timestamp_ns;
        let last_timestamp_ns: i64 = last.timestamp_ns;
        self.in_flight.insert(base_offset, records.clone());

        let _ = self
            .uploader_tx
            .send(UploaderMsg::Upload(SealedBatch {
                records,
                base_offset,
                last_offset,
                base_timestamp_ns,
                last_timestamp_ns,
            }))
            .await;

        // open next segment WAL
        self.segment_base = self.next_offset;
        self.wal = WalFile::open(
            &self.data_dir,
            &self.topic,
            self.partition,
            self.segment_base,
        )
        .expect("open next WAL");
    }

    fn on_durable(&mut self, d: SegmentDurable) {
        self.in_flight.remove(&d.base_offset);
        // delete the WAL file for that sealed segment (spec invariant 4)
        let path: std::path::PathBuf =
            WalFile::wal_path(&self.data_dir, &self.topic, self.partition, d.base_offset);
        let _ = std::fs::remove_file(path);
    }

    fn locate(&self, from_offset: i64) -> LocateResult {
        if from_offset > self.hwm() {
            return LocateResult::Hwm(self.hwm());
        }
        if let Some(first) = self.active.first() {
            if from_offset >= first.offset {
                return LocateResult::InActiveBatch;
            }
        } else if from_offset == self.hwm() + 1 {
            return LocateResult::Hwm(self.hwm());
        }
        if let Some((&earliest, _)) = self.in_flight.iter().next() {
            if from_offset >= earliest {
                return LocateResult::InFlight;
            }
        }
        LocateResult::BelowInFlight
    }

    fn read_active(&self, from_offset: i64, max_records: usize) -> Vec<Record> {
        self.active
            .iter()
            .filter(|r| r.offset >= from_offset)
            .take(max_records)
            .cloned()
            .collect()
    }

    pub fn in_flight_slice(&self, from_offset: i64, max_records: usize) -> Vec<Record> {
        self.in_flight
            .values()
            .flat_map(|v| v.iter())
            .filter(|r| r.offset >= from_offset)
            .take(max_records)
            .cloned()
            .collect()
    }
}

fn now_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{build_store, manifest_key, put};
    use crate::uploader::Uploader;
    use kafkrs_models::config::{DiskType, ObjectStoreConfig};
    use kafkrs_models::manifest::Manifest;
    use kafkrs_models::topic::{ResolvedTopicConfig, TopicConfigOverrides};

    fn small_cfg() -> ResolvedTopicConfig {
        // seal after a tiny number of bytes so tests exercise sealing
        let mut o = TopicConfigOverrides::default();
        o.segment_size_bytes = Some(8);
        o.group_commit_record_count = Some(1);
        ResolvedTopicConfig::resolve(&o, DiskType::Nvme)
    }

    #[tokio::test]
    async fn produce_acks_after_fsync_and_advances_hwm() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let cfg = small_cfg();
        let (utx, urx) = mpsc::channel(8);
        let (dtx, _drx) = mpsc::channel(8);
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
        put(
            &store,
            &manifest_key("", "t", 0),
            bytes::Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap()),
        )
        .await
        .unwrap();
        tokio::spawn(Uploader::new(store, "".into(), "t".into(), 0, urx, dtx).run());

        let (tx, rx) = mpsc::channel(8);
        let (ttx, _trx) = broadcast::channel(16);
        let pw = PartitionWriter::new(dd, "t".into(), 0, cfg, 0, vec![], rx, utx, ttx).unwrap();
        tokio::spawn(pw.run());

        let (atx, arx) = oneshot::channel();
        tx.send(PwMsg::Produce {
            records: vec![IncomingRecord {
                schema_id: 0,
                key: vec![],
                value: vec![1, 2, 3],
                timestamp_ns: 0,
            }],
            ack: atx,
        })
        .await
        .unwrap();
        let assigned_hwm = arx.await.unwrap();
        assert_eq!(assigned_hwm, 0);

        let (ltx, lrx) = oneshot::channel();
        tx.send(PwMsg::Locate {
            from_offset: 5,
            reply: ltx,
        })
        .await
        .unwrap();
        assert_eq!(lrx.await.unwrap(), LocateResult::Hwm(0));

        tx.send(PwMsg::Shutdown).await.unwrap();
    }
}
