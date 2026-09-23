//! `Produce` handler. Auto-creates the topic when `broker.auto_create_topics`
//! is true, enforces per-topic key/value size limits, slices the wire
//! payload into per-record bytes, and awaits the WAL fsync ack before
//! returning the assigned offsets to the client.
//!
//! Structured as five helpers behind a thin orchestrator:
//! - `produce_error` — one place to emit `PRODUCE_ERRORS` + build an error `Frame`.
//! - `resolve_or_ensure_partition` — auto-create-if-needed then partition lookup.
//! - `validate_record_sizes` — per-topic key/value byte limits.
//! - `slice_payload` — chunk the wire payload into `IncomingRecord`s.
//! - `commit_records` — hand off to the writer, await ack, emit success metrics.

use super::{PartitionHandle, SharedState};
use crate::metrics::{
    partition_label, partition_label_with, LABEL_ERROR_CODE, PRODUCE_BYTES, PRODUCE_ERRORS,
    PRODUCE_LATENCY_MS, PRODUCE_RECORDS,
};
use crate::partition_writer::{IncomingRecord, PwMsg};
use crate::startup::spawn_partition;
use crate::topic_registry::{RegistryError, RegistryMsg};
use crate::wire::errors::make_error;
use crate::wire::frame::Frame;
use bytes::Bytes;
use kafkrs_models::topic::{
    ResolvedTopicConfig, TopicConfigOverrides as TopicConfigOverridesModel,
};
use kafkrs_models::wire::v1::{command::Body, Command, ErrorCode, InRecordMeta, ProduceResponse};
use tokio::sync::oneshot;

pub async fn handle_produce(
    correlation_id: u64,
    state: &SharedState,
    topic: String,
    partition: u32,
    records_meta: Vec<InRecordMeta>,
    payload: Bytes,
) -> Frame {
    // Every helper below returns Result<_, Frame> where Err = a fully-built
    // error frame (metric already bumped by produce_error). `produce_inner`
    // uses `?` for early return; this outer function collapses both arms
    // back to a single Frame, since the caller doesn't distinguish.
    match produce_inner(
        correlation_id,
        state,
        &topic,
        partition,
        records_meta,
        payload,
    )
    .await
    {
        Ok(frame) | Err(frame) => frame,
    }
}

async fn produce_inner(
    correlation_id: u64,
    state: &SharedState,
    topic: &str,
    partition: u32,
    records_meta: Vec<InRecordMeta>,
    payload: Bytes,
) -> Result<Frame, Frame> {
    let start = std::time::Instant::now();

    if records_meta.is_empty() {
        return Err(produce_error(
            topic,
            partition,
            correlation_id,
            ErrorCode::ErrMalformedFrame,
            "produce must contain at least one record",
        ));
    }

    let handle = resolve_or_ensure_partition(state, topic, partition, correlation_id).await?;
    validate_record_sizes(&records_meta, &handle, topic, partition, correlation_id)?;
    let records = slice_payload(records_meta, payload, topic, partition, correlation_id)?;
    Ok(commit_records(&handle, records, topic, partition, correlation_id, start).await)
}

/// Build an error `Frame` for the Produce path AND bump `PRODUCE_ERRORS`
/// with the failing `error_code` as a metric label. Every Produce error
/// path routes through here so the metric-and-frame pairing is a single
/// call site.
fn produce_error(
    topic: &str,
    partition: u32,
    correlation_id: u64,
    code: ErrorCode,
    message: impl Into<String>,
) -> Frame {
    metrics::counter!(
        PRODUCE_ERRORS,
        &partition_label_with(
            topic,
            partition,
            &[(LABEL_ERROR_CODE, format!("{}", code as i32))],
        )
    )
    .increment(1);
    Frame {
        command: make_error(correlation_id, code, message),
        payload: Bytes::new(),
    }
}

