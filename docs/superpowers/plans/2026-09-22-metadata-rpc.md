# Metadata RPC + Broker Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `Metadata` admin RPC that returns broker addresses and per-partition leader info, plus first-class broker/cluster identity (`broker_id` + `cluster_id`) resolved at boot from config (fail-fast on missing `cluster_id`; auto-gen + disk-persist `broker_id` when unset).

**Architecture:** A new `broker_identity` module resolves identity at broker startup (config → `data_dir/broker_id` file → auto-generated `brk-<8hex>`), populates a `BrokerIdentity` struct that lives on `SharedState`, and threads through `handle_connected` (existing RPC — now returns real values) and the new `handle_metadata`. Registry access uses a new `RegistryMsg::Snapshot` variant; the wire handler filters and de-dupes client-side. Wire-protocol changes are strictly additive (new oneof arms at 54/55, new message types, new field on `ConnectedResponse`).

**Tech Stack:** Rust (tokio, prost, anyhow, serde), protobuf (buf lint/breaking), pytest (Python async client tests), cargo test (unit + integration).

**Spec:** `docs/superpowers/specs/2026-09-22-metadata-rpc-design.md`

## Global Constraints

- Version bump: all three crates from `0.6.2` to `0.7.0` in lockstep. Minor bump because `broker.cluster_id` is a newly-required config field.
- Wire protocol version stays at v1 — additive proto changes only.
- No new dependencies (auto-gen uses `uuid::Uuid::now_v7()` bytes, already a dep).
- Broker identity is immutable after boot (spec invariant #1).
- `data_dir/broker_id` file is write-once — written only in the auto-gen path; if config `broker.id` is set, the file is never touched (spec invariant #2).
- `cluster_id` is never persisted to disk (spec invariant #3).
- Metadata handler is strictly read-only — no auto-create, no state mutation (spec invariant #4).
- Every `TopicMetadata` with `error_code != 0` has empty `partitions` and empty `topic_uuid` (spec invariant #5).
- Missing `broker.cluster_id` causes fail-fast startup panic (spec invariant #8).
- `broker_id` format when auto-generated: exactly `brk-<8 lowercase hex chars>`, 12 chars total.
- Never add `Co-Authored-By` lines to commit messages. Running `git commit` is authorized for this SDD run.

---

### Task 1: Proto additions + regenerate Python bindings

**Files:**
- Modify: `kafkrs-models/proto/wire/v1.proto`
- Modify: `kafkrs-server/src/wire/connection.rs` (stub the new Body variants in the never-a-request arm so kafkrs-server keeps compiling; Task 7 replaces the request-side arm with a real dispatch)
- Modify: `kafkrs-python/kafkrs/wire/v1_pb2.py` (regenerated)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `kafkrs_models::wire::v1::MetadataRequest { topics: Vec<String> }`
  - `kafkrs_models::wire::v1::MetadataResponse { cluster_id: String, brokers: Vec<BrokerInfo>, topics: Vec<TopicMetadata> }`
  - `kafkrs_models::wire::v1::BrokerInfo { broker_id: String, host: String, port: u32 }`
  - `kafkrs_models::wire::v1::TopicMetadata { topic: String, error_code: u32, topic_uuid: String, partitions: Vec<PartitionMetadata> }`
  - `kafkrs_models::wire::v1::PartitionMetadata { partition: u32, leader_broker_id: String }`
  - `command::Body::Metadata(MetadataRequest)` at field 54
  - `command::Body::MetadataResp(MetadataResponse)` at field 55
  - `ConnectedResponse.cluster_id: String` at field 3
  - Python: `v1_pb2.MetadataRequest`, `v1_pb2.MetadataResponse`, `v1_pb2.BrokerInfo`, `v1_pb2.TopicMetadata`, `v1_pb2.PartitionMetadata`; `v1_pb2.ConnectedResponse.cluster_id`.

- [ ] **Step 1: Edit `kafkrs-models/proto/wire/v1.proto`**

Update the reserved ranges and the `Command` oneof:

```proto
message Command {
  // Reserved for v1.5+ streaming-consumer RPCs:
  //   Subscribe / Subscribed / Flow / Message / Ack / Unsubscribe
  reserved 40 to 49;
  // Admin overflow — filled as new admin RPCs land after Metadata/AlterConfig.
  reserved 56 to 59;
  // Future RPC categories (multi-broker admin, consumer-group admin, etc.).
  // Documented as reserved to prevent accidental collision when new categories arrive.
  reserved 60 to 79;

  uint64 correlation_id = 1;

  oneof body {
    // Connection lifecycle (10-19)
    ConnectRequest         connect               = 10;
    ConnectedResponse      connected             = 11;
    PingRequest            ping                  = 12;
    PongResponse           pong                  = 13;

    // Data plane (20-29)
    ProduceRequest         produce               = 20;
    ProduceResponse        produce_resp          = 21;
    FetchRequest           fetch                 = 22;
    FetchResponse          fetch_resp            = 23;

    // Control plane (30-39, 50-55)
    CreateTopicRequest        create_topic            = 30;
    CreateTopicResponse       create_topic_resp       = 31;
    DescribeTopicRequest      describe_topic          = 32;
    DescribeTopicResponse     describe_topic_resp     = 33;
    ListTopicsRequest         list_topics             = 34;
    ListTopicsResponse        list_topics_resp        = 35;
    DeleteTopicRequest        delete_topic            = 50;
    DeleteTopicResponse       delete_topic_resp       = 51;
    AlterTopicConfigRequest   alter_topic_config      = 52;
    AlterTopicConfigResponse  alter_topic_config_resp = 53;
    MetadataRequest           metadata                = 54;
    MetadataResponse          metadata_resp           = 55;

    // Errors
    ErrorResponse          error                 = 99;
  }
}
```

Add `cluster_id` to `ConnectedResponse`:

```proto
message ConnectedResponse {
  uint32 protocol_version = 1;
  string broker_id        = 2;
  string cluster_id       = 3;
}
```

Add the five new messages. Place them near the bottom of the file, after `DeleteTopicResponse` / `AlterTopicConfigResponse` and before the `ErrorResponse` block:

```proto
message MetadataRequest {
  // Empty list = return all topics.
  repeated string topics = 1;
}

message MetadataResponse {
  string cluster_id                = 1;
  repeated BrokerInfo brokers      = 2;
  repeated TopicMetadata topics    = 3;
}

message BrokerInfo {
  string broker_id = 1;
  string host      = 2;
  uint32 port      = 3;
}

message TopicMetadata {
  string topic                            = 1;
  // 0 = OK; otherwise the ErrorCode enum value.
  uint32 error_code                       = 2;
  string topic_uuid                       = 3;
  repeated PartitionMetadata partitions   = 4;
}

message PartitionMetadata {
  uint32 partition        = 1;
  string leader_broker_id = 2;
}
```

- [ ] **Step 2: Regenerate the Python bindings**

Run: `cd kafkrs-python && python3 -m grpc_tools.protoc -I ../kafkrs-models/proto --python_out=kafkrs/wire ../kafkrs-models/proto/wire/v1.proto`

If `grpc_tools` isn't installed, fall back to plain `protoc`: `protoc -I ../kafkrs-models/proto --python_out=kafkrs/wire ../kafkrs-models/proto/wire/v1.proto`. Prefer whichever tool was used for the previous regeneration (check `git log -p kafkrs-python/kafkrs/wire/v1_pb2.py` if unsure — the header comment names it).

Expected: `kafkrs-python/kafkrs/wire/v1_pb2.py` grows to include `MetadataRequest`, `MetadataResponse`, `BrokerInfo`, `TopicMetadata`, `PartitionMetadata`, and `ConnectedResponse.cluster_id`.

- [ ] **Step 3: Verify Rust bindings compile**

Run: `cargo build -p kafkrs-models`
Expected: PASS. Prost regenerates the Rust bindings under `OUT_DIR`.

- [ ] **Step 4: Verify buf lint passes**

Run: `buf lint`
Expected: PASS.

- [ ] **Step 5: Verify buf breaking passes vs master**

Run: `buf breaking --against '.git#branch=master,subdir=.'`
Expected: PASS. All additions are additive; `RESERVED_MESSAGE_NO_DELETE` / `RESERVED_ENUM_NO_DELETE` exemptions in `buf.yaml` already cover the reserved-range shrinkage.

- [ ] **Step 6: Stub the new Body variants in `wire/connection.rs`**

Adding `Body::Metadata(_)` and `Body::MetadataResp(_)` to the prost-generated enum breaks the exhaustive match in `dispatch_one`. Stub them into the never-a-request arm so `cargo build -p kafkrs-server` keeps compiling until Task 7 wires the real dispatch. In `kafkrs-server/src/wire/connection.rs`, find the never-a-request arm around lines 293-311:

```rust
        Body::Connect(_)
        | Body::Connected(_)
        | Body::Pong(_)
        | Body::ProduceResp(_)
        | Body::FetchResp(_)
        | Body::CreateTopicResp(_)
        | Body::DescribeTopicResp(_)
        | Body::ListTopicsResp(_)
        | Body::DeleteTopicResp(_)
        | Body::AlterTopicConfigResp(_)
        | Body::Error(_) => Frame { ... }
```

Add both new variants to that list:

```rust
        Body::Connect(_)
        | Body::Connected(_)
        | Body::Pong(_)
        | Body::ProduceResp(_)
        | Body::FetchResp(_)
        | Body::CreateTopicResp(_)
        | Body::DescribeTopicResp(_)
        | Body::ListTopicsResp(_)
        | Body::DeleteTopicResp(_)
        | Body::AlterTopicConfigResp(_)
        | Body::Metadata(_)
        | Body::MetadataResp(_)
        | Body::Error(_) => Frame { ... }
```

Task 7 will replace `Body::Metadata(_)` with a real dispatch arm; `Body::MetadataResp(_)` stays in the never-a-request pattern (responses are never legal requests).

- [ ] **Step 7: Verify workspace compiles**

Run: `cargo build --workspace`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add kafkrs-models/proto/wire/v1.proto kafkrs-python/kafkrs/wire/v1_pb2.py kafkrs-server/src/wire/connection.rs
git commit -m "wire: add Metadata RPC + ConnectedResponse.cluster_id (proto + stub)"
```

---

### Task 2: `BrokerConfig` gains `cluster_id` + `id` fields

**Files:**
- Modify: `kafkrs-models/src/config.rs`
- Test: `kafkrs-models/src/config.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `BrokerConfig.cluster_id: Option<String>` (defaults to `None`; runtime check in Task 3 rejects `None`)
  - `BrokerConfig.id: Option<String>` (defaults to `None`)

- [ ] **Step 1: Write failing tests**

Append inside the existing `#[cfg(test)] mod tests { ... }` block at the bottom of `kafkrs-models/src/config.rs`:

```rust
    #[test]
    fn broker_cluster_id_and_id_parse_when_present() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[broker]
cluster_id = "prod-east"
id = "broker-1"

[object_store]
backend = "filesystem"
bucket = "b"
prefix = ""
endpoint = ""
region = "us-east-1"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.broker.cluster_id.as_deref(), Some("prod-east"));
        assert_eq!(cfg.broker.id.as_deref(), Some("broker-1"));
    }

    #[test]
    fn broker_cluster_id_only_parses_id_defaults_none() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[broker]
cluster_id = "staging-us-west"

[object_store]
backend = "filesystem"
bucket = "b"
prefix = ""
endpoint = ""
region = "us-east-1"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.broker.cluster_id.as_deref(), Some("staging-us-west"));
        assert_eq!(cfg.broker.id, None);
    }

    #[test]
    fn broker_defaults_leave_cluster_id_and_id_none() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[object_store]
backend = "filesystem"
bucket = "b"
prefix = ""
endpoint = ""
region = "us-east-1"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.broker.cluster_id, None);
        assert_eq!(cfg.broker.id, None);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kafkrs-models config::tests::broker_cluster_id -- --nocapture`
Expected: FAIL with `no field 'cluster_id' on type 'BrokerConfig'` (or the equivalent).

- [ ] **Step 3: Add the fields to `BrokerConfig`**

Edit `kafkrs-models/src/config.rs`:

```rust
#[derive(Deserialize, Debug, Clone)]
pub struct BrokerConfig {
    #[serde(default)]
    pub disk_type: DiskType,
    #[serde(default)]
    pub auto_create_topics: bool,
    #[serde(default = "default_partition_count")]
    pub default_partition_count: u32,
    #[serde(default)]
    pub retention_sweep_interval_ms: Option<u64>,
    #[serde(default)]
    pub metrics_high_cardinality: bool,
    /// Cluster identifier. **Required** — broker refuses to start if unset.
    /// Load-bearing safety machinery: clients cache this to detect misconfiguration.
    #[serde(default)]
    pub cluster_id: Option<String>,
    /// Broker identifier. Optional. If unset, resolved from `data_dir/broker_id`
    /// on restart, or auto-generated (`brk-<8hex>`) and persisted on first boot.
    #[serde(default)]
    pub id: Option<String>,
}
```

Update the `Default` impl:

```rust
impl Default for BrokerConfig {
    fn default() -> Self {
        BrokerConfig {
            disk_type: DiskType::default(),
            auto_create_topics: false,
            default_partition_count: default_partition_count(),
            retention_sweep_interval_ms: None,
            metrics_high_cardinality: false,
            cluster_id: None,
            id: None,
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kafkrs-models config::tests`
Expected: all existing tests still PASS, plus the 3 new tests PASS.

- [ ] **Step 5: Commit**

```bash
git add kafkrs-models/src/config.rs
git commit -m "models: BrokerConfig gains cluster_id + id fields"
```

---

### Task 3: `broker_identity` module

**Files:**
- Create: `kafkrs-server/src/broker_identity.rs`
- Modify: `kafkrs-server/src/lib.rs` (declare the new module)
- Test: `kafkrs-server/src/broker_identity.rs` (`#[cfg(test)]` block within the file)

**Interfaces:**
- Consumes: `BrokerConfig` fields from Task 2.
- Produces:
  - `pub struct BrokerIdentity { pub broker_id: Arc<str>, pub cluster_id: Arc<str>, pub advertised_host: Arc<str>, pub advertised_port: u16 }` (derives `Clone`)
  - `pub enum IdentityError { MissingClusterId, IoError(String) }` (derives `Debug`, implements `Display` + `Error`)
  - `pub fn resolve_identity(cfg: &BrokerConfig, address: &str, wire_port: u16, data_dir: &Path) -> Result<BrokerIdentity, IdentityError>`
  - `pub fn generate_broker_id() -> String` returning `brk-<8hex>` (pub for testing).

- [ ] **Step 1: Declare the new module**

Edit `kafkrs-server/src/lib.rs`. Find the `pub mod` declarations near the top and add:

```rust
pub mod broker_identity;
```

Alphabetical placement if the file already sorts modules.

- [ ] **Step 2: Write the failing tests**

Create `kafkrs-server/src/broker_identity.rs` with the test module first (TDD):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kafkrs_models::config::BrokerConfig;
    use tempfile::tempdir;

    fn cfg_with(cluster_id: Option<&str>, id: Option<&str>) -> BrokerConfig {
        BrokerConfig {
            cluster_id: cluster_id.map(String::from),
            id: id.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_uses_config_id_when_set() {
        let dir = tempdir().unwrap();
        let cfg = cfg_with(Some("prod-east"), Some("broker-1"));
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        assert_eq!(&*ident.broker_id, "broker-1");
        assert_eq!(&*ident.cluster_id, "prod-east");
        assert_eq!(&*ident.advertised_host, "127.0.0.1");
        assert_eq!(ident.advertised_port, 5432);
        // No disk file created because config wins.
        assert!(!dir.path().join("broker_id").exists());
    }

    #[test]
    fn resolve_reads_persisted_file_when_config_id_unset() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("broker_id"), "brk-deadbeef").unwrap();
        let cfg = cfg_with(Some("prod-east"), None);
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        assert_eq!(&*ident.broker_id, "brk-deadbeef");
    }

    #[test]
    fn resolve_reads_persisted_file_trimming_trailing_newline() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("broker_id"), "brk-cafefeed\n").unwrap();
        let cfg = cfg_with(Some("prod-east"), None);
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        assert_eq!(&*ident.broker_id, "brk-cafefeed");
    }

    #[test]
    fn resolve_generates_and_persists_on_first_boot() {
        let dir = tempdir().unwrap();
        let cfg = cfg_with(Some("prod-east"), None);
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        // Generated ID matches format.
        let re = regex_lite::Regex::new(r"^brk-[0-9a-f]{8}$").unwrap();
        assert!(
            re.is_match(&ident.broker_id),
            "broker_id {:?} does not match brk-<8hex>",
            ident.broker_id
        );
        // File persisted with the same value.
        let persisted = std::fs::read_to_string(dir.path().join("broker_id")).unwrap();
        assert_eq!(persisted.trim(), &*ident.broker_id);
    }

    #[test]
    fn resolve_config_id_wins_over_disk_file() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("broker_id"), "brk-fromdisk").unwrap();
        let file_before = std::fs::read(dir.path().join("broker_id")).unwrap();
        let cfg = cfg_with(Some("prod-east"), Some("broker-config"));
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        assert_eq!(&*ident.broker_id, "broker-config");
        // Disk file untouched.
        let file_after = std::fs::read(dir.path().join("broker_id")).unwrap();
        assert_eq!(file_before, file_after);
    }

    #[test]
    fn resolve_fails_when_cluster_id_missing() {
        let dir = tempdir().unwrap();
        let cfg = cfg_with(None, Some("broker-1"));
        match resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()) {
            Err(IdentityError::MissingClusterId) => {}
            other => panic!("expected MissingClusterId, got {other:?}"),
        }
    }

    #[test]
    fn generate_broker_id_matches_brk_prefix_format() {
        let id = generate_broker_id();
        let re = regex_lite::Regex::new(r"^brk-[0-9a-f]{8}$").unwrap();
        assert!(re.is_match(&id), "{id:?} does not match brk-<8hex>");
    }

    #[test]
    fn generate_broker_id_produces_different_values() {
        let a = generate_broker_id();
        let b = generate_broker_id();
        // With 32 bits of entropy the collision probability is ~2^-32. Passing
        // twice guards against a stub implementation.
        assert_ne!(a, b, "generate_broker_id produced duplicate value");
    }
}
```

Note: the tests use `regex_lite::Regex`. If `regex_lite` isn't a workspace dep, replace the regex assertion with a manual check: `id.starts_with("brk-") && id.len() == 12 && id[4..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())`. Prefer the manual check to avoid adding a dev-dep for one test.

Rewrite using the manual check (drops the `regex_lite` reference):

```rust
    fn is_valid_broker_id(id: &str) -> bool {
        id.starts_with("brk-")
            && id.len() == 12
            && id[4..].chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }

    // Replace `re.is_match(...)` calls with `is_valid_broker_id(...)`.
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p kafkrs-server broker_identity::tests`
Expected: FAIL to compile — the module has no non-test code yet.

- [ ] **Step 4: Implement the module**

Add the non-test code to the top of `kafkrs-server/src/broker_identity.rs`:

```rust
use kafkrs_models::config::BrokerConfig;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct BrokerIdentity {
    pub broker_id: Arc<str>,
    pub cluster_id: Arc<str>,
    pub advertised_host: Arc<str>,
    pub advertised_port: u16,
}

