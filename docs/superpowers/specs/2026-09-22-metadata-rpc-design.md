# Metadata RPC + Broker Identity — Design

**Status:** Draft for review
**Date:** 2026-09-22
**Scope:** A `Metadata` admin RPC that lets clients ask "which broker owns partition P of topic T?" and enumerate broker addresses, plus first-class broker and cluster identity (`broker_id` + `cluster_id`) surfaced through the `Connect` handshake and the Metadata response. Single-broker only for now; the schema is designed so multi-broker Phase 4 can slot in real per-partition leader assignments without breaking clients. Bundles roadmap items 2.1 (Metadata RPC) and 2.2 (broker identity).

## Motivation

Two forces push these features together into one release:

Consumer groups (Phase 2.3) need a stable way to ask "which broker do I send Fetch requests for partition P of topic T to?" — even in single-broker mode where the answer is always "this one." Landing the client-facing surface now means multi-broker Phase 6 redesigns only the routing internals, not the API. Same argument was made for AlterTopicConfig: land the client-side contract before the coordination substrate exists, so cluster work reshapes only cluster mechanics.

Broker identity is the other half of the same shape. Every Metadata response needs to name brokers, and those names have to be stable across broker restarts and unique across a cluster. `ConnectedResponse.broker_id` already exists in the wire proto but is hard-coded to a build-time constant (`"kafkrs-broker-v1"`), and no `cluster_id` exists anywhere. Both fields are load-bearing for the misconfiguration-detection story ("I think I'm talking to prod but this is staging") and for future clients that route by broker. Ship the identity surface at the same time as the RPC that consumes it — one design conversation, one release.

## Design choices, with rationale

### Bundled release, not two sequential releases

Alternatives considered:
- **Ship 2.2 first (broker identity only), then 2.1 (Metadata).** Two smaller releases; two design conversations; delays the client-side surface that Phase 2.3 needs.
- **Ship 2.1 first without broker identity.** Metadata response returns brokers with placeholder `broker_id: "default"` or empty. Clients start caching a value that will change under them when 2.2 lands.

Bundling is cleanest: one schema decision, one release, one design conversation covers the whole surface.

### `cluster_id` is required config; `broker_id` is optional with disk-persisted auto-gen

Two-part choice.

**`cluster_id` is a required config field.** Startup fails with a clear error naming the field if it's missing. Rationale:
- `cluster_id` is human-facing safety machinery. Its whole purpose is detecting misconfiguration ("wrong cluster") — a UUID or auto-generated value defeats that purpose.
- Ops teams already name their clusters (`prod-east`, `staging-us-west`). Requiring a config value forces that name into the broker.
- Fail-fast startup matches the pattern established by `TopicRegistry::load`'s validation gate: broker refuses to boot in a state that would silently mislead operators later.

**`broker_id` is layered resolution.**
1. Config `broker.id = "..."` set → use it verbatim. Explicit operator control.
2. Otherwise `data_dir/broker_id` file exists → read it. Restart; identity survives.
3. Otherwise generate a fresh `brk-<8hex>` (12 characters, 32 bits of entropy), write it to `data_dir/broker_id`, use it. First-time boot.

Rejected alternatives:
- **Hostname as default.** Works fine for K8s StatefulSets (`kafkrs-broker-0` is stable across restarts of the same pod). Breaks for **ECS Fargate**, where every task restart gets a fresh hostname derived from the private IP. Fargate is an explicit target deployment; the design has to work there.
- **UUIDv7 for auto-gen.** Machine-friendly but ops-hostile — a 36-character UUID doesn't fit in dashboards, logs, or PagerDuty summaries cleanly. `brk-<8hex>` gives us the same "unique enough" property in 12 characters.
- **Auto-gen and persist `cluster_id` too.** Silently generating a cluster identity is the exact failure mode we're trying to prevent. Two brokers that were meant to be in the same cluster but each auto-generated their own `cluster_id` would look independent to clients — no error surfaced.

The disk-persisted `broker_id` file works uniformly across every target deployment:
- **K8s StatefulSet**: PV persists → `broker_id` file persists.
- **Fargate + EBS-per-task**: EBS reattaches → file persists.
- **Bare metal**: local disk → file persists.
- **Dev**: local disk → persists until you `rm -rf data/`, at which point a fresh identity is arguably correct (fresh disk = fresh broker).

### Metadata response schema, YAGNI applied

The schema:

