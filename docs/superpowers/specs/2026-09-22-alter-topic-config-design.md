# AlterTopicConfig — Design

**Status:** Draft for review
**Date:** 2026-09-22
**Scope:** An `AlterTopicConfig` admin RPC that mutates a topic's per-topic configuration overrides at runtime, without a broker restart, without disrupting in-flight batches. Partial-patch semantics, propagated to running actors via a new `UpdateConfig` message on the existing actor channels. Also introduces a `TopicConfigOverrides::validate()` gate used at `CreateTopic`, `AlterTopicConfig`, and broker startup.

## Motivation

Retention (0.4.0), metrics (0.5.0), and DeleteTopic (0.6.0) all entrenched the pattern that a topic's config is immutable after `CreateTopic`. Tuning `retention_ms`, `metrics_high_cardinality` (topic scope isn't applicable but the pattern generalises), or any of the group-commit knobs today requires a broker restart. That's the biggest ergonomics gap in the current admin story.

Beyond ergonomics, this feature is a load-bearing prerequisite on the roadmap to multi-broker. The multi-broker era needs a way for a controller to push config changes to per-broker actors; landing the "config change → running actor" propagation pattern in single-broker means the multi-broker work reshapes only the *cluster coordination* around this pattern, not the per-broker plumbing itself.

## Design choices, with rationale

### Partial-patch semantics, not full-replace

`AlterTopicConfigRequest` carries a `TopicConfigOverrides` message. Fields set to `Some(x)` overwrite the corresponding field in the stored config; fields left `None` are unchanged. Matches Kafka's `AlterConfigs` shape and the common ops pattern "just tweak `retention_ms`, leave the rest."

Rejected alternatives:
- **Full replace.** Caller sends the full current config every time. Races if two operators try to change different fields concurrently; each overwrites the other's field.
- **Partial patch + explicit unset sentinel.** Reserved values or a separate `unset: repeated string` field to explicitly revert to broker defaults. More expressive; more surface area. Deferred until a driver appears — for now, if you want to revert a field to broker default, you have to know what that default is and set the patch to that value.

### Push messages via existing mpsc channels, not shared atomic swap

The registry actor is the merge-and-persist authority. Once persisted, the wire handler sends `PwMsg::UpdateConfig(new_resolved)` and `UploaderMsg::UpdateConfig(new_resolved)` to each partition's actors, and updates `PartitionHandle.cfg` under the partitions `RwLock`.

Rejected alternatives:
- **Shared atomic swap** (e.g. `Arc<ArcSwap<ResolvedTopicConfig>>`). Elegant in a shared-state sense, but breaks the "actor owns its config" invariant that every other subsystem relies on. Also requires adding `arc-swap` as a dependency.
- **Hybrid** — messages for actors, atomic for `PartitionHandle`. Two propagation paths to maintain; no clear win over pure message-passing.

Push messages have three properties worth calling out:
- **Deterministic mid-batch semantics.** FIFO dequeue means the actor processes any in-flight `Upload` or seal before it processes `UpdateConfig`. In-flight batches complete under the old config; the next batch uses the new config.
- **Matches the existing pattern.** `PwMsg::Shutdown`, `UploaderMsg::Shutdown`, `RetentionKick` all use the same shape.
- **Translates cleanly to multi-broker.** When the future controller lands, it converts cluster-wide alter into per-broker `UpdateConfig` messages the same way.

### Validation at three sites, not just at Alter time

`TopicConfigOverrides::validate()` is a pure function on the model. It's called from:

1. **`CreateTopic`** — reject invalid overrides at topic creation.
2. **`AlterTopicConfig`** — validate the *merged* result (patch composed onto current stored config) before persisting.
3. **`TopicRegistry::load`** on broker startup — validate each `TopicEntry.config` when parsing `topics.json`.

Startup validation is fail-fast: a broker refusing to start with a clear error naming the topic and field is better than the current behaviour (accept anything, then enter an infinite seal loop at runtime if `segment_size_bytes = 0`).

