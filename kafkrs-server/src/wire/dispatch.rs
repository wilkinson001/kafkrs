//! Per-RPC handlers. Each handler turns an inbound Command into an outbound
//! response Command. Handlers are pure functions of (state, request) →
//! response and contain no connection-level concerns (no socket I/O,
//! no Connect-state tracking).

use crate::fetcher::{fetch, FetchRequest};
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
    command::Body, Command, ConnectedResponse, CreateTopicResponse, DescribeTopicResponse,
    ErrorCode, FetchResponse, ListTopicsResponse, OutRecordMeta, PongResponse, ProduceResponse,
    TopicConfigOverrides,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};

pub const PROTOCOL_VERSION: u32 = 1;
pub const BROKER_ID: &str = "kafkrs-broker-v1";

/// Handle to a partition's actor: an mpsc sender for the PartitionWriter,
/// a broadcast sender for tail subscribers, and the resolved per-topic config
/// (used by handlers to enforce per-topic limits without a registry round-trip).
#[derive(Clone)]
pub struct PartitionHandle {
    pub pw_tx: mpsc::Sender<PwMsg>,
    pub tail: broadcast::Sender<i64>,
    pub cfg: ResolvedTopicConfig,
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
    if records_meta.is_empty() {
        return Frame {
            command: make_error(
                correlation_id,
                ErrorCode::ErrMalformedFrame,
                "produce must contain at least one record",
            ),
            payload: Bytes::new(),
        };
    }

    // Slice payload into per-record (key, value) pairs using the metas.
    let mut records = Vec::with_capacity(records_meta.len());
    let mut cursor = 0usize;
    for m in &records_meta {
        let kl = m.key_len as usize;
        let vl = m.value_len as usize;
        if cursor + kl + vl > payload.len() {
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
        return Frame {
            command: make_error(
                correlation_id,
                ErrorCode::ErrMalformedFrame,
                "produce payload longer than declared record sizes",
            ),
            payload: Bytes::new(),
        };
    }

    // Auto-create the topic if configured.
    if state.auto_create {
        let (r, rr) = oneshot::channel::<Result<(), RegistryError>>();
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
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
        }
        match rr.await {
            Ok(Ok(())) => {
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
                        p,
                        cfg.clone(),
                        state.store.clone(),
                        state.prefix.clone(),
                        state.partitions.clone(),
                    )
                    .await;
                }
            }
            Ok(Err(RegistryError::AlreadyExists)) => { /* partition workers already running */ }
            Ok(Err(RegistryError::Io(msg))) => {
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
                return Frame {
                    command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                    payload: Bytes::new(),
                };
            }
        }
    }

    // Resolve the partition handle.
    let handle = {
        let guard = state.partitions.read().await;
        guard.get(&(topic.clone(), partition)).cloned()
    };
    let Some(handle) = handle else {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
            payload: Bytes::new(),
        };
    };

    let n = records.len() as i64;
    let (ack, ack_rx) = oneshot::channel::<i64>();
    if handle
        .pw_tx
        .send(PwMsg::Produce { records, ack })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    match ack_rx.await {
        Ok(hwm) => Frame {
            command: Command {
                correlation_id,
                body: Some(Body::ProduceResp(ProduceResponse {
                    base_offset: hwm - n + 1,
                    last_offset: hwm,
                    hwm,
                })),
            },
            payload: Bytes::new(),
        },
        Err(_) => Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        },
    }
}

pub async fn handle_fetch(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::FetchRequest,
) -> Frame {
    let handle = {
        let guard = state.partitions.read().await;
        guard.get(&(req.topic.clone(), req.partition)).cloned()
    };
    let Some(handle) = handle else {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
            payload: Bytes::new(),
        };
    };
    let result = fetch(
        FetchRequest {
            topic: req.topic,
            partition: req.partition,
            from_offset: req.from_offset,
            max_records: req.max_records as usize,
            max_wait_ms: req.max_wait_ms as u64,
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
            return Frame {
                command: make_error(correlation_id, fetch_error_code(&e), ""),
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
    let overrides = wire_overrides_to_model(req.overrides.unwrap_or_default());
    let (tx, rx) = oneshot::channel::<Result<(), RegistryError>>();
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
        Ok(Ok(())) => Frame {
            command: Command {
                correlation_id,
                body: Some(Body::CreateTopicResp(CreateTopicResponse {})),
            },
            payload: Bytes::new(),
        },
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
    }
}