/// Return a live `PartitionHandle` for `(topic, partition)`, auto-creating
/// the topic first if `state.auto_create` is set. On any failure returns
/// a fully-built error `Frame` (via `produce_error`) so the caller can
/// short-circuit with `?`.
async fn resolve_or_ensure_partition(
    state: &SharedState,
    topic: &str,
    partition: u32,
    correlation_id: u64,
) -> Result<PartitionHandle, Frame> {
    if state.auto_create {
        let (r, rr) = oneshot::channel::<Result<String, RegistryError>>();
        if state
            .registry
            .send(RegistryMsg::EnsureExists {
                name: topic.to_string(),
                partition_count: state.default_partition_count,
                reply: r,
            })
            .await
            .is_err()
        {
            return Err(produce_error(
                topic,
                partition,
                correlation_id,
                ErrorCode::ErrBrokerNotReady,
                "",
            ));
        }
        match rr.await {
            Ok(Ok(uuid)) => {
                // Newly created: spawn partition workers so they are present in
                // state.partitions before the produce handle-lookup below.
                let cfg = ResolvedTopicConfig::resolve(
                    &TopicConfigOverridesModel::default(),
                    state.disk_type.clone(),
                );
                for p in 0..state.default_partition_count {
                    spawn_partition(
                        &state.data_dir,
                        topic,
                        uuid.clone(),
                        p,
                        cfg,
                        state.store.clone(),
                        state.prefix.clone(),
                        state.partitions.clone(),
                        state.spawn_locks.clone(),
                    )
                    .await;
                }
            }
            Ok(Err(RegistryError::AlreadyExists)) => {
                // Topic existed before this produce, so its partition workers were
                // spawned by a prior CreateTopic or EnsureExists call. Nothing to do.
            }
            Ok(Err(RegistryError::UnknownTopic)) => {
                // EnsureExists never returns UnknownTopic (that variant is only
                // produced by Delete); unreachable in practice, but the shared
                // RegistryError type requires this arm to be exhaustive.
                return Err(produce_error(
                    topic,
                    partition,
                    correlation_id,
                    ErrorCode::ErrInternal,
                    "auto-create failed: unexpected UnknownTopic",
                ));
            }
            Ok(Err(RegistryError::InvalidConfig(_))) => {
                // EnsureExists always passes TopicConfigOverrides::default(),
                // which is always valid; unreachable in practice, but the
                // shared RegistryError type requires this arm to be exhaustive.
                return Err(produce_error(
                    topic,
                    partition,
                    correlation_id,
                    ErrorCode::ErrInternal,
                    "auto-create failed: unexpected InvalidConfig",
                ));
            }
            Ok(Err(RegistryError::Io(msg))) => {
                return Err(produce_error(
                    topic,
                    partition,
                    correlation_id,
                    ErrorCode::ErrInternal,
                    format!("auto-create failed: {msg}"),
                ));
            }
            Err(_) => {
                return Err(produce_error(
                    topic,
                    partition,
                    correlation_id,
                    ErrorCode::ErrBrokerNotReady,
                    "",
                ));
            }
        }
    }

    let handle = {
        let guard = state.partitions.read().await;
        guard.get(&(topic.to_string(), partition)).cloned()
    };
    handle.ok_or_else(|| {
        produce_error(
            topic,
            partition,
            correlation_id,
            ErrorCode::ErrUnknownTopic,
            "",
        )
    })
}

/// Enforce per-record key/value byte limits against the topic's resolved
/// config. Returns the first violation as a `Frame`; `Ok(())` if every
/// record is within limits.
///
/// `#[allow(clippy::result_large_err)]`: `Frame` is intentionally large
/// (it carries a protobuf `Command`); boxing it would force
/// `.map_err(Box::new)` at every `?` in this module without any real
/// memory win — this helper is called exactly once per Produce RPC.
#[allow(clippy::result_large_err)]
fn validate_record_sizes(
    records_meta: &[InRecordMeta],
    handle: &PartitionHandle,
    topic: &str,
    partition: u32,
    correlation_id: u64,
) -> Result<(), Frame> {
    for m in records_meta {
        if m.key_len > handle.cfg.max_key_size_bytes {
            return Err(produce_error(
                topic,
                partition,
                correlation_id,
                ErrorCode::ErrKeyTooLarge,
                format!(
                    "key {} bytes exceeds topic limit {} bytes",
                    m.key_len, handle.cfg.max_key_size_bytes,
                ),
            ));
        }
        if m.value_len > handle.cfg.max_value_size_bytes {
            return Err(produce_error(
                topic,
                partition,
                correlation_id,
                ErrorCode::ErrRecordTooLarge,
                format!(
                    "value {} bytes exceeds topic limit {} bytes",
                    m.value_len, handle.cfg.max_value_size_bytes,
                ),
            ));
        }
    }
    Ok(())
}