```proto
message MetadataRequest {
  repeated string topics = 1;   // empty = all topics
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
  uint32 error_code                       = 2;
  string topic_uuid                       = 3;
  repeated PartitionMetadata partitions   = 4;
}

message PartitionMetadata {
  uint32 partition        = 1;
  string leader_broker_id = 2;
}
```

Explicit exclusions, each with a driver-appears trigger:
- **`controller_broker_id`** in `MetadataResponse` — Phase 3.1 (coordination substrate) will decide whether we even have a controller concept. Adding it now either forces the decision or ships a meaningless field.
- **`replicas` / `isr`** on `PartitionMetadata` — kafkrs's durability model routes through the object store, not follower replication. There is no per-partition replica set to advertise.
- **`rack`** on `BrokerInfo` — Phase 4.1 rack-aware placement is future.
- **`advertised_address` / `advertised_port`** in config — for single-broker 0.7.0 the bind address IS what clients reach. Phase 6 will introduce these as optional config overrides with the current fields as fallbacks. Additive.

All future additions are strictly additive wire changes (new field numbers, existing clients unaffected).

### Per-topic errors, not call-level errors

When the request specifies a topic filter and one topic is unknown, we return a full response with a per-topic error entry rather than failing the whole call:

```
TopicMetadata { topic: "not-a-topic", error_code: ERR_UNKNOWN_TOPIC, topic_uuid: "", partitions: [] }
```

Rejected alternatives:
- **All-or-nothing (any unknown topic → `ErrorResponse`).** Forces clients to retry with a filtered list, and to keep track of which topic caused the failure. Bad ergonomics for consumer-group clients that subscribe to a set of topics some of which may have been deleted since the group last committed.

Per-topic errors match Kafka's `TopicMetadata { error_code, ... }` shape, so consumer clients ported from Kafka can reuse muscle memory.

### Snapshot-based registry access, not per-topic Describe loops

The wire handler gets a single-shot registry snapshot via a new `RegistryMsg::Snapshot { reply: oneshot::Sender<Vec<TopicEntry>> }` message. Handler indexes by name, filters, de-dupes, builds the response.

Rejected alternatives:
- **Loop of Describes.** N+1 registry round trips for a filter of N topics. Fine for small N but wasteful and clumsy.
- **Registry does the filtering.** Registry actor stays focused on being the file-of-truth; presentation and per-topic error decoration are wire-layer concerns.

Snapshot returns cheap `TopicEntry` clones (name, uuid, partition_count, created_at_ns, config). For the current scale (tens of topics), this is nothing.

### `broker_id` file is write-once, `cluster_id` is never persisted

The `broker_id` file at `data_dir/broker_id` is written **only** during the auto-gen path (first-time boot with no config override). If `broker.id` is set in config on any subsequent boot, the disk file is not touched — not even to write a matching value. Rationale: config-set operators should never see a stale disk file appear later that suggests something happened at runtime.

`cluster_id` is deliberately not persisted to `data_dir`. It comes from config on every boot. Rationale: persisting would let a stale disk file silently override a legitimate config rename. If ops move a broker from `staging-us-west` to `prod-east` by editing config, they mean it.

## Architecture

No new modules of any significance. All changes localized:

```
kafkrs-models/proto/wire/v1.proto                ← Metadata + BrokerInfo + TopicMetadata + PartitionMetadata;
                                                    ConnectedResponse.cluster_id; reserved range updates
kafkrs-models/src/config.rs                      ← BrokerConfig gains `id: Option<String>`, `cluster_id: String`

kafkrs-server/src/broker_identity.rs (new, ~80 lines) ← resolve_identity() pure function + BrokerIdentity struct
kafkrs-server/src/topic_registry.rs              ← RegistryMsg::Snapshot variant + handler arm
kafkrs-server/src/wire/dispatch.rs               ← handle_metadata; SharedState carries BrokerIdentity;
                                                    handle_connected sources broker_id + cluster_id from identity
kafkrs-server/src/wire/connection.rs             ← Body::Metadata dispatch arm + "metadata" rpc label
kafkrs-server/src/main.rs                        ← Call resolve_identity() before wire listeners bind

kafkrs-python/kafkrs/client.py                   ← Client.get_metadata
kafkrs-python/kafkrs/wire/v1_pb2.py              ← Regenerated

kafkrs-models/CHANGELOG.md,
kafkrs-server/CHANGELOG.md,
kafkrs-python/CHANGELOG.md                       ← 0.7.0 entries (breaking config change flagged)
Cargo.toml files, __init__.py, Cargo.lock       ← 0.6.2 → 0.7.0
```

### Broker identity resolution

