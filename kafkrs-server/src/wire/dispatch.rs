//! Per-RPC handlers. Each handler turns an inbound Command into an outbound
//! response Command. Handlers are pure functions of (state, request) →
//! response and contain no connection-level concerns (no socket I/O,
//! no Connect-state tracking).

use crate::fetcher::{fetch, FetchRequest};
use crate::metrics::{
    partition_label, partition_label_with, FETCH_BYTES, FETCH_ERRORS, FETCH_LATENCY_MS,
    FETCH_RECORDS, FETCH_REQUESTS, LABEL_ERROR_CODE, PRODUCE_BYTES, PRODUCE_ERRORS,
    PRODUCE_LATENCY_MS, PRODUCE_RECORDS,
};
use crate::partition_writer::{IncomingRecord, PwMsg};
use crate::startup::spawn_partition;
use crate::topic_registry::{RegistryError, RegistryMsg};
use crate::wire::errors::{fetch_error_code, make_error, registry_error_code};
use crate::wire::frame::Frame;
use bytes::Bytes;
use kafkrs_models::config::DiskType;
use kafkrs_models::topic::{
    ResolvedTopicConfig, TopicConfigOverrides as TopicConfigOverridesModel,
};
use kafkrs_models::wire::v1::{
    command::Body, Command, ConnectedResponse, CreateTopicResponse, DeleteTopicResponse,
    DescribeTopicResponse, ErrorCode, FetchResponse, ListTopicsResponse, OutRecordMeta,
    PongResponse, ProduceResponse, TopicConfigOverrides,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex as TokioMutex, RwLock};

/// Per-(topic, partition) locks coordinating concurrent `spawn_partition`
/// calls. Outer std::sync::Mutex guards the map (held briefly for entry
/// lookup/insert, never across await); per-key tokio::sync::Mutex is held
/// across the full spawn body (which awaits on recovery and channel setup).
pub type PartitionSpawnLocks = Arc<StdMutex<HashMap<(String, u32), Arc<TokioMutex<()>>>>>;

pub const PROTOCOL_VERSION: u32 = 1;
pub const BROKER_ID: &str = "kafkrs-broker-v1";

/// Handle to a partition's actor: an mpsc sender for the PartitionWriter,
/// a broadcast sender for tail subscribers, the resolved per-topic config
/// (for wire-layer limit enforcement), an mpsc sender for the Uploader
/// (used by the RetentionSweeper to enqueue kicks), and the topic's UUID
/// (isolates topic incarnations across a Delete + re-Create of the same
/// topic name so segment/manifest keys never collide).
#[derive(Clone)]
pub struct PartitionHandle {
    pub pw_tx: mpsc::Sender<PwMsg>,
    pub tail: broadcast::Sender<i64>,
    pub cfg: ResolvedTopicConfig,
    pub uploader_tx: mpsc::Sender<crate::uploader::UploaderMsg>,
    pub uuid: String,
}

/// Shared state available to every per-connection task.
#[derive(Clone)]
pub struct SharedState {
    pub partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
    pub registry: mpsc::Sender<RegistryMsg>,
    pub store: Arc<dyn object_store::ObjectStore>,
    pub prefix: String,
    pub auto_create: bool,
    pub default_partition_count: u32,
    /// Needed by the auto-create path to spawn partition workers.
    pub data_dir: String,
    pub disk_type: DiskType,
    pub spawn_locks: PartitionSpawnLocks,
}

// ---- Per-RPC handlers ----

pub fn handle_ping(correlation_id: u64) -> Frame {
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::Pong(PongResponse {})),
        },
        payload: Bytes::new(),
    }
}

pub fn handle_connected(correlation_id: u64) -> Frame {
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::Connected(ConnectedResponse {
                protocol_version: PROTOCOL_VERSION,
                broker_id: BROKER_ID.to_string(),
            })),
        },
        payload: Bytes::new(),
    }
}

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