/// Chunk `payload` into `IncomingRecord`s using the declared `(key_len,
/// value_len)` pairs in `records_meta`. Malformed frame — payload shorter
/// or longer than the declared sizes — returns `ErrMalformedFrame`.
///
/// `#[allow(clippy::result_large_err)]`: same rationale as
/// `validate_record_sizes` — one call site per Produce RPC, `Frame` is
/// intentionally large.
#[allow(clippy::result_large_err)]
fn slice_payload(
    records_meta: Vec<InRecordMeta>,
    payload: Bytes,
    topic: &str,
    partition: u32,
    correlation_id: u64,
) -> Result<Vec<IncomingRecord>, Frame> {
    let mut records = Vec::with_capacity(records_meta.len());
    let mut cursor = 0usize;
    for m in &records_meta {
        let kl = m.key_len as usize;
        let vl = m.value_len as usize;
        if cursor + kl + vl > payload.len() {
            return Err(produce_error(
                topic,
                partition,
                correlation_id,
                ErrorCode::ErrMalformedFrame,
                "produce payload shorter than declared record sizes",
            ));
        }
        let key = payload.slice(cursor..cursor + kl).to_vec();
        let value = payload.slice(cursor + kl..cursor + kl + vl).to_vec();
        cursor += kl + vl;
        records.push(IncomingRecord {
            schema_id: m.schema_id,
            key,
            value,
            timestamp_ns: m.timestamp_ns,
        });
    }
    if cursor != payload.len() {
        return Err(produce_error(
            topic,
            partition,
            correlation_id,
            ErrorCode::ErrMalformedFrame,
            "produce payload longer than declared record sizes",
        ));
    }
    Ok(records)
}