`kafkrs-server/src/broker_identity.rs` — a new small module:

```rust
#[derive(Clone)]
pub struct BrokerIdentity {
    pub broker_id: Arc<str>,
    pub cluster_id: Arc<str>,
    pub advertised_host: Arc<str>,
    pub advertised_port: u16,
}

pub enum IdentityError {
    MissingClusterId,
    IoError(String),
}

pub fn resolve_identity(
    cfg: &BrokerConfig,
    address: &str,
    wire_port: u16,
    data_dir: &Path,
) -> Result<BrokerIdentity, IdentityError> {
    let cluster_id = cfg.cluster_id.clone()
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
    std::fs::write(&path, &id)
        .map_err(|e| IdentityError::IoError(e.to_string()))?;
    Ok(id)
}

fn generate_broker_id() -> String {
    // "brk-<8hex>" using getrandom or thread_rng
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).unwrap();
    format!("brk-{:02x}{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2], bytes[3])
}
```

Called from `main.rs` before `accept_loop` starts. On `IdentityError::MissingClusterId`, panic with a clear message naming the field. On `IdentityError::IoError`, panic naming the file path.

### `SharedState` carries `BrokerIdentity`

Extend `SharedState` (in `wire/dispatch.rs`):

```rust
#[derive(Clone)]
pub struct SharedState {
    // ... existing fields ...
    pub identity: BrokerIdentity,
}
```

`Clone` is already required — `BrokerIdentity` is cheap to clone (three `Arc<str>` clones + a `u16`).

### `handle_connected` sources identity from state

The existing `pub const BROKER_ID: &str = "kafkrs-broker-v1"` in `dispatch.rs:39` goes away. `handle_connected` now takes `&SharedState` and reads:

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

The dispatcher already has `&SharedState` available; adding the parameter is a one-line callsite change.

### Registry snapshot message + handler

`RegistryMsg::Snapshot { reply: oneshot::Sender<Vec<TopicEntry>> }`. Handler in `TopicRegistry::run`:

```rust
RegistryMsg::Snapshot { reply } => {
    let _ = reply.send(self.topics.values().cloned().collect());
}
```

Trivial — single line inside the match. Registry stays focused on read/write of `topics.json`.

### `handle_metadata` orchestration

`handle_metadata` in `wire/dispatch.rs`:

1. Send `RegistryMsg::Snapshot`, await reply. Channel closed → `ErrBrokerNotReady`.
2. Build `HashMap<String, TopicEntry>` from the snapshot.
3. Compute the topic list to return:
   - Empty request filter → every entry in the map, arbitrary iteration order (client sorts if it cares).
   - Non-empty filter → walk `dedup(req.topics)`; matched entries return normally, unmatched return `TopicMetadata { topic: name, error_code: ERR_UNKNOWN_TOPIC, topic_uuid: "".into(), partitions: vec![] }`.
4. For each returned `TopicEntry`, expand `0..partition_count` into `PartitionMetadata { partition: i, leader_broker_id: state.identity.broker_id.to_string() }`.
5. Populate `brokers` with a single `BrokerInfo { broker_id, host: identity.advertised_host, port: identity.advertised_port as u32 }`.
6. Populate `cluster_id` from identity.
7. Return `MetadataResponse` wrapped in a `Frame`.

### Wire proto changes

`kafkrs-models/proto/wire/v1.proto`:

```proto
message Command {
  reserved 40 to 49;   // Streaming-consumer RPCs (Phase 2.3 consumer groups)
  reserved 56 to 59;   // Admin overflow
  reserved 60 to 79;   // Future RPC categories (multi-broker admin, group admin, etc.)

  uint64 correlation_id = 1;

  oneof body {
    // ... existing (unchanged) ...
    MetadataRequest    metadata      = 54;
    MetadataResponse   metadata_resp = 55;
    // ... existing Error = 99 ...
  }
}

message ConnectedResponse {
  uint32 protocol_version = 1;
  string broker_id        = 2;
  string cluster_id       = 3;
}

message MetadataRequest {
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
  uint32 error_code                       = 2;
  string topic_uuid                       = 3;
  repeated PartitionMetadata partitions   = 4;
}

message PartitionMetadata {
  uint32 partition        = 1;
  string leader_broker_id = 2;
}
```

The `[60, 79]` pre-reservation is documentation of intent — costs nothing and saves the next contributor from having to think about wire-format organization.

### Config surface

`kafkrs-models/src/config.rs`:

```rust
pub struct BrokerConfig {
    // ... existing (disk_type, auto_create_topics, default_partition_count, etc.) ...
    pub cluster_id: String,            // required; deserialization fails if missing
    pub id: Option<String>,            // optional; auto-gen + disk-persist if unset
}
```

Serde deserialization enforces `cluster_id` presence. The config file docs at the top of `config.toml` add:

```toml
[broker]
cluster_id = "prod-east"
# id = "broker-1"   # optional; auto-generated as brk-<8hex> and persisted to data/broker_id if unset
```

### Python client

```python
async def get_metadata(
    self,
    topics: Optional[List[str]] = None,
) -> v1_pb2.MetadataResponse:
```

Returns the raw proto response. Matches the pattern used by `alter_topic_config`. Caller iterates `resp.brokers`, `resp.topics`, `topic.partitions` as proto attributes.

## Impact on existing code

| Location | Change |
| --- | --- |
| `kafkrs-models/proto/wire/v1.proto` | Add `MetadataRequest`/`Response`, `BrokerInfo`, `TopicMetadata`, `PartitionMetadata` at 54/55; add `ConnectedResponse.cluster_id = 3`; shrink admin reserved to `[56, 59]`; pre-reserve `[60, 79]`. |
| `kafkrs-models/src/config.rs` | `BrokerConfig` gains `cluster_id: String` (required) and `id: Option<String>` (optional). Unit tests for both. |
| `kafkrs-server/src/broker_identity.rs` | New module: `BrokerIdentity` struct, `resolve_identity()` pure function, disk file at `data_dir/broker_id`. Unit tests. |
| `kafkrs-server/src/topic_registry.rs` | `RegistryMsg::Snapshot` variant + one-line handler arm. Two unit tests. |
| `kafkrs-server/src/wire/dispatch.rs` | `SharedState` gains `identity: BrokerIdentity`. `handle_connected` sources `broker_id` + `cluster_id` from identity. New `handle_metadata`. Removes hard-coded `pub const BROKER_ID`. |
| `kafkrs-server/src/wire/connection.rs` | Dispatch arm `Body::Metadata(req) => handle_metadata(...)`; `"metadata"` label for `WIRE_RPC_REQUESTS`; `handle_connected` callsite updated to pass `&state`. |
| `kafkrs-server/src/main.rs` | Call `resolve_identity()` before wire listeners bind; wrap result in `SharedState`. |
| `kafkrs-server/tests/wire_e2e.rs` | 5 new tests covering Connect identity, Metadata unfiltered, filtered, unknown-topic per-topic error, per-partition leader. |
| `kafkrs-python/kafkrs/client.py` | `Client.get_metadata`. |
| `kafkrs-python/kafkrs/wire/v1_pb2.py` | Regenerated. |
| `kafkrs-python/tests/test_client.py` | 4 new tests. |
| Cargo.toml + pyproject.toml + __init__.py + README.md | `0.6.2` → `0.7.0`. README status line bumped. |
| CHANGELOG.md (×3) | `0.7.0` entries with a **Breaking changes** section flagging `broker.cluster_id` as newly required. |

No new external dependencies. `getrandom` for the auto-gen path is already a transitive dep (via `uuid`).

## Versioning

Bump all three crates from `0.6.2` → `0.7.0` in lockstep. Minor bump, not patch, for one specific reason: **`broker.cluster_id` is a required config field that didn't exist before**. Every deployment upgrading past 0.6.x will fail to start until `cluster_id` is added to `config.toml`. That's a breaking operator change even though every wire-protocol change is strictly additive.

CHANGELOG entries call this out under a `## Breaking changes` heading in the server and models CHANGELOGs.

Wire protocol stays at v1. Every proto edit is additive:
- New field on `ConnectedResponse` (field 3, unknown to old clients — they ignore it).
- New oneof arms `metadata` and `metadata_resp` at 54/55.
- Five new message types.
- Reserved range shrunk from `[54, 59]` to `[56, 59]` (exemption already in `buf.yaml` from prior work); new pre-reservation of `[60, 79]`.

## Test plan

### `kafkrs-models` unit tests (`config.rs`)

- `parse_config_with_broker_id_and_cluster_id`.
- `parse_config_with_cluster_id_only` — `id` defaults to `None`.
- `parse_config_missing_cluster_id_fails` — expects a serde error naming the field.

### Broker identity resolution (`kafkrs-server/src/broker_identity.rs`)