pub async fn handle_fetch(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::FetchRequest,
) -> Frame {
    let __start = std::time::Instant::now();
    let __topic = req.topic.clone();
    let __partition = req.partition;
    metrics::counter!(FETCH_REQUESTS, &partition_label(&__topic, __partition)).increment(1);

    let handle = {
        let guard = state.partitions.read().await;
        guard.get(&(req.topic.clone(), req.partition)).cloned()
    };
    let Some(handle) = handle else {
        metrics::counter!(
            FETCH_ERRORS,
            &partition_label_with(
                &__topic,
                __partition,
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
    let effective_wait = (req.max_wait_ms as u64).min(handle.cfg.max_fetch_wait_ms);
    let result = fetch(
        FetchRequest {
            topic: req.topic,
            topic_uuid: handle.uuid.clone(),
            partition: req.partition,
            from_offset: req.from_offset,
            max_records: req.max_records as usize,
            max_wait_ms: effective_wait,
        },
        &handle.pw_tx,
        &handle.tail,
        &state.store,
        &state.prefix,
    )
    .await;
    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            let err_code = fetch_error_code(&e);
            metrics::counter!(
                FETCH_ERRORS,
                &partition_label_with(
                    &__topic,
                    __partition,
                    &[(LABEL_ERROR_CODE, format!("{}", err_code as i32))],
                )
            )
            .increment(1);
            return Frame {
                command: make_error(correlation_id, err_code, ""),
                payload: Bytes::new(),
            };
        }
    };
    // Build payload + metas.
    let mut payload = bytes::BytesMut::new();
    let mut metas = Vec::with_capacity(resp.records.len());
    for r in &resp.records {
        metas.push(OutRecordMeta {
            offset: r.offset,
            timestamp_ns: r.timestamp_ns,
            schema_id: r.schema_id,
            key_len: r.key.len() as u32,
            value_len: r.value.len() as u32,
        });
        payload.extend_from_slice(&r.key);
        payload.extend_from_slice(&r.value);
    }
    let returned_records_count = resp.records.len() as u64;
    let returned_bytes: u64 = resp
        .records
        .iter()
        .map(|r| (r.key.len() + r.value.len()) as u64)
        .sum();
    let __labels = partition_label(&__topic, __partition);
    metrics::counter!(FETCH_RECORDS, &__labels).increment(returned_records_count);
    metrics::counter!(FETCH_BYTES, &__labels).increment(returned_bytes);
    metrics::histogram!(FETCH_LATENCY_MS, &__labels)
        .record(__start.elapsed().as_secs_f64() * 1000.0);
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::FetchResp(FetchResponse {
                records: metas,
                hwm: resp.hwm,
            })),
        },
        payload: payload.freeze(),
    }
}

pub async fn handle_create_topic(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::CreateTopicRequest,
) -> Frame {
    let topic_name = req.topic.clone();
    let partition_count = req.partition_count;
    let overrides = wire_overrides_to_model(req.overrides.unwrap_or_default());
    let resolved_cfg = ResolvedTopicConfig::resolve(&overrides, state.disk_type.clone());

    let (tx, rx) = oneshot::channel::<Result<String, RegistryError>>();
    if state
        .registry
        .send(RegistryMsg::Create {
            name: req.topic,
            partition_count: req.partition_count,
            overrides,
            reply: tx,
        })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    match rx.await {
        Ok(Ok(uuid)) => {
            // Spawn partition workers so subsequent Produce/Fetch RPCs find them.
            for p in 0..partition_count {
                spawn_partition(
                    &state.data_dir,
                    &topic_name,
                    uuid.clone(),
                    p,
                    resolved_cfg,
                    state.store.clone(),
                    state.prefix.clone(),
                    state.partitions.clone(),
                    state.spawn_locks.clone(),
                )
                .await;
            }
            Frame {
                command: Command {
                    correlation_id,
                    body: Some(Body::CreateTopicResp(CreateTopicResponse {})),
                },
                payload: Bytes::new(),
            }
        }
        Ok(Err(e)) => Frame {
            command: make_error(correlation_id, registry_error_code(&e), format!("{e:?}")),
            payload: Bytes::new(),
        },
        Err(_) => Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        },
    }
}

