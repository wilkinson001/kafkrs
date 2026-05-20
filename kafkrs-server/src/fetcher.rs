use crate::object_store::{get, manifest_key, segment_key};
use crate::partition_writer::{LocateResult, PwMsg};
use anyhow::Result;
use bytes::Bytes;
use kafkrs_models::manifest::{Manifest, SegmentEntry};
use kafkrs_models::record::Record;
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{timeout, Duration};

#[derive(Debug, PartialEq)]
pub enum FetchError {
    UnknownTopic,
    UnknownPartition,
    OffsetOutOfRange,
    BrokerNotReady,
}

pub struct FetchRequest {
    pub topic: String,
    pub partition: u32,
    pub from_offset: i64,
    pub max_records: usize,
    pub max_wait_ms: u64,
}

#[derive(Debug)]
pub struct FetchResponse {
    pub records: Vec<Record>,
    pub hwm: i64,
}

/// Resolves one fetch. `pw_tx`/`tail` are the target partition's handles
/// (resolved by the caller from the topic registry; None → UnknownTopic/Partition).
pub async fn fetch(
    req: FetchRequest,
    pw_tx: &mpsc::Sender<PwMsg>,
    tail: &broadcast::Sender<i64>,
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> Result<FetchResponse, FetchError> {
    if req.from_offset < 0 {
        return Err(FetchError::OffsetOutOfRange);
    }
    let loc: LocateResult = locate(pw_tx, req.from_offset).await?;
    match loc {
        LocateResult::Hwm(hwm) => {
            if req.max_wait_ms == 0 {
                return Ok(FetchResponse {
                    records: vec![],
                    hwm,
                });
            }
            let mut sub: broadcast::Receiver<i64> = tail.subscribe();
            let _ = timeout(Duration::from_millis(req.max_wait_ms), async {
                loop {
                    match sub.recv().await {
                        Ok(new_hwm) if new_hwm >= req.from_offset => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            })
            .await;
            // re-resolve once after wake
            match locate(pw_tx, req.from_offset).await? {
                LocateResult::InActiveBatch => read_active(pw_tx, &req).await,
                LocateResult::Hwm(h) => Ok(FetchResponse {
                    records: vec![],
                    hwm: h,
                }),
                _ => read_object_store(req, store, prefix).await,
            }
        }
        LocateResult::InActiveBatch => read_active(pw_tx, &req).await,
        LocateResult::InFlight => read_object_store(req, store, prefix).await.or_else(|_| {
            Ok(FetchResponse {
                records: vec![],
                hwm: -1,
            })
        }),
        LocateResult::BelowInFlight => read_object_store(req, store, prefix).await,
    }
}

async fn locate(pw_tx: &mpsc::Sender<PwMsg>, from_offset: i64) -> Result<LocateResult, FetchError> {
    let (tx, rx): (
        oneshot::Sender<LocateResult>,
        oneshot::Receiver<LocateResult>,
    ) = oneshot::channel();
    pw_tx
        .send(PwMsg::Locate {
            from_offset,
            reply: tx,
        })
        .await
        .map_err(|_| FetchError::BrokerNotReady)?;
    rx.await.map_err(|_| FetchError::BrokerNotReady)
}

async fn read_active(
    pw_tx: &mpsc::Sender<PwMsg>,
    req: &FetchRequest,
) -> Result<FetchResponse, FetchError> {
    let (tx, rx): (oneshot::Sender<Vec<Record>>, oneshot::Receiver<Vec<Record>>) =
        oneshot::channel();
    pw_tx
        .send(PwMsg::ReadActive {
            from_offset: req.from_offset,
            max_records: req.max_records,
            reply: tx,
        })
        .await
        .map_err(|_| FetchError::BrokerNotReady)?;
    let records: Vec<Record> = rx.await.map_err(|_| FetchError::BrokerNotReady)?;
    let hwm: i64 = records
        .last()
        .map(|r| r.offset)
        .unwrap_or(req.from_offset - 1);
    Ok(FetchResponse { records, hwm })
}

async fn read_object_store(
    req: FetchRequest,
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> Result<FetchResponse, FetchError> {
    let m_key: ObjPath = manifest_key(prefix, &req.topic, req.partition);
    let raw: Bytes = get(store, &m_key)
        .await
        .map_err(|_| FetchError::UnknownTopic)?;
    let manifest: Manifest =
        serde_json::from_slice(&raw).map_err(|_| FetchError::BrokerNotReady)?;
    let seg: &SegmentEntry = manifest
        .segment_for_offset(req.from_offset)
        .ok_or(FetchError::OffsetOutOfRange)?;
    let key: ObjPath = segment_key(prefix, &req.topic, req.partition, seg.base_offset);
    let bytes: Bytes = get(store, &key)
        .await
        .map_err(|_| FetchError::BrokerNotReady)?;
    let reader: ParquetRecordBatchReader = ParquetRecordBatchReaderBuilder::try_new(bytes)
        .map_err(|_| FetchError::BrokerNotReady)?
        .build()
        .map_err(|_| FetchError::BrokerNotReady)?;
    let mut out: Vec<Record> = Vec::new();
    for batch in reader {
        let batch: arrow_array::RecordBatch = batch.map_err(|_| FetchError::BrokerNotReady)?;
        out.extend(recordbatch_to_records(&batch));
    }
    let hwm: i64 = manifest.last_uploaded_offset().unwrap_or(-1);
    let records: Vec<Record> = out
        .into_iter()
        .filter(|r| r.offset >= req.from_offset)
        .take(req.max_records)
        .collect();
    Ok(FetchResponse { records, hwm })
}

fn recordbatch_to_records(batch: &arrow_array::RecordBatch) -> Vec<Record> {
    use arrow_array::{Array, BinaryArray, Int32Array, Int64Array, TimestampNanosecondArray};
    let off: &Int64Array = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let ts: &TimestampNanosecondArray = batch
        .column(1)
        .as_any()
        .downcast_ref::<TimestampNanosecondArray>()
        .unwrap();
    let key: &BinaryArray = batch
        .column(2)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .unwrap();
    let val: &BinaryArray = batch
        .column(3)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .unwrap();
    let sid: &Int32Array = batch
        .column(4)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    (0..batch.num_rows())
        .map(|i| Record {
            offset: off.value(i),
            timestamp_ns: ts.value(i),
            schema_id: sid.value(i) as u32,
            key: if key.is_null(i) {
                vec![]
            } else {
                key.value(i).to_vec()
            },
            value: val.value(i).to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{build_store, put};
    use crate::segment::write_segment;
    use kafkrs_models::config::ObjectStoreConfig;
    use kafkrs_models::manifest::{Manifest, SegmentEntry};

    #[tokio::test]
    async fn negative_offset_is_out_of_range() {
        // pw_tx with no receiver alive triggers BrokerNotReady on locate, but
        // negative offset is checked first.
        let (tx, _rx) = mpsc::channel(1);
        let (ttx, _t) = broadcast::channel(1);
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(
            &ObjectStoreConfig {
                backend: "filesystem".into(),
                bucket: "b".into(),
                prefix: "".into(),
                endpoint: "".into(),
                region: "us-east-1".into(),
            },
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        let err = fetch(
            FetchRequest {
                topic: "t".into(),
                partition: 0,
                from_offset: -1,
                max_records: 10,
                max_wait_ms: 0,
            },
            &tx,
            &ttx,
            &store,
            "",
        )
        .await
        .unwrap_err();
        assert_eq!(err, FetchError::OffsetOutOfRange);
    }

    #[tokio::test]
    async fn reads_from_object_store_tier() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(
            &ObjectStoreConfig {
                backend: "filesystem".into(),
                bucket: "b".into(),
                prefix: "".into(),
                endpoint: "".into(),
                region: "us-east-1".into(),
            },
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        let recs: Vec<Record> = (0..10)
            .map(|o| Record {
                offset: o,
                timestamp_ns: o,
                schema_id: 0,
                key: vec![],
                value: vec![o as u8],
            })
            .collect();
        let bytes = write_segment(&recs).unwrap();
        let byte_size = bytes.len() as u64;
        put(&store, &segment_key("", "t", 0, 0), bytes)
            .await
            .unwrap();
        let mut m = Manifest::empty("t", 0);
        m.segments.push(SegmentEntry {
            base_offset: 0,
            last_offset: 9,
            base_timestamp_ns: 0,
            last_timestamp_ns: 9,
            record_count: 10,
            byte_size,
            object_key: "segment-00000000000000000000.parquet".into(),
        });
        put(
            &store,
            &manifest_key("", "t", 0),
            bytes::Bytes::from(serde_json::to_vec(&m).unwrap()),
        )
        .await
        .unwrap();

        // Force the object-store path: a pw that answers BelowInFlight.
        let (tx, mut rx) = mpsc::channel(1);
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let PwMsg::Locate { reply, .. } = msg {
                    let _ = reply.send(LocateResult::BelowInFlight);
                }
            }
        });
        let (ttx, _t) = broadcast::channel(1);
        let resp = fetch(
            FetchRequest {
                topic: "t".into(),
                partition: 0,
                from_offset: 5,
                max_records: 100,
                max_wait_ms: 0,
            },
            &tx,
            &ttx,
            &store,
            "",
        )
        .await
        .unwrap();
        assert_eq!(resp.records.first().unwrap().offset, 5);
        assert_eq!(resp.records.len(), 5);
        assert_eq!(resp.hwm, 9);
    }
}