#[derive(Debug)]
pub enum IdentityError {
    MissingClusterId,
    IoError(String),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::MissingClusterId => write!(
                f,
                "broker.cluster_id must be set in config.toml (it's the human-readable \
                 cluster identifier clients use to detect misconfiguration)"
            ),
            IdentityError::IoError(msg) => write!(f, "identity IO error: {msg}"),
        }
    }
}

impl std::error::Error for IdentityError {}

pub fn resolve_identity(
    cfg: &BrokerConfig,
    address: &str,
    wire_port: u16,
    data_dir: &Path,
) -> Result<BrokerIdentity, IdentityError> {
    let cluster_id = cfg
        .cluster_id
        .clone()
        .ok_or(IdentityError::MissingClusterId)?;
    let broker_id = resolve_broker_id(&cfg.id, data_dir)?;
    Ok(BrokerIdentity {
        broker_id: Arc::from(broker_id),
        cluster_id: Arc::from(cluster_id),
        advertised_host: Arc::from(address.to_string()),
        advertised_port: wire_port,
    })
}

fn resolve_broker_id(cfg_id: &Option<String>, data_dir: &Path) -> Result<String, IdentityError> {
    if let Some(id) = cfg_id.as_ref() {
        return Ok(id.clone());
    }
    let path = data_dir.join("broker_id");
    if path.exists() {
        return std::fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .map_err(|e| IdentityError::IoError(e.to_string()));
    }
    let id = generate_broker_id();
    std::fs::write(&path, &id).map_err(|e| IdentityError::IoError(e.to_string()))?;
    Ok(id)
}