pub async fn handle_describe_topic(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::DescribeTopicRequest,
) -> Frame {
    let (tx, rx) = oneshot::channel();
    if state
        .registry
        .send(RegistryMsg::Describe {
            name: req.topic.clone(),
            reply: tx,
        })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    match rx.await.ok().flatten() {
        Some(entry) => Frame {
            command: Command {
                correlation_id,
                body: Some(Body::DescribeTopicResp(DescribeTopicResponse {
                    topic: entry.name,
                    partition_count: entry.partition_count,
                    created_at_ns: entry.created_at_ns,
                    config: Some(model_overrides_to_wire(entry.config)),
                })),
            },
            payload: Bytes::new(),
        },
        None => Frame {
            command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
            payload: Bytes::new(),
        },
    }
}

pub async fn handle_list_topics(correlation_id: u64, state: &SharedState) -> Frame {
    let (tx, rx) = oneshot::channel();
    if state
        .registry
        .send(RegistryMsg::List { reply: tx })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    let topics = rx.await.unwrap_or_default();
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::ListTopicsResp(ListTopicsResponse { topics })),
        },
        payload: Bytes::new(),
    }
}

pub async fn handle_delete_topic(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::DeleteTopicRequest,
) -> Frame {
    let topic = req.topic.clone();
    let delete_data = req.delete_data.unwrap_or(true);

    // Capture the UUID + partition count BEFORE the registry removes the
    // entry, because we need them to look up partition handles and to
    // construct keys for the manifest snapshot below.
    let describe_reply = {
        let (tx, rx) = oneshot::channel();
        if state
            .registry
            .send(RegistryMsg::Describe {
                name: topic.clone(),
                reply: tx,
            })
            .await
            .is_err()
        {
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
        }
        rx.await.ok().flatten()
    };
    let entry = match describe_reply {
        None => {
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
                payload: Bytes::new(),
            };
        }
        Some(e) => e,
    };
    let topic_uuid = entry.uuid.clone();
    let partition_count = entry.partition_count;

    // Ask the registry to atomically remove + persist. If Describe raced
    // with a concurrent Delete, this call gets UnknownTopic.
    let del_reply = {
        let (tx, rx) = oneshot::channel();
        if state
            .registry
            .send(RegistryMsg::Delete {
                name: topic.clone(),
                delete_data,
                reply: tx,
            })
            .await
            .is_err()
        {
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
        }
        rx.await.ok()
    };
    match del_reply {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            return Frame {
                command: make_error(correlation_id, registry_error_code(&e), format!("{e:?}")),
                payload: Bytes::new(),
            };
        }
        None => {
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
        }
    }

    // Registry entry is gone. Now shut down partition actors, remove them
    // from state.partitions, clean spawn_locks, remove the WAL directory,
    // and (if delete_data) snapshot manifests + append a pending-delete
    // record + spawn the sweep.
    let mut handles_to_shutdown: Vec<PartitionHandle> =
        Vec::with_capacity(partition_count as usize);
    {
        let mut guard = state.partitions.write().await;
        for p in 0..partition_count {
            if let Some(h) = guard.remove(&(topic.clone(), p)) {
                handles_to_shutdown.push(h);
            }
        }
    }

    // Send Shutdown to each partition writer and await its ack so the WAL
    // and manifest state are quiesced before this RPC responds.
    for h in &handles_to_shutdown {
        let (ack_tx, ack_rx) = oneshot::channel();
        let _ = h.pw_tx.send(PwMsg::Shutdown { ack: ack_tx }).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), ack_rx).await;
    }

    // Send Shutdown to each partition's Uploader and await its ack. The
    // PartitionWriter Shutdown above already ran seal_and_handoff, so any
    // final sealed batch is already enqueued on uploader_tx; draining to
    // this Shutdown message guarantees the segment PUT + manifest update
    // are durable before we snapshot manifests below (spec invariant:
    // WAL/manifest state is quiesced when the client sees success).
    for h in &handles_to_shutdown {
        let (ack_tx, ack_rx) = oneshot::channel();
        let _ = h
            .uploader_tx
            .send(crate::uploader::UploaderMsg::Shutdown { ack: ack_tx })
            .await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), ack_rx).await;
    }

    // Clean up spawn_locks entries for this topic's partitions.
    {
        let mut locks = state.spawn_locks.lock().unwrap();
        for p in 0..partition_count {
            locks.remove(&(topic.clone(), p));
        }
    }

    if delete_data {
        use crate::deletion::sweep_deletion;
        use crate::object_store::{get, manifest_key};
        use crate::pending_deletes::{append, PendingDelete};
        use kafkrs_models::manifest::Manifest;
        use std::collections::BTreeMap;

        // Remove the WAL directory for this topic. Gated on delete_data
        // because `delete_data = false` is "detach" semantics per spec:
        // WAL files and object-store data are left untouched for the user
        // to handle out-of-band.
        let wal_dir = std::path::Path::new(&state.data_dir)
            .join("wal")
            .join(&topic);
        if wal_dir.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&wal_dir).await {
                log::warn!("failed to remove WAL dir {}: {e:?}", wal_dir.display());
            }
        }

        let mut manifests_by_partition: BTreeMap<u32, Manifest> = BTreeMap::new();
        for p in 0..partition_count {
            let mkey = manifest_key(&state.prefix, &topic, &topic_uuid, p);
            match get(&state.store, &mkey).await {
                Ok(bytes) => {
                    if let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) {
                        manifests_by_partition.insert(p, m);
                    }
                }
                Err(_) => {
                    // Manifest missing (partition may never have uploaded).
                    manifests_by_partition.insert(p, Manifest::empty(&topic, p));
                }
            }
        }

        let record = PendingDelete {
            topic: topic.clone(),
            uuid: topic_uuid.clone(),
            manifests_by_partition,
            created_ns: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0),
        };
        if let Err(e) = append(&state.data_dir, record.clone()).await {
            log::warn!("failed to persist pending_deletes.json: {e:?}");
            return Frame {
                command: make_error(
                    correlation_id,
                    ErrorCode::ErrInternal,
                    format!("pending_deletes persistence failed: {e:?}"),
                ),
                payload: Bytes::new(),
            };
        }

        metrics::gauge!(crate::metrics::DELETE_PENDING_TOPICS).increment(1.0);

        let store = state.store.clone();
        let prefix = state.prefix.clone();
        let data_dir = state.data_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = sweep_deletion(record, store, prefix, data_dir).await {
                log::warn!("sweep_deletion failed: {e:?}");
                // Pending record stays in pending_deletes.json; next boot replays.
            }
        });
    }

    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::DeleteTopicResp(DeleteTopicResponse {})),
        },
        payload: Bytes::new(),
    }
}