The startup gate is technically breaking for any operator whose current `topics.json` contains an out-of-range value. Given the constraints below, that broker is already broken; failing fast at startup is the correct fix.

### Constraints applied by `validate()`

Realistic-only. Everything else has a legitimate "0 is a thing" or "any positive is fine" reading.

- `segment_size_bytes >= 1` — 0 would cause the writer to seal every record then re-seal instantly.
- `segment_seal_time_ms >= 1` — 0 would seal after every record, defeating the seal-by-time semantic.
- `retention_ms >= -1` — either the sentinel `-1` for "never" or a positive value. Other negatives are meaningless.
- `retention_bytes >= -1` — same sentinel semantics.

Not constrained:
- `max_key_size_bytes`, `max_value_size_bytes` — `0` means "reject all records with a non-empty key/value" which is a legitimate lockdown setting.
- `group_commit_time_ms`, `group_commit_size_bytes`, `group_commit_record_count` — `0` on any of these means "commit as fast as possible on that dimension" which is a real low-latency setting.
- `max_fetch_wait_ms` — `0` means "no long-poll", fine.

### Persist-first, notify-second ordering

The registry merges the patch into the current config, validates the merged result, persists `topics.json` atomically (`.tmp` + fsync + rename), and only *then* returns success to the wire handler. The wire handler then pushes `UpdateConfig` messages and updates `PartitionHandle.cfg`.

A crash between persist and notify is self-healing: on the next broker restart, `spawn_partition` reads the new config from `topics.json` and spawns actors with it. A crash between notify and persist (the reverse order) would leave actors briefly running with a config that reverts on restart. That's why the reverse order is forbidden.

### One topic per RPC (batching deferred)

Kafka's `AlterConfigs` accepts multiple resources per RPC with per-resource results. It's a real win at scale (fewer round trips, one `topics.json` write instead of N) but adds partial-failure semantics that are easy to design wrong (Kafka itself has two versions).

kafkrs today has tens of topics, not thousands. A loop of single-topic RPCs from the client covers it. If a driver appears, we'll add a new `AlterTopicConfigsBatch` RPC at a later field number — additive, no breaking change. Same call the codebase made for `DeleteTopic`.

## Architecture

No new modules. All changes localized to existing files:

```
kafkrs-models/proto/wire/v1.proto        ← AlterTopicConfigRequest/Response at 52/53; ERR_INVALID_CONFIG = 207
kafkrs-models/src/topic.rs               ← TopicConfigOverrides::validate() + ConfigValidationError

kafkrs-server/src/topic_registry.rs      ← RegistryMsg::Alter + handler; startup validation gate; CreateTopic gains validation
kafkrs-server/src/partition_writer.rs    ← PwMsg::UpdateConfig(ResolvedTopicConfig); run() match arm
kafkrs-server/src/uploader.rs            ← UploaderMsg::UpdateConfig(ResolvedTopicConfig); run() match arm
kafkrs-server/src/wire/dispatch.rs       ← handle_alter_topic_config
kafkrs-server/src/wire/connection.rs     ← Body::AlterTopicConfig dispatch arm + "alter_topic_config" rpc label
kafkrs-server/src/wire/errors.rs         ← ConfigValidationError → ErrInvalidConfig mapping

kafkrs-python/kafkrs/client.py           ← Client.alter_topic_config
kafkrs-python/kafkrs/wire/v1_pb2.py      ← regenerated
kafkrs-python/tests/test_client.py       ← round-trip + error cases

kafkrs-models/CHANGELOG.md, kafkrs-server/CHANGELOG.md, kafkrs-python/CHANGELOG.md
Cargo.toml files, __init__.py, Cargo.lock  ← 0.6.1 → 0.6.2
```

### Registry message + handler flow

`RegistryMsg` gains:

```rust
Alter {
    name: String,
    patch: TopicConfigOverrides,
    reply: oneshot::Sender<Result<TopicConfigOverrides, RegistryError>>,
}
```

`RegistryError` gains:

```rust
InvalidConfig(String),   // holds the ConfigValidationError's Display message
```

Handler flow inside `TopicRegistry::run`:

1. Look up entry by name → `Err(UnknownTopic)` if absent.
2. Clone the current `entry.config` into a working copy `merged`. For each `Some(x)` field in `patch`, overwrite the corresponding field in `merged`. Fields set to `None` in `patch` stay unchanged.
3. Call `merged.validate()`. On failure: don't touch in-memory state, return `Err(InvalidConfig(err.to_string()))`.
4. Swap `entry.config = merged.clone()` in-memory.
5. Persist `topics.json` atomically. On IO failure: revert the in-memory swap, return `Err(Io(msg))`.
6. Reply `Ok(merged)`.

Steps 4-5 could rollback on failure by keeping the old config in a local variable. The registry actor is single-threaded so no lock is needed around the rollback.

### Wire handler orchestration

`handle_alter_topic_config` in `wire/dispatch.rs`:

1. Extract `patch` from the request (proto → model conversion via existing `wire_overrides_to_model`).
2. Send `RegistryMsg::Alter { name, patch, reply }`, await reply.
3. Map errors:
   - `Ok(merged)` → continue.
   - `Err(UnknownTopic)` → `ERR_UNKNOWN_TOPIC`.
   - `Err(InvalidConfig(msg))` → `ERR_INVALID_CONFIG`.
   - `Err(Io(_))` → `ERR_INTERNAL`.
4. Resolve the merged overrides into a `ResolvedTopicConfig` using `state.disk_type`.
5. Walk the topic's partitions in `state.partitions`. For each `PartitionHandle`:
   - Send `PwMsg::UpdateConfig(new_resolved)` fire-and-forget.
   - Send `UploaderMsg::UpdateConfig(new_resolved)` fire-and-forget.
6. Acquire `state.partitions.write().await`, walk the partitions again, replace `handle.cfg = new_resolved` on each.
7. Return `AlterTopicConfigResponse { overrides: model_overrides_to_wire(merged) }`.

The two "walk partitions" loops (step 5 read snapshot, step 6 write mutation) are separated so the read-lock isn't held across the mpsc sends. Any partition that materializes concurrently (auto-create race) picks up the new config on spawn from the already-persisted `topics.json`; any that's torn down concurrently (DeleteTopic race) fails the send silently and doesn't matter.

### Actor message handlers

`PwMsg::UpdateConfig(new_cfg)` in `PartitionWriter::run`'s match:

```rust
PwMsg::UpdateConfig(new_cfg) => {
    self.cfg = new_cfg;
    // Continue the run loop. Next batch will use new group_commit_*,
    // next seal check will use new segment_size_bytes. In-flight batch
    // stays with the old cfg (already-computed thresholds in the
    // current commit_after_delay / seal path).
}
```

`UploaderMsg::UpdateConfig(new_cfg)` in `Uploader::run`'s match:

```rust
UploaderMsg::UpdateConfig(new_cfg) => {
    self.cfg = new_cfg;
    // Next retention_pass reads new retention_ms / retention_bytes.
}
```

Both arms are one-liners: just replace the field. No draining, no acking.

### Validation surface

`kafkrs-models/src/topic.rs`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValidationError {
    FieldOutOfRange {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
}

impl std::fmt::Display for ConfigValidationError { /* ... */ }
impl std::error::Error for ConfigValidationError {}

impl TopicConfigOverrides {
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        // segment_size_bytes >= 1
        // segment_seal_time_ms >= 1
        // retention_ms >= -1
        // retention_bytes >= -1
    }
}
```

Called from:
- `RegistryMsg::Create` handler — validate initial overrides.
- `RegistryMsg::Alter` handler — validate merged overrides (before persist).
- `TopicRegistry::load` — validate each `TopicEntry.config` when reading `topics.json`; broker startup fails with a `panic!` naming the topic + field.

### Wire proto changes

`kafkrs-models/proto/wire/v1.proto`:

```proto
message Command {
  // ... existing fields 20-51 ...
  oneof body {
    AlterTopicConfigRequest   alter_topic_config      = 52;
    AlterTopicConfigResponse  alter_topic_config_resp = 53;
  }
  reserved 54 to 59;   // was: reserved 52 to 59;
}