/// Generate a fresh broker identifier of the form `brk-<8 lowercase hex chars>`.
/// Uses UUIDv7's tail (fully random per RFC 9562) to avoid pulling in a
/// dedicated random crate; the 32 bits of entropy are more than enough for
/// realistic single-cluster broker counts.
pub fn generate_broker_id() -> String {
    let uuid = uuid::Uuid::now_v7();
    let bytes = uuid.as_bytes();
    format!(
        "brk-{:02x}{:02x}{:02x}{:02x}",
        bytes[12], bytes[13], bytes[14], bytes[15]
    )
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p kafkrs-server broker_identity::tests`
Expected: all 8 tests PASS.

- [ ] **Step 6: Verify workspace still compiles**

Run: `cargo build --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add kafkrs-server/src/lib.rs kafkrs-server/src/broker_identity.rs
git commit -m "broker_identity: resolve_identity + BrokerIdentity + generate_broker_id"
```

---

### Task 4: `RegistryMsg::Snapshot`

**Files:**
- Modify: `kafkrs-server/src/topic_registry.rs`
- Test: `kafkrs-server/src/topic_registry.rs` (existing `#[cfg(test)]` block)

**Interfaces:**
- Consumes: `TopicEntry` (already exists on the registry).
- Produces: `RegistryMsg::Snapshot { reply: oneshot::Sender<Vec<TopicEntry>> }`.

- [ ] **Step 1: Write failing tests**

Append inside the existing `#[cfg(test)] mod tests` block at the bottom of `kafkrs-server/src/topic_registry.rs`:

```rust
    #[tokio::test]
    async fn snapshot_returns_all_topics_in_map() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd, DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        // Create two topics.
        for (name, pc) in [("orders", 2u32), ("events", 3u32)] {
            let (r, rr) = oneshot::channel();
            tx.send(RegistryMsg::Create {
                name: name.into(),
                partition_count: pc,
                overrides: TopicConfigOverrides::default(),
                reply: r,
            })
            .await
            .unwrap();
            rr.await.unwrap().unwrap();
        }

        // Snapshot.
        let (r, rr) = oneshot::channel();
        tx.send(RegistryMsg::Snapshot { reply: r }).await.unwrap();
        let snapshot = rr.await.unwrap();
        assert_eq!(snapshot.len(), 2);
        let mut names: Vec<String> = snapshot.iter().map(|t| t.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["events".to_string(), "orders".to_string()]);
        // Each entry carries a valid uuid + partition_count.
        for t in &snapshot {
            assert!(!t.uuid.is_empty());
            assert!(t.partition_count >= 1);
        }
    }

    #[tokio::test]
    async fn snapshot_returns_empty_when_registry_empty() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd, DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r, rr) = oneshot::channel();
        tx.send(RegistryMsg::Snapshot { reply: r }).await.unwrap();
        let snapshot = rr.await.unwrap();
        assert!(snapshot.is_empty());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kafkrs-server topic_registry::tests::snapshot`
Expected: FAIL to compile with `no variant or associated item named 'Snapshot' found for enum 'RegistryMsg'`.

- [ ] **Step 3: Add the `RegistryMsg::Snapshot` variant**

In `kafkrs-server/src/topic_registry.rs`, extend the `RegistryMsg` enum. Add just before the closing `}`:

```rust
    /// Return a snapshot of every `TopicEntry` currently in the registry.
    /// Used by the wire layer's `handle_metadata` to build the Metadata
    /// response in one round trip instead of N Describes.
    Snapshot {
        reply: oneshot::Sender<Vec<TopicEntry>>,
    },
```

- [ ] **Step 4: Wire the handler arm into `run`**

In `TopicRegistry::run`, add an arm to the `match msg { ... }` block, next to the other handlers:

```rust
                RegistryMsg::Snapshot { reply } => {
                    let _ = reply.send(self.topics.values().cloned().collect());
                }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p kafkrs-server topic_registry::tests`
Expected: all existing tests still PASS, plus the 2 new tests PASS.

- [ ] **Step 6: Commit**

```bash
git add kafkrs-server/src/topic_registry.rs
git commit -m "topic_registry: RegistryMsg::Snapshot for one-shot topic list"
```

---

### Task 5: Wire `BrokerIdentity` through `SharedState`, `handle_connected`, and `main.rs`

**Files:**
- Modify: `kafkrs-server/src/wire/dispatch.rs`
- Modify: `kafkrs-server/src/main.rs`

**Interfaces:**
- Consumes:
  - `BrokerIdentity` from Task 3.
  - `resolve_identity` from Task 3.
  - `BrokerConfig.cluster_id` / `.id` from Task 2.
- Produces:
  - `SharedState.identity: BrokerIdentity` field (new).
  - `handle_connected(correlation_id: u64, state: &SharedState) -> Frame` signature (adds `&SharedState` param).

- [ ] **Step 1: Extend `SharedState`**

In `kafkrs-server/src/wire/dispatch.rs`, find the `SharedState` struct (around line 57-69):

```rust
#[derive(Clone)]
pub struct SharedState {
    pub partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
    pub registry: mpsc::Sender<RegistryMsg>,
    pub store: Arc<dyn object_store::ObjectStore>,
    pub prefix: String,
    pub auto_create: bool,
    pub default_partition_count: u32,
    pub data_dir: String,
    pub disk_type: DiskType,
    pub spawn_locks: PartitionSpawnLocks,
}
```

Add the `identity` field at the end (keeps the diff minimal and puts identity next to other broker-scoped state):

```rust
    pub identity: crate::broker_identity::BrokerIdentity,
```

- [ ] **Step 2: Update `handle_connected` to source identity from state**

Also in `wire/dispatch.rs`, delete the `pub const BROKER_ID: &str = "kafkrs-broker-v1";` line (~line 39). Then change `handle_connected` (~line 83) to take `&SharedState`:

```rust
pub fn handle_connected(correlation_id: u64, state: &SharedState) -> Frame {
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::Connected(ConnectedResponse {
                protocol_version: PROTOCOL_VERSION,
                broker_id: state.identity.broker_id.to_string(),
                cluster_id: state.identity.cluster_id.to_string(),
            })),
        },
        payload: Bytes::new(),
    }
}
```

- [ ] **Step 3: Update `handle_connected` callsite in `wire/connection.rs`**

In `kafkrs-server/src/wire/connection.rs`, find the `handle_connected(cid)` call inside `run_connection`'s dispatcher (around line 192 in the `(false, Some(Body::Connect(req)))` arm):

```rust
                count_rpc("connect", 0);
                let _ = resp_tx.send(handle_connected(cid)).await;
                connected = true;
```

Change to:

```rust
                count_rpc("connect", 0);
                let _ = resp_tx.send(handle_connected(cid, &state)).await;
                connected = true;
```

- [ ] **Step 4: Thread identity resolution through `main.rs`**

In `kafkrs-server/src/main.rs`, add the `broker_identity` import at the top:

```rust
use kafkrs_server::broker_identity::resolve_identity;
```

Right after `let cfg: kafkrs_models::config::Config = config::load_config(config_path);`, resolve identity before anything else needs it:

```rust
    // Broker identity — fail-fast if cluster_id missing. Panics with a clear
    // error naming the field.
    let wire_port_for_advertise = *cfg.ports.wire.first().expect(
        "at least one wire port must be configured (ports.wire = [...])",
    );
    let identity = resolve_identity(
        &cfg.broker,
        &cfg.address,
        wire_port_for_advertise,
        std::path::Path::new(&cfg.data_dir),
    )
    .unwrap_or_else(|e| panic!("broker identity resolution failed: {e}"));
```

Then in the `SharedState { ... }` construction block (~line 81-91), add:

```rust
        identity: identity.clone(),
```

- [ ] **Step 5: Update all existing SharedState construction sites in tests**

Search the crate for `SharedState {` usages:

```
grep -rn "SharedState {" kafkrs-server/src kafkrs-server/tests
```

Every test file that constructs a `SharedState` (typically `wire_e2e.rs` and `metrics_high_cardinality_e2e.rs`) needs to add `identity: ...`. Use a test helper constant like:

```rust
use kafkrs_server::broker_identity::BrokerIdentity;
use std::sync::Arc;

fn test_identity() -> BrokerIdentity {
    BrokerIdentity {
        broker_id: Arc::from("brk-testtest".to_string()),
        cluster_id: Arc::from("test-cluster".to_string()),
        advertised_host: Arc::from("127.0.0.1".to_string()),
        advertised_port: 5432,
    }
}
```

Then use `identity: test_identity()` in every `SharedState { ... }` block. This keeps tests independent of the identity resolution logic.

If `setup_broker` in `wire_e2e.rs` is the only construction site, update just that. If there are multiple copies (one per test binary), update each.

- [ ] **Step 6: Verify workspace still compiles**

Run: `cargo build --workspace`
Expected: PASS.

- [ ] **Step 7: Run the full server test suite**

Run: `cargo test -p kafkrs-server`
Expected: all existing tests still PASS. (`handle_connected` now emits a `cluster_id` in `ConnectedResponse`, but that's an additive proto field; existing wire e2e tests that pattern-match `Body::Connected(_)` continue to succeed.)

- [ ] **Step 8: Verify clippy is clean**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add kafkrs-server/src/wire/dispatch.rs kafkrs-server/src/wire/connection.rs kafkrs-server/src/main.rs kafkrs-server/tests/wire_e2e.rs kafkrs-server/tests/metrics_high_cardinality_e2e.rs
git commit -m "wire+startup: thread BrokerIdentity through SharedState + Connect"
```

(Include whichever test files you touched in Step 5; if `metrics_high_cardinality_e2e.rs` didn't need changes, omit it.)

---

### Task 6: `handle_metadata` in `wire/dispatch.rs`

**Files:**
- Modify: `kafkrs-server/src/wire/dispatch.rs`

**Interfaces:**
- Consumes:
  - `RegistryMsg::Snapshot` from Task 4.
  - `SharedState.identity` from Task 5.
  - Proto `MetadataRequest` / `MetadataResponse` / `BrokerInfo` / `TopicMetadata` / `PartitionMetadata` from Task 1.
- Produces: `pub async fn handle_metadata(correlation_id: u64, state: &SharedState, req: MetadataRequest) -> Frame`.

- [ ] **Step 1: Add the handler function**

At the bottom of `kafkrs-server/src/wire/dispatch.rs`, just above the `fn wire_overrides_to_model` block, add:

```rust
pub async fn handle_metadata(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::MetadataRequest,
) -> Frame {
    use kafkrs_models::topic::TopicEntry;
    use kafkrs_models::wire::v1::{
        BrokerInfo, MetadataResponse, PartitionMetadata, TopicMetadata,
    };
    use std::collections::{HashMap, HashSet};

    // Snapshot the registry (one round trip).
    let (tx, rx) = oneshot::channel::<Vec<TopicEntry>>();
    if state
        .registry
        .send(RegistryMsg::Snapshot { reply: tx })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    let snapshot: Vec<TopicEntry> = match rx.await {
        Ok(s) => s,
        Err(_) => {
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
        }
    };
    let by_name: HashMap<String, TopicEntry> = snapshot
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();

    // Determine the topic list to return.
    let leader_id = state.identity.broker_id.to_string();
    let topics: Vec<TopicMetadata> = if req.topics.is_empty() {
        // All topics.
        by_name
            .values()
            .map(|entry| topic_meta_from_entry(entry, &leader_id))
            .collect()
    } else {
        // Filtered — dedupe request while preserving first-seen order.
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out: Vec<TopicMetadata> = Vec::with_capacity(req.topics.len());
        for name in &req.topics {
            if !seen.insert(name.as_str()) {
                continue;
            }
            match by_name.get(name) {
                Some(entry) => out.push(topic_meta_from_entry(entry, &leader_id)),
                None => out.push(TopicMetadata {
                    topic: name.clone(),
                    error_code: ErrorCode::ErrUnknownTopic as u32,
                    topic_uuid: String::new(),
                    partitions: Vec::new(),
                }),
            }
        }
        out
    };

    let brokers = vec![BrokerInfo {
        broker_id: state.identity.broker_id.to_string(),
        host: state.identity.advertised_host.to_string(),
        port: state.identity.advertised_port as u32,
    }];

    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::MetadataResp(MetadataResponse {
                cluster_id: state.identity.cluster_id.to_string(),
                brokers,
                topics,
            })),
        },
        payload: Bytes::new(),
    }
}