- `resolve_uses_config_id_when_set` — config `id = "broker-1"` beats an existing disk file.
- `resolve_reads_persisted_file_when_config_id_unset` — writes a file, calls resolve, gets back the file's content.
- `resolve_generates_and_persists_on_first_boot` — no config, no file → returns `brk-<hex>` and file exists with matching content.
- `resolve_config_id_wins_over_disk_file` — with both present, config wins; disk file left untouched (assert unchanged mtime or byte-identical).
- `resolve_fails_when_cluster_id_missing` — returns `Err(MissingClusterId)`.
- `generate_broker_id_matches_brk_prefix_format` — regex `^brk-[0-9a-f]{8}$`.

### Registry tests (`kafkrs-server/src/topic_registry.rs`)

- `snapshot_returns_all_topics_in_map`.
- `snapshot_returns_empty_when_registry_empty`.

### Integration tests (`kafkrs-server/tests/wire_e2e.rs`)

- `connect_response_carries_broker_id_and_cluster_id_from_config`.
- `metadata_empty_filter_returns_all_topics_and_self_broker`.
- `metadata_filter_returns_only_requested_topics`.
- `metadata_filter_with_unknown_topic_returns_per_topic_error_and_populated_others` — mixed request: one exists, one doesn't; assert `error_code == ERR_UNKNOWN_TOPIC` on the missing entry and full population on the existing entry.
- `metadata_partition_metadata_lists_self_as_leader_for_every_partition` — topic with `partition_count = 3` produces three `PartitionMetadata { leader_broker_id: <this-broker> }` entries.

### Python integration tests (`kafkrs-python/tests/test_client.py`)

- `test_get_metadata_returns_broker_and_topic_info`.
- `test_get_metadata_filter_returns_subset`.
- `test_get_metadata_unknown_topic_has_per_topic_error`.
- `test_connect_populates_cluster_id_on_response` — confirms the identity leg reaches Python.

### Manual smoke

Start the broker with a fresh `data/` dir; observe `data/broker_id` gets created with a `brk-<hex>` line; restart the broker; observe the same identity in `ConnectedResponse` on both boots. Change `broker.cluster_id` in config, restart, observe the new value in the response.

## Out of scope

### Deferred (add when a driver appears)

- **`controller_broker_id` in `MetadataResponse`.** Phase 3.1 (coordination substrate) decision determines whether the concept applies.
- **`replicas` / `isr` on `PartitionMetadata`.** Only if we ever adopt follower-based WAL replication as an alternative durability mode (unlikely given the object-store model).
- **`rack` on `BrokerInfo`.** Phase 4.1 rack-aware placement.
- **`broker.advertised_address` / `broker.advertised_port` config.** Phase 6 when clients need to reach a different address than the bind address.
- **Metadata push notifications / cache invalidation.** Pull-only; client re-queries.
- **Cluster-id-mismatch handling in the Python client.** Additive when someone hits it. Server just returns `cluster_id`; comparison is client-side.
- **Rate-limiting / caching on the Metadata handler.** No cardinality driver yet.

### Not in scope at all

- **Real multi-broker routing.** Phase 6.
- **Auto-detecting broker address from container metadata** (ECS Task metadata, GKE metadata server, etc.). Explicit config only.
- **Broker discovery protocol** (DNS SRV, cloud-native discovery). Phase 3.3.
- **Client-side connection pooling** in the Python client.

## Invariants (for implementers)

1. **Broker identity is immutable after boot.** `BrokerIdentity` is set once by `resolve_identity()` and cloned (via `Arc<str>`) for every read; never mutated.
2. **`data_dir/broker_id` is write-once.** Written only in the auto-gen path (first boot). If `broker.id` is set in config on any subsequent boot, the file is not touched — not even to write a matching value.
3. **`cluster_id` is never persisted to `data_dir`.** Comes from config on every boot. Config rename is the only path to change it.
4. **Metadata handler is strictly read-only.** No auto-create, no state mutation, no side effects. Even under `auto_create_topics = true`, Metadata never creates a topic.
5. **Every `TopicMetadata` entry with `error_code != 0` has empty `partitions` and empty `topic_uuid`.** Client can distinguish the error case unambiguously.
6. **`BrokerInfo.host` and `BrokerInfo.port` are the broker's bind address for 0.7.0.** Phase 6 will introduce `advertised_address` as an additive config override.
7. **Cluster_id-mismatch detection is client-side.** Server returns `cluster_id`; comparison happens in the client.
8. **Missing `broker.cluster_id` in config causes fail-fast startup.** No default, no auto-generation, no boot-with-empty-string. Matches the pattern established by `TopicRegistry::load`'s validation gate.