message AlterTopicConfigRequest {
  string topic = 1;
  TopicConfigOverrides overrides = 2;
}

message AlterTopicConfigResponse {
  TopicConfigOverrides overrides = 1;   // merged result post-patch
}

enum ErrorCode {
  // ... existing 200-206 ...
  ERR_INVALID_CONFIG = 207;
}
```

### Python client

`Client.alter_topic_config(name: str, overrides: TopicConfigOverrides) -> TopicConfigOverrides`. Same shape as `create_topic` — request/response with proto types, raises `WireError` on error codes. Regenerated `v1_pb2.py`.

## Impact on existing code

| Location | Change |
| --- | --- |
| `kafkrs-models/proto/wire/v1.proto` | Add `AlterTopicConfigRequest`/`Response` at 52/53; `ERR_INVALID_CONFIG = 207`; shrink `reserved` to `54 to 59`. |
| `kafkrs-models/src/topic.rs` | `TopicConfigOverrides::validate()`; `ConfigValidationError`. Extensive unit tests. |
| `kafkrs-server/src/topic_registry.rs` | `RegistryMsg::Alter`; `RegistryError::InvalidConfig`; handler; call `validate()` from Create + Alter + `load()`. |
| `kafkrs-server/src/partition_writer.rs` | `PwMsg::UpdateConfig(ResolvedTopicConfig)` variant + one-line handler arm. |
| `kafkrs-server/src/uploader.rs` | `UploaderMsg::UpdateConfig(ResolvedTopicConfig)` variant + one-line handler arm. |
| `kafkrs-server/src/wire/dispatch.rs` | `handle_alter_topic_config`. |
| `kafkrs-server/src/wire/connection.rs` | Dispatch arm + `"alter_topic_config"` label for `WIRE_RPC_REQUESTS`. |
| `kafkrs-server/src/wire/errors.rs` | Map `RegistryError::InvalidConfig` → `ErrorCode::ErrInvalidConfig`. |
| `kafkrs-server/tests/wire_e2e.rs` | 4 new integration tests (round-trip, invalid rejection, unknown topic, partial patch). |
| `kafkrs-python/kafkrs/client.py` | `Client.alter_topic_config`. |
| `kafkrs-python/kafkrs/wire/v1_pb2.py` | Regenerated. |
| `kafkrs-python/tests/test_client.py` | 3 new tests. |
| Cargo.toml + pyproject.toml + __init__.py | `0.6.1` → `0.6.2`. |
| CHANGELOG.md (×3) | `0.6.2` entries. |

No new modules. No breaking changes to wire protocol version (v1 still). No breaking on-disk format changes. The only behaviour breakage is the new startup-validation gate, which only affects operators whose `topics.json` was already in a broken state.

## Versioning

Bump all three crates from `0.6.1` to `0.6.2` in lockstep.

- **Additive proto changes** — no version bump forced by the wire protocol.
- **Additive on-disk format** — `topics.json` schema unchanged; existing files parse identically.
- **New feature.** Patch bump is defensible: no breaking changes, purely additive behaviour with a fail-fast startup gate that only affects already-broken configs.

## Test plan

### Unit tests (`kafkrs-models/src/topic.rs`)

- `validate_accepts_defaults` — `TopicConfigOverrides::default().validate()` is `Ok`.
- `validate_rejects_segment_size_zero`.
- `validate_rejects_segment_seal_time_zero`.
- `validate_rejects_retention_ms_below_minus_one`.
- `validate_rejects_retention_bytes_below_minus_one`.
- `validate_accepts_valid_full_config` — every field populated with a valid value.
- `validate_accepts_retention_ms_negative_one_sentinel`.
- `validate_accepts_retention_bytes_negative_one_sentinel`.

### Registry tests (`kafkrs-server/src/topic_registry.rs`)

- `alter_unknown_topic_returns_unknown_topic`.
- `alter_invalid_patch_returns_invalid_config_and_leaves_state_unchanged` — send an alter with `segment_size_bytes = Some(0)`; expect `Err(InvalidConfig(_))`. Re-read `topics.json` and confirm on-disk config is untouched. `Describe` returns the unchanged config.
- `alter_valid_patch_persists_and_returns_merged_overrides` — apply a patch changing `retention_ms`; expect `Ok(merged)`. Re-read `topics.json`; confirm new value present, every other field unchanged.
- `alter_second_patch_composes_on_first` — two sequential patches to different fields; both take effect.
- `load_rejects_invalid_topics_json_at_startup` — pre-write `topics.json` with `segment_size_bytes = 0`; `TopicRegistry::load` returns `Err` (or panics per the chosen error-handling); test asserts the message contains the field name.

### Integration tests (`kafkrs-server/tests/wire_e2e.rs`)

- `alter_topic_config_updates_uploader_retention` — the load-bearing behavioural test:
  1. Broker up with default retention (`24h`); topic created without overrides.
  2. Produce enough records to seal at least one segment; wait for upload.
  3. Alter to `retention_ms = 200`.
  4. Wait ~1s past the new watermark plus sweeper interval.
  5. Fetch from offset 0 → expect `ErrOffsetOutOfRange` (old segment evicted under new retention).
  6. `Describe` returns overrides showing `retention_ms = 200`.
- `alter_topic_config_rejects_invalid_and_leaves_state_unchanged`.
- `alter_topic_config_unknown_topic_returns_err_unknown_topic`.
- `alter_topic_config_respects_partial_patch` — alter with only `retention_ms`; describe and confirm every other override field is unchanged.

### Python integration tests (`kafkrs-python/tests/test_client.py`)

- `test_alter_topic_config_round_trip` — create, alter `retention_ms`, assert response overrides show the new value.
- `test_alter_topic_config_unknown_raises` — expect `WireError(200)`.
- `test_alter_topic_config_invalid_value_raises` — `segment_size_bytes = 0`; expect `WireError(207)`.

### Manual smoke

Same shape as retention/delete smoke tests: create → produce → alter with tight retention → wait → fetch → expect `ErrOffsetOutOfRange`.

## Out of scope

### Deferred to future work
- **Explicit "revert to broker default" mechanism.** Add if a driver appears; new `unset: repeated string` wire field is additive.
- **Batched multi-topic alter.** New RPC at a later field number if operators need it.
- **Dry-run mode.**
- **Retention retro-apply throttling.** No rate-limit on the eviction sweep after shortening retention. If it becomes a driver, throttle in the retention sweeper.
- **Auditing / change log.** Piggybacks on future auth work.
- **Broker-level `AlterConfig`.** Changing broker defaults still requires restart. Different design.

### Not in scope at all
- **Multi-broker coordination.** Single-broker only. When multi-broker lands, the future controller will translate cluster-wide alter into per-broker `UpdateConfig` messages using this feature's pattern.
- **Per-partition config.** Only per-topic.
- **Altering `partition_count`.** Structurally a different feature — spinning up new partition actors, redistributing data. Not this spec.

## Invariants (for implementers)

1. **Persistence-first ordering.** `topics.json` is fsync'd with the merged config BEFORE any `UpdateConfig` message is pushed. A crash between persist and push self-heals on restart. The reverse order is forbidden.
2. **Partial-patch semantics.** `Some(x)` overwrites; `None` is unchanged. No v1 mechanism to revert-to-default.
3. **Validation runs at three sites** — `CreateTopic`, `AlterTopicConfig`, and startup — using the same `TopicConfigOverrides::validate()` pure function.
4. **In-flight batches complete under the old config.** `PwMsg::UpdateConfig` is FIFO-ordered with other messages; the next batch after that message uses the new config.
5. **Push message failures are best-effort.** A closed mpsc is dropped silently; the RPC still succeeds because persistent state is already correct.
6. **`AlterTopicConfig` never mutates `partition_count`.** That field is on `TopicEntry`, unreachable via this RPC.
7. **The wire response returns the merged `TopicConfigOverrides`, not the fully-resolved config.** Clients see what's stored, not what broker defaults filled in.