/// Send the records to the partition writer, await the WAL-fsync ack, and
/// emit success-path metrics. Always returns a `Frame` — either the
/// `ProduceResponse` with assigned offsets, or an `ErrBrokerNotReady` if
/// the writer's channel is gone.
async fn commit_records(
    handle: &PartitionHandle,
    records: Vec<IncomingRecord>,
    topic: &str,
    partition: u32,
    correlation_id: u64,
    start: std::time::Instant,
) -> Frame {
    let request_bytes: u64 = records
        .iter()
        .map(|r| (r.key.len() + r.value.len()) as u64)
        .sum();
    let n = records.len() as i64;
    let (ack, ack_rx) = oneshot::channel::<i64>();
    if handle
        .pw_tx
        .send(PwMsg::Produce { records, ack })
        .await
        .is_err()
    {
        return produce_error(
            topic,
            partition,
            correlation_id,
            ErrorCode::ErrBrokerNotReady,
            "",
        );
    }
    match ack_rx.await {
        Ok(hwm) => {
            let labels = partition_label(topic, partition);
            metrics::counter!(PRODUCE_RECORDS, &labels).increment(n as u64);
            metrics::counter!(PRODUCE_BYTES, &labels).increment(request_bytes);
            metrics::histogram!(PRODUCE_LATENCY_MS, &labels)
                .record(start.elapsed().as_secs_f64() * 1000.0);
            Frame {
                command: Command {
                    correlation_id,
                    body: Some(Body::ProduceResp(ProduceResponse {
                        base_offset: hwm - n + 1,
                        last_offset: hwm,
                        hwm,
                    })),
                },
                payload: Bytes::new(),
            }
        }
        Err(_) => produce_error(
            topic,
            partition,
            correlation_id,
            ErrorCode::ErrBrokerNotReady,
            "",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kafkrs_models::config::DiskType;
    use kafkrs_models::topic::TopicConfigOverrides as ModelOverrides;
    use tokio::sync::broadcast;

    fn test_handle(cfg: ResolvedTopicConfig) -> PartitionHandle {
        let (pw_tx, _pw_rx) = tokio::sync::mpsc::channel(1);
        let (uploader_tx, _u_rx) = tokio::sync::mpsc::channel(1);
        let (tail, _t_rx) = broadcast::channel(1);
        PartitionHandle {
            pw_tx,
            tail,
            cfg,
            uploader_tx,
            uuid: "test-uuid".into(),
        }
    }

    fn cfg_with_limits(max_key: u32, max_value: u32) -> ResolvedTopicConfig {
        ResolvedTopicConfig::resolve(
            &ModelOverrides {
                max_key_size_bytes: Some(max_key),
                max_value_size_bytes: Some(max_value),
                ..Default::default()
            },
            DiskType::Nvme,
        )
    }

    fn err_code_of(frame: &Frame) -> Option<i32> {
        match &frame.command.body {
            Some(Body::Error(e)) => Some(e.code),
            _ => None,
        }
    }

    // ---- validate_record_sizes ----

    #[test]
    fn validate_record_sizes_accepts_within_limits() {
        let handle = test_handle(cfg_with_limits(100, 1000));
        let metas = vec![InRecordMeta {
            key_len: 50,
            value_len: 500,
            schema_id: 0,
            timestamp_ns: 0,
        }];
        assert!(validate_record_sizes(&metas, &handle, "t", 0, 1).is_ok());
    }

    #[test]
    fn validate_record_sizes_rejects_oversized_key() {
        let handle = test_handle(cfg_with_limits(100, 1000));
        let metas = vec![InRecordMeta {
            key_len: 101, // one byte over
            value_len: 500,
            schema_id: 0,
            timestamp_ns: 0,
        }];
        let err = validate_record_sizes(&metas, &handle, "t", 0, 1).unwrap_err();
        assert_eq!(err_code_of(&err), Some(ErrorCode::ErrKeyTooLarge as i32));
    }

    #[test]
    fn validate_record_sizes_rejects_oversized_value() {
        let handle = test_handle(cfg_with_limits(100, 1000));
        let metas = vec![InRecordMeta {
            key_len: 50,
            value_len: 1001, // one byte over
            schema_id: 0,
            timestamp_ns: 0,
        }];
        let err = validate_record_sizes(&metas, &handle, "t", 0, 1).unwrap_err();
        assert_eq!(err_code_of(&err), Some(ErrorCode::ErrRecordTooLarge as i32));
    }

    // ---- slice_payload ----

    #[test]
    fn slice_payload_chunks_two_records_correctly() {
        // Record 1: key_len=3, value_len=5 → "key" + "value" (8 bytes)
        // Record 2: key_len=2, value_len=4 → "ke"  + "valu"  (6 bytes)
        // Total payload = 14 bytes = "keyvaluekevalu"
        let metas = vec![
            InRecordMeta {
                key_len: 3,
                value_len: 5,
                schema_id: 7,
                timestamp_ns: 100,
            },
            InRecordMeta {
                key_len: 2,
                value_len: 4,
                schema_id: 8,
                timestamp_ns: 200,
            },
        ];
        let payload = Bytes::from_static(b"keyvaluekevalu");
        let records = slice_payload(metas, payload, "t", 0, 1).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].key, b"key");
        assert_eq!(records[0].value, b"value");
        assert_eq!(records[0].schema_id, 7);
        assert_eq!(records[0].timestamp_ns, 100);
        assert_eq!(records[1].key, b"ke");
        assert_eq!(records[1].value, b"valu");
        assert_eq!(records[1].schema_id, 8);
        assert_eq!(records[1].timestamp_ns, 200);
    }

    #[test]
    fn slice_payload_handles_single_record() {
        let metas = vec![InRecordMeta {
            key_len: 3,
            value_len: 5,
            schema_id: 42,
            timestamp_ns: 999,
        }];
        let records = slice_payload(metas, Bytes::from_static(b"keyvalue"), "t", 0, 1).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, b"key");
        assert_eq!(records[0].value, b"value");
    }

    #[test]
    fn slice_payload_rejects_payload_shorter_than_declared() {
        let metas = vec![InRecordMeta {
            key_len: 3,
            value_len: 5,
            schema_id: 0,
            timestamp_ns: 0,
        }];
        // Declared 8 bytes total, only 5 provided.
        let err = slice_payload(metas, Bytes::from_static(b"short"), "t", 0, 1).unwrap_err();
        assert_eq!(err_code_of(&err), Some(ErrorCode::ErrMalformedFrame as i32));
    }

    #[test]
    fn slice_payload_rejects_payload_longer_than_declared() {
        let metas = vec![InRecordMeta {
            key_len: 3,
            value_len: 5,
            schema_id: 0,
            timestamp_ns: 0,
        }];
        // Declared 8 bytes total, 13 provided.
        let err = slice_payload(metas, Bytes::from_static(b"keyvalueEXTRA"), "t", 0, 1)
            .unwrap_err();
        assert_eq!(err_code_of(&err), Some(ErrorCode::ErrMalformedFrame as i32));
    }
}