fn topic_meta_from_entry(
    entry: &kafkrs_models::topic::TopicEntry,
    leader_broker_id: &str,
) -> kafkrs_models::wire::v1::TopicMetadata {
    use kafkrs_models::wire::v1::{PartitionMetadata, TopicMetadata};
    let partitions: Vec<PartitionMetadata> = (0..entry.partition_count)
        .map(|p| PartitionMetadata {
            partition: p,
            leader_broker_id: leader_broker_id.to_string(),
        })
        .collect();
    TopicMetadata {
        topic: entry.name.clone(),
        error_code: 0,
        topic_uuid: entry.uuid.clone(),
        partitions,
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p kafkrs-server`
Expected: PASS. The function is `pub` but not yet called from `dispatch_one`; expect a `dead_code` warning at most. Task 7 wires it.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add kafkrs-server/src/wire/dispatch.rs
git commit -m "wire/dispatch: handle_metadata"
```

---

### Task 7: Wire `handle_metadata` into `dispatch_one`

**Files:**
- Modify: `kafkrs-server/src/wire/connection.rs`

**Interfaces:**
- Consumes: `handle_metadata` from Task 6; `Body::Metadata` from Task 1.
- Produces: `dispatch_one` routing `Body::Metadata` → `handle_metadata`, and `WIRE_RPC_REQUESTS` label `"metadata"`.

- [ ] **Step 1: Add the handler to the import list**

Edit the `use crate::wire::dispatch::{...}` block at the top of `kafkrs-server/src/wire/connection.rs`. Add `handle_metadata` alphabetically:

```rust
use crate::wire::dispatch::{
    handle_alter_topic_config, handle_connected, handle_create_topic, handle_delete_topic,
    handle_describe_topic, handle_fetch, handle_list_topics, handle_metadata, handle_ping,
    handle_produce, SharedState, PROTOCOL_VERSION,
};
```

- [ ] **Step 2: Add the RPC label**

In `dispatch_one` (around line 265), extend the `__rpc` match. Add `Body::Metadata(_) => "metadata"` alongside the existing labels:

```rust
    let __rpc = match &body {
        Body::Ping(_) => "ping",
        Body::Produce(_) => "produce",
        Body::Fetch(_) => "fetch",
        Body::CreateTopic(_) => "create_topic",
        Body::DescribeTopic(_) => "describe_topic",
        Body::ListTopics(_) => "list_topics",
        Body::DeleteTopic(_) => "delete_topic",
        Body::AlterTopicConfig(_) => "alter_topic_config",
        Body::Metadata(_) => "metadata",
        _ => "unknown",
    };
```

- [ ] **Step 3: Add the real dispatch arm and remove the stub**

In the `match body { ... }` block, add a real arm:

```rust
        Body::Metadata(req) => handle_metadata(correlation_id, state, req).await,
```

Then remove `Body::Metadata(_)` from the never-a-request arm added in Task 1. The arm should now read:

```rust
        Body::Connect(_)
        | Body::Connected(_)
        | Body::Pong(_)
        | Body::ProduceResp(_)
        | Body::FetchResp(_)
        | Body::CreateTopicResp(_)
        | Body::DescribeTopicResp(_)
        | Body::ListTopicsResp(_)
        | Body::DeleteTopicResp(_)
        | Body::AlterTopicConfigResp(_)
        | Body::MetadataResp(_)
        | Body::Error(_) => Frame { ... }
```

(`MetadataResp` STAYS in the never-a-request arm — responses are never valid requests.)

- [ ] **Step 4: Verify build**

Run: `cargo build -p kafkrs-server`
Expected: PASS.

- [ ] **Step 5: Run all existing tests**

Run: `cargo test -p kafkrs-server`
Expected: PASS.

- [ ] **Step 6: Verify clippy is clean**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add kafkrs-server/src/wire/connection.rs
git commit -m "wire/connection: dispatch Metadata"
```

---

### Task 8: Integration tests in `wire_e2e.rs`

**Files:**
- Modify: `kafkrs-server/tests/wire_e2e.rs`

**Interfaces:**
- Consumes: everything from Tasks 1-7; existing `setup_broker`, `encode`, `read_frame` helpers.
- Produces: 5 new integration tests.

- [ ] **Step 1: Write failing tests**

Append to the end of `kafkrs-server/tests/wire_e2e.rs`. Match the existing pattern (Connect + call + assert). If `setup_broker` needs to know the identity's `cluster_id`/`broker_id`, use whatever value your test-identity helper (from Task 5 Step 5) returns.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_response_carries_broker_id_and_cluster_id() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "e2e".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::Connected(c)) => {
            assert_eq!(c.broker_id, "brk-testtest");
            assert_eq!(c.cluster_id, "test-cluster");
        }
        other => panic!("expected Connected, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_empty_filter_returns_all_topics_and_self_broker() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect.
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "e2e".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // Metadata request with empty filter.
    let md = Command {
        correlation_id: 2,
        body: Some(Body::Metadata(
            kafkrs_models::wire::v1::MetadataRequest { topics: vec![] },
        )),
    };
    sock.write_all(&encode(&md, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::MetadataResp(m)) => {
            assert_eq!(m.cluster_id, "test-cluster");
            assert_eq!(m.brokers.len(), 1);
            assert_eq!(m.brokers[0].broker_id, "brk-testtest");
            assert_eq!(m.brokers[0].host, "127.0.0.1");
            assert_eq!(m.brokers[0].port, 5432);
            // setup_broker seeds topic "t" — expect at least that.
            let names: Vec<&str> = m.topics.iter().map(|t| t.topic.as_str()).collect();
            assert!(names.contains(&"t"), "expected topic 't' in metadata, got {names:?}");
            let t = m.topics.iter().find(|t| t.topic == "t").unwrap();
            assert_eq!(t.error_code, 0);
            assert!(!t.topic_uuid.is_empty());
            assert!(!t.partitions.is_empty());
            for p in &t.partitions {
                assert_eq!(p.leader_broker_id, "brk-testtest");
            }
        }
        other => panic!("expected MetadataResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_filter_returns_only_requested_topics() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "e2e".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    let md = Command {
        correlation_id: 2,
        body: Some(Body::Metadata(
            kafkrs_models::wire::v1::MetadataRequest {
                topics: vec!["t".into()],
            },
        )),
    };
    sock.write_all(&encode(&md, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::MetadataResp(m)) => {
            assert_eq!(m.topics.len(), 1);
            assert_eq!(m.topics[0].topic, "t");
            assert_eq!(m.topics[0].error_code, 0);
        }
        other => panic!("expected MetadataResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_filter_with_unknown_topic_returns_per_topic_error() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "e2e".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    let md = Command {
        correlation_id: 2,
        body: Some(Body::Metadata(
            kafkrs_models::wire::v1::MetadataRequest {
                topics: vec!["t".into(), "no-such-topic".into()],
            },
        )),
    };
    sock.write_all(&encode(&md, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::MetadataResp(m)) => {
            assert_eq!(m.topics.len(), 2);
            let ok = m.topics.iter().find(|t| t.topic == "t").expect("t missing");
            assert_eq!(ok.error_code, 0);
            assert!(!ok.topic_uuid.is_empty());
            assert!(!ok.partitions.is_empty());

            let bad = m
                .topics
                .iter()
                .find(|t| t.topic == "no-such-topic")
                .expect("no-such-topic missing");
            assert_eq!(bad.error_code, ErrorCode::ErrUnknownTopic as u32);
            assert!(bad.topic_uuid.is_empty());
            assert!(bad.partitions.is_empty());
        }
        other => panic!("expected MetadataResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_partition_metadata_lists_self_as_leader_for_every_partition() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "e2e".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // Create a topic with 3 partitions.
    let create = Command {
        correlation_id: 2,
        body: Some(Body::CreateTopic(
            kafkrs_models::wire::v1::CreateTopicRequest {
                topic: "multi".into(),
                partition_count: 3,
                overrides: None,
            },
        )),
    };
    sock.write_all(&encode(&create, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    let md = Command {
        correlation_id: 3,
        body: Some(Body::Metadata(
            kafkrs_models::wire::v1::MetadataRequest {
                topics: vec!["multi".into()],
            },
        )),
    };
    sock.write_all(&encode(&md, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::MetadataResp(m)) => {
            let t = &m.topics[0];
            assert_eq!(t.partitions.len(), 3);
            let mut pids: Vec<u32> = t.partitions.iter().map(|p| p.partition).collect();
            pids.sort();
            assert_eq!(pids, vec![0, 1, 2]);
            for p in &t.partitions {
                assert_eq!(p.leader_broker_id, "brk-testtest");
            }
        }
        other => panic!("expected MetadataResp, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run the new tests**

Run: `cargo test -p kafkrs-server --test wire_e2e metadata_ connect_response_carries_broker_id_and_cluster_id`
Expected: all 5 PASS.

- [ ] **Step 3: Run the full server suite**

Run: `cargo test -p kafkrs-server`
Expected: all tests PASS.

- [ ] **Step 4: Clippy clean**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add kafkrs-server/tests/wire_e2e.rs
git commit -m "wire_e2e: integration tests for Metadata + Connect identity"
```

---

### Task 9: Python client `get_metadata` + tests

**Files:**
- Modify: `kafkrs-python/kafkrs/client.py`
- Modify: `kafkrs-python/tests/test_client.py`

**Interfaces:**
- Consumes: `v1_pb2.MetadataRequest`, `v1_pb2.MetadataResponse`, `v1_pb2.ConnectedResponse.cluster_id` (from Task 1).
- Produces: `Client.get_metadata(topics: Optional[List[str]] = None) -> v1_pb2.MetadataResponse`.

- [ ] **Step 1: Write failing tests**

Append to `kafkrs-python/tests/test_client.py`. Grep the existing file for the current fixture pattern (`grep '@pytest.fixture' tests/test_client.py`); the AlterTopicConfig work established `broker_no_auto_create` as the fixture in use. Reuse whichever fixture exists.

```python
async def test_get_metadata_returns_broker_and_topic_info(broker_no_auto_create):
    port = broker_no_auto_create
    async with Client("127.0.0.1", port) as client:
        await client.create_topic("orders", partition_count=2)
        resp = await client.get_metadata()
        assert resp.cluster_id  # non-empty
        assert len(resp.brokers) == 1
        assert resp.brokers[0].broker_id  # non-empty
        names = [t.topic for t in resp.topics]
        assert "orders" in names
        t = next(t for t in resp.topics if t.topic == "orders")
        assert t.error_code == 0
        assert t.topic_uuid  # non-empty
        assert len(t.partitions) == 2
        for p in t.partitions:
            assert p.leader_broker_id == resp.brokers[0].broker_id


async def test_get_metadata_filter_returns_subset(broker_no_auto_create):
    port = broker_no_auto_create
    async with Client("127.0.0.1", port) as client:
        await client.create_topic("a", partition_count=1)
        await client.create_topic("b", partition_count=1)
        resp = await client.get_metadata(topics=["a"])
        names = [t.topic for t in resp.topics]
        assert names == ["a"]


async def test_get_metadata_unknown_topic_has_per_topic_error(broker_no_auto_create):
    port = broker_no_auto_create
    async with Client("127.0.0.1", port) as client:
        await client.create_topic("real", partition_count=1)
        resp = await client.get_metadata(topics=["real", "not-a-topic"])
        by_name = {t.topic: t for t in resp.topics}
        assert by_name["real"].error_code == 0
        assert by_name["real"].topic_uuid  # non-empty
        assert len(by_name["real"].partitions) == 1
        assert by_name["not-a-topic"].error_code == v1_pb2.ERR_UNKNOWN_TOPIC
        assert by_name["not-a-topic"].topic_uuid == ""
        assert len(by_name["not-a-topic"].partitions) == 0


async def test_connect_populates_cluster_id_on_response(broker_no_auto_create):
    # The Connect response is consumed by Client.connect(), which doesn't
    # currently expose the parsed ConnectedResponse. This test drives out
    # a client-side change to make cluster_id observable, OR runs a raw
    # Connect through _roundtrip to inspect the response directly.
    # Follow the second approach — it exercises the wire without changing
    # the Client public API.
    port = broker_no_auto_create
    async with Client("127.0.0.1", port) as client:
        # Send a Ping/Pong to prove the client is connected, then inspect
        # nothing further — the meaningful assertion is that the wire
        # request/response roundtrip already validated cluster_id shape.
        # For an assertion, do a raw Connect + response inspection via the
        # module-level helper. Since the Client already consumed the
        # single Connect on construction, dial a fresh socket:
        pass

    # Fresh raw socket to inspect the Connect response.
    import asyncio, struct
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    try:
        cmd = v1_pb2.Command()
        cmd.correlation_id = 42
        cmd.connect.protocol_version = 1
        cmd.connect.client_id = "cluster-id-test"
        cmd_bytes = cmd.SerializeToString()
        total_size = 4 + len(cmd_bytes)
        writer.write(struct.pack(">II", total_size, len(cmd_bytes)))
        writer.write(cmd_bytes)
        await writer.drain()

        outer = await reader.readexactly(4)
        (resp_total,) = struct.unpack(">I", outer)
        body = await reader.readexactly(resp_total)
        (resp_cmd_size,) = struct.unpack(">I", body[:4])
        resp = v1_pb2.Command()
        resp.ParseFromString(body[4 : 4 + resp_cmd_size])
        assert resp.WhichOneof("body") == "connected"
        assert resp.connected.broker_id  # non-empty
        assert resp.connected.cluster_id  # non-empty
    finally:
        writer.close()
        await writer.wait_closed()
```

If `v1_pb2` isn't already imported at the top of the test file, add `from kafkrs.wire import v1_pb2`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd kafkrs-python && python3 -m pytest tests/test_client.py::test_get_metadata_returns_broker_and_topic_info -v`
Expected: FAIL with `AttributeError: 'Client' object has no attribute 'get_metadata'`.

- [ ] **Step 3: Implement `Client.get_metadata`**

Add to `kafkrs-python/kafkrs/client.py`, just after `list_topics` (around line 200 depending on prior tasks' additions):

```python
    async def get_metadata(
        self,
        topics: Optional[List[str]] = None,
    ) -> v1_pb2.MetadataResponse:
        """Return broker + topic metadata.

        Empty `topics` (or `None`) returns metadata for all topics. Otherwise
        returns metadata for the requested topics; unknown topics come back
        with a per-topic `error_code = ERR_UNKNOWN_TOPIC`.
        """
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        if topics:
            cmd.metadata.topics.extend(topics)
        else:
            # Ensure the metadata oneof arm is set even when the filter list
            # is empty; without this, WhichOneof("body") is None on the wire.
            cmd.metadata.SetInParent()
        resp, _ = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "metadata_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")
        return resp.metadata_resp
```

- [ ] **Step 4: Run tests**

Run: `cd kafkrs-python && python3 -m pytest tests/test_client.py -v`
Expected: all tests PASS (existing + 4 new).

- [ ] **Step 5: Commit**

```bash
git add kafkrs-python/kafkrs/client.py kafkrs-python/tests/test_client.py
git commit -m "python: Client.get_metadata + tests"
```

---

### Task 10: Version bump to 0.7.0 + CHANGELOG entries + README + config.toml

**Files:**
- Modify: `kafkrs-models/Cargo.toml`
- Modify: `kafkrs-server/Cargo.toml`
- Modify: `kafkrs-python/pyproject.toml`
- Modify: `kafkrs-python/kafkrs/__init__.py`
- Modify: `Cargo.lock` (auto-regenerated by cargo)
- Modify: `kafkrs-models/CHANGELOG.md`
- Modify: `kafkrs-server/CHANGELOG.md`
- Modify: `kafkrs-python/CHANGELOG.md`
- Modify: `README.md`
- Modify: `config.toml`

**Interfaces:**
- Consumes: everything from Tasks 1-9.
- Produces: 0.7.0 across all three crates.

- [ ] **Step 1: Bump crate versions**

Change `version = "0.6.2"` → `version = "0.7.0"` in each of:
- `kafkrs-models/Cargo.toml`
- `kafkrs-server/Cargo.toml`
- `kafkrs-python/pyproject.toml`
- `kafkrs-python/kafkrs/__init__.py` (the `__version__` string)

If `kafkrs-server/Cargo.toml` pins `kafkrs-models` by version (e.g. `kafkrs-models = { path = "...", version = "0.6.2" }`), bump that pin too.

- [ ] **Step 2: Rebuild to refresh `Cargo.lock`**

Run: `cargo build --workspace`
Expected: PASS. Confirm `Cargo.lock` shows `kafkrs-models 0.7.0` and `kafkrs-server 0.7.0`.

- [ ] **Step 3: Prepend CHANGELOG entries**

Each CHANGELOG uses the existing format `## [X.Y.Z] — YYYY-MM-DD`. Check the last entry in each file for the exact spacing, then prepend a new entry above it.

`kafkrs-models/CHANGELOG.md`:

```markdown
## [0.7.0] — 2026-09-22

- Added `MetadataRequest` / `MetadataResponse` at oneof fields 54 / 55.
- Added `BrokerInfo`, `TopicMetadata`, `PartitionMetadata` message types.
- Added `ConnectedResponse.cluster_id` at field 3.
- Shrunk the admin reserved range to `[56, 59]`; pre-reserved `[60, 79]` for future RPC categories.
- Added `BrokerConfig.cluster_id: Option<String>` and `BrokerConfig.id: Option<String>` fields.

### Breaking changes

- `broker.cluster_id` is now **required** in `config.toml`. Deployments upgrading from 0.6.x must add this field before starting the 0.7.0 broker; startup panics with a clear error otherwise.
```

`kafkrs-server/CHANGELOG.md`:

```markdown
## [0.7.0] — 2026-09-22

- Added `Metadata` admin RPC returning per-partition leader info + broker addresses. Empty topic filter returns all topics; unknown topics in a filter return per-topic `error_code = ERR_UNKNOWN_TOPIC` with empty partitions and empty `topic_uuid`.
- Added broker identity resolution at boot: `broker.id` in config wins; otherwise reads `data_dir/broker_id`; otherwise auto-generates `brk-<8hex>` and persists on first boot.
- `ConnectedResponse` now returns real `broker_id` and `cluster_id` sourced from resolved identity (previously a build-time constant).
- Added `RegistryMsg::Snapshot` for one-shot topic list queries.

### Breaking changes

- `broker.cluster_id` is now **required** in `config.toml`. The broker refuses to start with a clear error if unset. Rationale: `cluster_id` is a load-bearing safety mechanism (clients cache it to detect misconfiguration); silently auto-generating it defeats the purpose.
```

`kafkrs-python/CHANGELOG.md`:

```markdown
## [0.7.0] — 2026-09-22

- Added `Client.get_metadata(topics=None) -> MetadataResponse`. Returns broker addresses and per-topic/per-partition metadata. Empty `topics` returns all; unknown topics come back with per-topic error codes.
- `ConnectedResponse` now carries a `cluster_id` field.
```

- [ ] **Step 4: Bump README status line**

Grep for the current version string in `README.md`: `grep -n '0\.6\.' README.md`. Change whichever version marker is there to `0.7.0`.

- [ ] **Step 5: Update `config.toml` with the new required field**

Add `broker.cluster_id` and a commented-out `broker.id` to the shipped example config at `/Users/owilkinson/repos/personal/kafkrs/config.toml`:

```toml
[broker]
disk_type = "nvme"
auto_create_topics = false
default_partition_count = 1
cluster_id = "kafkrs-local"
# id = "broker-1"          # optional; auto-generated as brk-<8hex> and persisted to data/broker_id if unset
```

Preserve the other existing `[broker]` fields (do not delete `disk_type`, `default_partition_count`, etc.).

- [ ] **Step 6: Full workspace verification**

Run in parallel:
- `cargo build --workspace` → PASS
- `cargo test --workspace` → PASS
- `cargo clippy --workspace --all-targets -- -D warnings` → PASS
- `cd kafkrs-python && python3 -m pytest tests/` → PASS

- [ ] **Step 7: Manual smoke (skip if broker startup would block on stdin)**

Skip the interactive smoke — Task 8 (Rust e2e) and Task 9 (Python integration) already cover the behaviour. Note in the report that the smoke was skipped in favour of e2e coverage.

- [ ] **Step 8: Commit**

```bash
git add kafkrs-models/Cargo.toml kafkrs-server/Cargo.toml kafkrs-python/pyproject.toml kafkrs-python/kafkrs/__init__.py Cargo.lock kafkrs-models/CHANGELOG.md kafkrs-server/CHANGELOG.md kafkrs-python/CHANGELOG.md README.md config.toml
git commit -m "release: 0.7.0 (Metadata RPC + broker identity)"
```

---
