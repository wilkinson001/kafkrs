//! `Produce` handler. Auto-creates the topic when `broker.auto_create_topics`
//! is true, enforces per-topic key/value size limits, slices the wire
//! payload into per-record bytes, and awaits the WAL fsync ack before
//! returning the assigned offsets to the client.

use super::SharedState;
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
use kafkrs_models::topic::{ResolvedTopicConfig, TopicConfigOverrides as TopicConfigOverridesModel};
use kafkrs_models::wire::v1::{command::Body, Command, ErrorCode, ProduceResponse};
use tokio::sync::oneshot;

pub async fn handle_produce(
    correlation_id: u64,
    state: &SharedState,
    topic: String,
    partition: u32,
    records_meta: Vec<kafkrs_models::wire::v1::InRecordMeta>,
    payload: Bytes,
) -> Frame {
    let __start = std::time::Instant::now();

    if records_meta.is_empty() {
        metrics::counter!(
            PRODUCE_ERRORS,
            &partition_label_with(
                &topic,
                partition,
                &[(
                    LABEL_ERROR_CODE,
                    format!("{}", ErrorCode::ErrMalformedFrame as i32)
                )],
            )
        )
        .increment(1);
        return Frame {
            command: make_error(
                correlation_id,
                ErrorCode::ErrMalformedFrame,
                "produce must contain at least one record",
            ),
            payload: Bytes::new(),
        };
    }

    // Auto-create the topic if configured.
    if state.auto_create {
        let (r, rr) = oneshot::channel::<Result<String, RegistryError>>();
        if state
            .registry
            .send(RegistryMsg::EnsureExists {
                name: topic.clone(),
                partition_count: state.default_partition_count,
                reply: r,
            })
            .await
            .is_err()
        {
            metrics::counter!(
                PRODUCE_ERRORS,
                &partition_label_with(
                    &topic,
                    partition,
                    &[(
                        LABEL_ERROR_CODE,
                        format!("{}", ErrorCode::ErrBrokerNotReady as i32)
                    )],
                )
            )
            .increment(1);
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
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
                        &topic,
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
                metrics::counter!(
                    PRODUCE_ERRORS,
                    &partition_label_with(
                        &topic,
                        partition,
                        &[(
                            LABEL_ERROR_CODE,
                            format!("{}", ErrorCode::ErrInternal as i32)
                        )],
                    )
                )
                .increment(1);
                return Frame {
                    command: make_error(
                        correlation_id,
                        ErrorCode::ErrInternal,
                        "auto-create failed: unexpected UnknownTopic",
                    ),
                    payload: Bytes::new(),
                };
            }
            Ok(Err(RegistryError::InvalidConfig(_))) => {
                // EnsureExists always passes TopicConfigOverrides::default(),
                // which is always valid; unreachable in practice, but the
                // shared RegistryError type requires this arm to be exhaustive.
                metrics::counter!(
                    PRODUCE_ERRORS,
                    &partition_label_with(
                        &topic,
                        partition,
                        &[(
                            LABEL_ERROR_CODE,
                            format!("{}", ErrorCode::ErrInternal as i32)
                        )],
                    )
                )
                .increment(1);
                return Frame {
                    command: make_error(
                        correlation_id,
                        ErrorCode::ErrInternal,
                        "auto-create failed: unexpected InvalidConfig",
                    ),
                    payload: Bytes::new(),
                };
            }
            Ok(Err(RegistryError::Io(msg))) => {
                metrics::counter!(
                    PRODUCE_ERRORS,
                    &partition_label_with(
                        &topic,
                        partition,
                        &[(
                            LABEL_ERROR_CODE,
                            format!("{}", ErrorCode::ErrInternal as i32)
                        )],
                    )
                )
                .increment(1);
                return Frame {
                    command: make_error(
                        correlation_id,
                        ErrorCode::ErrInternal,
                        format!("auto-create failed: {msg}"),
                    ),
                    payload: Bytes::new(),
                };
            }
            Err(_) => {
                metrics::counter!(
                    PRODUCE_ERRORS,
                    &partition_label_with(
                        &topic,
                        partition,
                        &[(
                            LABEL_ERROR_CODE,
                            format!("{}", ErrorCode::ErrBrokerNotReady as i32)
                        )],
                    )
                )
                .increment(1);
                return Frame {
                    command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                    payload: Bytes::new(),
                };
            }
        }
    }

    // Resolve the partition handle — earlier than before, so the size check can read its cfg.
    let handle = {
        let guard = state.partitions.read().await;
        guard.get(&(topic.clone(), partition)).cloned()
    };
    let Some(handle) = handle else {
        metrics::counter!(
            PRODUCE_ERRORS,
            &partition_label_with(
                &topic,
                partition,
                &[(
                    LABEL_ERROR_CODE,
                    format!("{}", ErrorCode::ErrUnknownTopic as i32)
                )],
            )
        )
        .increment(1);
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
            payload: Bytes::new(),
        };
    };

    // Per-record size check against the resolved per-topic limits.
    for m in &records_meta {
        if m.key_len > handle.cfg.max_key_size_bytes {
            metrics::counter!(
                PRODUCE_ERRORS,
                &partition_label_with(
                    &topic,
                    partition,
                    &[(
                        LABEL_ERROR_CODE,
                        format!("{}", ErrorCode::ErrKeyTooLarge as i32)
                    )],
                )
            )
            .increment(1);
            return Frame {
                command: make_error(
                    correlation_id,
                    ErrorCode::ErrKeyTooLarge,
                    format!(
                        "key {} bytes exceeds topic limit {} bytes",
                        m.key_len, handle.cfg.max_key_size_bytes,
                    ),
                ),
                payload: Bytes::new(),
            };
        }
        if m.value_len > handle.cfg.max_value_size_bytes {
            metrics::counter!(
                PRODUCE_ERRORS,
                &partition_label_with(
                    &topic,
                    partition,
                    &[(
                        LABEL_ERROR_CODE,
                        format!("{}", ErrorCode::ErrRecordTooLarge as i32)
                    )],
                )
            )
            .increment(1);
            return Frame {
                command: make_error(
                    correlation_id,
                    ErrorCode::ErrRecordTooLarge,
                    format!(
                        "value {} bytes exceeds topic limit {} bytes",
                        m.value_len, handle.cfg.max_value_size_bytes,
                    ),
                ),
                payload: Bytes::new(),
            };
        }
    }

    // Slice payload into per-record (key, value) pairs using the metas.
    let mut records = Vec::with_capacity(records_meta.len());
    let mut cursor = 0usize;
    for m in &records_meta {
        let kl = m.key_len as usize;
        let vl = m.value_len as usize;
        if cursor + kl + vl > payload.len() {
            metrics::counter!(
                PRODUCE_ERRORS,
                &partition_label_with(
                    &topic,
                    partition,
                    &[(
                        LABEL_ERROR_CODE,
                        format!("{}", ErrorCode::ErrMalformedFrame as i32)
                    )],
                )
            )
            .increment(1);
            return Frame {
                command: make_error(
                    correlation_id,
                    ErrorCode::ErrMalformedFrame,
                    "produce payload shorter than declared record sizes",
                ),
                payload: Bytes::new(),
            };
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
        metrics::counter!(
            PRODUCE_ERRORS,
            &partition_label_with(
                &topic,
                partition,
                &[(
                    LABEL_ERROR_CODE,
                    format!("{}", ErrorCode::ErrMalformedFrame as i32)
                )],
            )
        )
        .increment(1);
        return Frame {
            command: make_error(
                correlation_id,
                ErrorCode::ErrMalformedFrame,
                "produce payload longer than declared record sizes",
            ),
            payload: Bytes::new(),
        };
    }

    let __topic = topic.clone();
    let __bytes: u64 = records_meta
        .iter()
        .map(|m| (m.key_len + m.value_len) as u64)
        .sum();
    let n = records.len() as i64;
    let (ack, ack_rx) = oneshot::channel::<i64>();
    if handle
        .pw_tx
        .send(PwMsg::Produce { records, ack })
        .await
        .is_err()
    {
        metrics::counter!(
            PRODUCE_ERRORS,
            &partition_label_with(
                &topic,
                partition,
                &[(
                    LABEL_ERROR_CODE,
                    format!("{}", ErrorCode::ErrBrokerNotReady as i32)
                )],
            )
        )
        .increment(1);
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    match ack_rx.await {
        Ok(hwm) => {
            let __labels = partition_label(&__topic, partition);
            metrics::counter!(PRODUCE_RECORDS, &__labels).increment(n as u64);
            metrics::counter!(PRODUCE_BYTES, &__labels).increment(__bytes);
            metrics::histogram!(PRODUCE_LATENCY_MS, &__labels)
                .record(__start.elapsed().as_secs_f64() * 1000.0);
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
        Err(_) => {
            metrics::counter!(
                PRODUCE_ERRORS,
                &partition_label_with(
                    &topic,
                    partition,
                    &[(
                        LABEL_ERROR_CODE,
                        format!("{}", ErrorCode::ErrBrokerNotReady as i32)
                    )],
                )
            )
            .increment(1);
            Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            }
        }
    }
}