// ---- Overrides translation ----

fn wire_overrides_to_model(w: TopicConfigOverrides) -> TopicConfigOverridesModel {
    TopicConfigOverridesModel {
        segment_size_bytes: w.segment_size_bytes,
        segment_seal_time_ms: w.segment_seal_time_ms,
        max_key_size_bytes: w.max_key_size_bytes,
        max_value_size_bytes: w.max_value_size_bytes,
        group_commit_time_ms: w.group_commit_time_ms,
        // proto uses u64 / u32; model uses usize
        group_commit_size_bytes: w.group_commit_size_bytes.map(|v| v as usize),
        group_commit_record_count: w.group_commit_record_count.map(|v| v as usize),
        max_fetch_wait_ms: w.max_fetch_wait_ms,
        retention_ms: w.retention_ms,
        retention_bytes: w.retention_bytes,
    }
}

fn model_overrides_to_wire(m: TopicConfigOverridesModel) -> TopicConfigOverrides {
    TopicConfigOverrides {
        segment_size_bytes: m.segment_size_bytes,
        segment_seal_time_ms: m.segment_seal_time_ms,
        max_key_size_bytes: m.max_key_size_bytes,
        max_value_size_bytes: m.max_value_size_bytes,
        group_commit_time_ms: m.group_commit_time_ms,
        // model uses usize; proto uses u64 / u32
        group_commit_size_bytes: m.group_commit_size_bytes.map(|v| v as u64),
        group_commit_record_count: m.group_commit_record_count.map(|v| v as u32),
        max_fetch_wait_ms: m.max_fetch_wait_ms,
        retention_ms: m.retention_ms,
        retention_bytes: m.retention_bytes,
    }
}
