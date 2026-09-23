//! Per-RPC dispatch. Each handler turns an inbound Command into an outbound
//! response Command. Handlers are pure functions of (state, request) →
//! response and contain no connection-level concerns (no socket I/O,
//! no Connect-state tracking).
//!
//! Organized by RPC family — one submodule per group. Shared types
//! (`SharedState`, `PartitionHandle`) and the wire↔model override
//! translation helpers live here; handlers live in the submodules and are
//! re-exported so callers `use crate::wire::dispatch::handle_produce`
//! unchanged.

use crate::partition_writer::PwMsg;
use crate::topic_registry::RegistryMsg;
use kafkrs_models::config::DiskType;
use kafkrs_models::topic::{
    ResolvedTopicConfig, TopicConfigOverrides as TopicConfigOverridesModel,
};
use kafkrs_models::wire::v1::TopicConfigOverrides;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex, RwLock};

mod fetch;
mod lifecycle;
mod metadata;
mod produce;
mod topic_admin;

pub use fetch::handle_fetch;
pub use lifecycle::{handle_connected, handle_ping};
pub use metadata::handle_metadata;
pub use produce::handle_produce;
pub use topic_admin::{
    handle_alter_topic_config, handle_create_topic, handle_delete_topic, handle_describe_topic,
    handle_list_topics,
};

/// Per-(topic, partition) locks coordinating concurrent `spawn_partition`
/// calls. Outer std::sync::Mutex guards the map (held briefly for entry
/// lookup/insert, never across await); per-key tokio::sync::Mutex is held
/// across the full spawn body (which awaits on recovery and channel setup).
pub type PartitionSpawnLocks = Arc<StdMutex<HashMap<(String, u32), Arc<TokioMutex<()>>>>>;

pub const PROTOCOL_VERSION: u32 = 1;

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
    pub identity: crate::broker_identity::BrokerIdentity,
}

// ---- Overrides translation ----

pub(super) fn wire_overrides_to_model(w: TopicConfigOverrides) -> TopicConfigOverridesModel {
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

pub(super) fn model_overrides_to_wire(m: TopicConfigOverridesModel) -> TopicConfigOverrides {
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
