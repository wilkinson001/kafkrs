# DeleteTopic — Design

**Status:** Draft for review
**Date:** 2026-09-22
**Scope:** A `DeleteTopic` admin RPC using mark-and-sweep semantics, a per-delete ephemeral cleanup task, restart-safe pending state, and a baked-in UUIDv7 per topic so recreation under the same name is immediate and race-free. Also bundled: a breaking on-disk layout change adding `v=<uuid>/` to every object-store key.

## Motivation

kafkrs has no way to delete a topic. Every topic created — deliberately or via auto-create — persists forever, consuming local disk (WAL directory) and object-store storage (Parquet segments + manifests). Retention (0.4.0) closed the disk-cost bleed for *live* topics; `DeleteTopic` closes it for retired ones. The wire protocol has explicitly reserved 50-59 for admin RPCs since 0.1; this feature is the first occupant.

Beyond disk reclamation, `DeleteTopic` unblocks:
- The `spawn_locks` never-remove policy noted in memory — cleanup lands with topic deletion, as originally deferred.
- Retention's accepted v1 orphan-reclamation limitation — `DeleteTopic`'s sweep does a one-shot LIST of the topic's UUID prefix and catches orphans that retention's manifest-only approach never sees.
- The rest of the admin RPC namespace — `AlterConfig` and future admin operations will follow the same shape as this feature.

## Design choices, with rationale

### Mark-and-sweep with fast client response, not fully synchronous

Rejected alternatives:
- **Fully synchronous delete.** The client blocks until every WAL file, every Parquet segment, and every manifest is gone. Simple contract; scales badly. On S3 a topic with 10 000 segments hits per-prefix DELETE rate limits — synchronous wall time in the minutes range, during which a wire connection is hung and the client faces timeout/retry ambiguity.
- **Registry-only removal with a separate `kafkrs-cli purge` step.** Fastest response of all, but leaves orphaned data indefinitely until an operator remembers to run the cleanup. Trades short-term simplicity for permanent ops debt — the exact problem retention was built to solve resurfaces.

Chosen: mark-and-sweep. The registry entry is removed and partition actors are shut down synchronously (~hundreds of ms), then the client's `DeleteTopic` returns. A per-delete `tokio::spawn` task cleans up local WAL directory and object-store objects in the background. Client sees a fast response; storage cleanup happens at whatever pace the object store allows. Restart-safe via `data/pending_deletes.json`.

### Optional `delete_data: false` for detach semantics

`DeleteTopicRequest.delete_data: optional bool` (default `true`).

- **`delete_data = true` (default):** full mark-and-sweep. Registry removal + actor shutdown + WAL directory removal happen synchronously; object-store cleanup happens in the background sweep task.
- **`delete_data = false`:** "detach". Registry entry removed + actors shut down + `spawn_locks` cleaned — but WAL files and object-store data are left untouched for the user to handle out-of-band. No sweep task spawned. No `pending_deletes.json` entry.

Real use cases for `delete_data = false`: archive/audit retention where the raw data must remain for compliance, rename-under-new-name while keeping the old data browsable, migration to another system, cost-controlled S3 lifecycle policy management by the operator, or a recovery hedge for operators who don't fully trust the sweep code yet.

### UUIDv7 baked into every topic's object-store prefix

Every topic gets a UUIDv7 at `CreateTopic` time, stored in `topics.json`, and included in every object-store key. Path shape changes from `orders/partition=N/…` to `orders/v=<uuid>/partition=N/…`.

Rejected alternatives:
- **Block topic recreation until sweep completes.** Simple, but a stuck sweep (S3 outage during delete) blocks legitimate re-creation indefinitely until operator intervention.
- **Rename at delete time** (`orders/` → `_deleted/<uuid>/orders/`). Renames on S3 are COPY + DELETE per object; 20 000 API calls for a topic with 10 000 segments. Kills the fast-response property. Also puts the rename in the critical path before recreation is safe.

Chosen: bake the UUID into every topic's prefix from creation. Zero cost at delete time — the sweeper deletes the retired UUID's prefix; a recreated topic gets a new UUID and writes to a disjoint prefix. No rename, no coordination. This is what Kafka does (`topic_id` UUID). UUIDv7 specifically for the time-ordering benefit: object-store LIST output for a recreated topic name is chronologically sorted, useful for debugging and orphan inspection.

### No migration for existing 0.5.0 topics

Rejected: dual-layout code that supports both `orders/partition=N/…` and `orders/v=<uuid>/partition=N/…` simultaneously; startup migration that rewrites every existing topic's keys.

Chosen: no migration. kafkrs has no users yet, so no data to protect. 0.6.0 is a breaking release; any pre-existing `topics.json` and `data_dir` must be discarded before upgrading. Announced clearly in the changelog. Single clean layout going forward.

### Per-delete ephemeral task, not a broker-wide sweeper actor

Rejected alternatives:
- **Broker-wide `DeletionSweeper` actor with a queue.** Natural home for global rate-limiting but adds a long-lived actor for work that's bounded per-topic.
- **Extend the existing `RetentionSweeper`.** Conflates two different concerns (per-partition retention vs retired-topic deletion). Wrong fit.

Chosen: one `tokio::spawn(sweep_deletion(record))` task per pending delete. State persists to `data/pending_deletes.json` so a broker restart replays every unfinished record. Simplest structure; deletion work is bounded per-topic (not ongoing), so an ephemeral task fits the shape. Global rate-limiting is a future concern; when needed, migrate to the actor variant.

### Segments-first ordering in the sweep

Retention uses manifest-first-then-DELETE ordering to prevent a fetch from ever seeing a manifest that references a deleted segment. For **deletion**, the topic is going away entirely — there is no consumer to see any intermediate state. The sweep can safely delete segments first, then the manifest, then LIST-and-sweep any orphans. A mid-sweep crash leaves partial data plus a manifest; a resumed sweep re-walks the snapshot and completes idempotently.

### The one-shot LIST during sweep

Deletion is the only place in the broker that lists the object store. The storage spec's "no LIST on hot path" invariant is preserved — deletion is not a hot path. The LIST catches orphan segments from prior crashes (Uploader PUT succeeded but manifest update crashed) that retention's manifest-only sweep can never find. Retention's "accepted v1 orphan-reclamation limitation" gets closed here for topics that are deleted.

## Architecture

```
kafkrs-models/proto/wire/v1.proto           ← DeleteTopicRequest/Response at fields 50-51
kafkrs-models/src/topic.rs                  ← TopicEntry gains uuid: String
kafkrs-models/Cargo.toml                    ← + uuid = { version = "1", features = ["v7", "serde"] }

kafkrs-server/src/topic_registry.rs         ← RegistryMsg::DeleteTopic; handler
kafkrs-server/src/deletion.rs               ← NEW: sweep_deletion function + PendingDelete type
kafkrs-server/src/pending_deletes.rs        ← NEW: append/remove/load_all for data/pending_deletes.json
kafkrs-server/src/object_store.rs           ← segment_key / manifest_key gain topic_uuid parameter
kafkrs-server/src/metrics.rs                ← 5 new metric constants + describes
kafkrs-server/src/lib.rs                    ← pub mod deletion; pub mod pending_deletes;
kafkrs-server/src/main.rs                   ← startup replay of pending_deletes
kafkrs-server/src/startup.rs                ← spawn_partition threads uuid through
kafkrs-server/src/partition_writer.rs       ← accept and hold topic_uuid
kafkrs-server/src/uploader.rs               ← accept and hold topic_uuid; use in key construction
kafkrs-server/src/fetcher.rs                ← use topic_uuid when building keys
kafkrs-server/src/wire/dispatch.rs          ← handle_delete_topic + PartitionHandle carries uuid
kafkrs-server/src/wire/connection.rs        ← dispatch match arm gets a Delete case

kafkrs-python/kafkrs/__init__.py            ← Client.delete_topic(name, delete_data=True)
kafkrs-python/kafkrs/wire/v1_pb2.py         ← regenerated

kafkrs-models/CHANGELOG.md, kafkrs-server/CHANGELOG.md, kafkrs-python/CHANGELOG.md
Cargo.toml files, __init__.py, Cargo.lock  ← 0.5.0 → 0.6.0
```

### Delete request flow (in-process)

1. Wire dispatch receives `Command::DeleteTopic(req)`. Handler sends `RegistryMsg::DeleteTopic { topic, delete_data, respond }` and awaits the oneshot.
2. Registry handler:
   1. Look up the topic. If absent → respond `Err(UnknownTopic)`; wire layer returns `ERR_UNKNOWN_TOPIC`.
   2. Capture `uuid` and `partition_count` from the entry.
   3. Remove entry from in-memory map. Persist `topics.json` with `.tmp` + rename + fsync. **This is the atomic point.**
   4. For each partition `0..partition_count`:
      - Send `PwMsg::Shutdown` (new variant) to the `PartitionWriter`. Wait on a oneshot for confirmation of clean exit — this ensures no more segments will be uploaded.
      - Remove from `SharedState.partitions`.
      - Remove entry from `spawn_locks`.
   5. Remove `data/wal/<topic>/` directory (fast; local filesystem).
   6. If `delete_data = true`:
      - For each partition, read its manifest from object store into a snapshot (single GET per partition). This means N GETs sequentially inside `DeleteTopic`'s critical path — a topic with 100 partitions incurs ~100 object-store round-trips before the RPC responds. Acceptable for v1 scale; parallelising these GETs is a small future add if it becomes a driver.
      - Construct `PendingDelete { topic, uuid, manifests_by_partition, created_ns }`.
      - Atomically append to `data/pending_deletes.json` (fsync).
      - `tokio::spawn(sweep_deletion(record, store, prefix, data_dir))`.
      - Increment `DELETE_PENDING_TOPICS` gauge.
   7. Respond `Ok(())`.
3. Wire dispatch returns `DeleteTopicResponse` to the client.

Actor shutdown (step 4) uses a new `PwMsg::Shutdown(oneshot::Sender<()>)`; the `PartitionWriter::run` loop matches this variant, drains any in-flight seal + hand-off, then sends `()` and exits. The Uploader receives its mpsc closure and exits naturally (any in-flight `upload_once` completes first; it always completes because the WAL data outlives the actor).

### Sweep task

`sweep_deletion(record, store, prefix, data_dir) -> Result<()>`:

1. For each `(partition, manifest)` in `record.manifests_by_partition`:
   - For each `seg` in `manifest.segments`: `store.delete(segment_key(&prefix, &record.topic, &record.uuid, partition, seg.base_offset))`. Track counts.
   - `store.delete(manifest_key(&prefix, &record.topic, &record.uuid, partition))`.
2. One-shot LIST the prefix `{prefix}/{topic}/v={uuid}/` and delete every returned key. Catches orphan segments (PUT-succeeded-but-manifest-update-crashed) not present in any snapshot manifest.
3. Remove this record from `data/pending_deletes.json` (fsync).
4. Emit `DELETE_SEGMENTS_REMOVED`, `DELETE_BYTES_REMOVED`, `DELETE_DURATION_MS`. Decrement `DELETE_PENDING_TOPICS`.

Any DELETE failure returns `Err`. The `PendingDelete` record stays in `pending_deletes.json`. Next broker startup replays it. Failures do NOT cause a mid-process retry — the sweep is expected to be rare (topic deletion is not steady-state work), and per-topic idempotence means restart is a safe recovery mechanism. Retry-with-backoff is deferred.

### Startup replay

`main.rs` after `SharedState` construction:

```rust
let pending = pending_deletes::load_all(&cfg.data_dir).await.expect("load pending deletes");
metrics::gauge!(DELETE_PENDING_TOPICS).increment(pending.len() as f64);
for record in pending {
    tokio::spawn(deletion::sweep_deletion(
        record, store.clone(), prefix.clone(), cfg.data_dir.clone(),
    ));
}
```

Recovered pending records refer to topics that are no longer in `topics.json` (the registry entry was removed atomically at delete time), so no actor bring-up interferes.

### Object-store key construction

Every call site that builds a segment or manifest key now takes a topic UUID. Wide but mechanical change:

```rust
pub fn segment_key(prefix: &str, topic: &str, topic_uuid: &str, partition: u32, base_offset: i64) -> ObjPath;
pub fn manifest_key(prefix: &str, topic: &str, topic_uuid: &str, partition: u32) -> ObjPath;
```

Path: `{prefix}/{topic}/v={topic_uuid}/partition={partition}/{leaf}`. The `v=<uuid>` segment is the entire "cookie" — no other change needed to make recreation safe.

`PartitionHandle`, `Uploader`, `PartitionWriter`, and the fetcher's per-partition state all carry `topic_uuid: String` alongside `topic: String`. `spawn_partition` accepts the UUID from the registry snapshot at boot and from `CreateTopic` at runtime.

### Registry evolution

`TopicEntry` in `kafkrs-models/src/topic.rs`:

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TopicEntry {
    pub name: String,
    pub uuid: String,                       // NEW: UUIDv7 as a canonical string
    pub partition_count: u32,
    pub created_at_ns: i64,                 // existing
    #[serde(default)]
    pub config: TopicConfigOverrides,       // existing
}
```

No `#[serde(default)]` on `uuid` — a `topics.json` from 0.5.0 will fail to parse, matching the "no migration" ruling. The changelog will document that operators upgrading from 0.5.0 must delete `data/topics.json` (and, for consistency, the rest of `data_dir` and the object-store bucket) before starting 0.6.0.

`CreateTopic` handler generates the UUID:

```rust
use uuid::Uuid;
let uuid = Uuid::now_v7().to_string();
```

### Wire proto changes

`kafkrs-models/proto/wire/v1.proto`:

```proto
message Command {
  // ... existing fields ...
  oneof body {
    // ... existing 20-27 ...
    DeleteTopicRequest     delete_topic         = 50;
    DeleteTopicResponse    delete_topic_resp    = 51;
  }
  reserved 52 to 59;   // was: reserved 50 to 59;
}

message DeleteTopicRequest {
  string topic = 1;
  optional bool delete_data = 2;   // absent → true
}

message DeleteTopicResponse {}
```

No new `ErrorCode` variants. The atomic registry removal means:
- Delete on unknown topic → `ERR_UNKNOWN_TOPIC`.
- Concurrent `DeleteTopic` on same topic — the second sees the topic absent → `ERR_UNKNOWN_TOPIC`.
- Post-delete produce/fetch — partition handle absent from `SharedState.partitions` → `ERR_UNKNOWN_TOPIC`.

## Metrics

Five new metric constants added to `kafkrs-server/src/metrics.rs`, following the 0.5.0 `pub const` catalogue pattern:

| Constant | Name | Type | Labels | Description |
|---|---|---|---|---|
| `DELETE_PENDING_TOPICS` | `kafkrs.delete.pending_topics` | Gauge | — | Currently in-flight deletion sweeps |
| `DELETE_SEGMENTS_REMOVED` | `kafkrs.delete.segments_removed` | Counter | `topic` | Segments deleted by sweeps |
| `DELETE_BYTES_REMOVED` | `kafkrs.delete.bytes_removed` | Counter | `topic` | Cumulative bytes reclaimed by sweeps |
| `DELETE_DURATION_MS` | `kafkrs.delete.duration_ms` | Histogram | `topic` | Per-sweep wall-clock |
| `DELETE_ERRORS` | `kafkrs.delete.errors` | Counter | `topic` | Sweep DELETEs that failed |

Registered in `describe_all()`. Total broker metric count climbs from 32 to 37.

Cardinality: `topic` label only. Per-partition detail is not useful for deletion metrics — a topic is deleted as a whole. `DELETE_PENDING_TOPICS` is broker-wide.

## Python client

`kafkrs-python/kafkrs/__init__.py`:

```python
async def delete_topic(self, name: str, delete_data: bool = True) -> None:
    """Delete a topic. If delete_data is True (default), WAL files and
    object-store data are also removed asynchronously. If False, only
    the registry entry and running actors are torn down; storage is left
    for the caller to manage."""
```

Same shape as `create_topic`. Broker errors raise `WireError(ERR_UNKNOWN_TOPIC, ...)`.

## Impact on existing code

| Location | Change |
| --- | --- |
| `kafkrs-models/proto/wire/v1.proto` | Add `DeleteTopicRequest`/`DeleteTopicResponse` at fields 50/51; shrink `reserved` to `52 to 59`. |
| `kafkrs-models/src/topic.rs` | `TopicEntry.uuid: String`. No default — legacy `topics.json` fails to parse. |
| `kafkrs-models/Cargo.toml` | Add `uuid = { version = "1", features = ["v7", "serde"] }`. |
| `kafkrs-server/src/deletion.rs` | New. `PendingDelete` struct, `sweep_deletion` async fn, unit tests. |
| `kafkrs-server/src/pending_deletes.rs` | New. `append`, `remove`, `load_all`. Atomic write via `.tmp` + rename + `fsync`. |
| `kafkrs-server/src/topic_registry.rs` | `RegistryMsg::DeleteTopic`; handler; `CreateTopic` handler assigns UUIDv7. |
| `kafkrs-server/src/object_store.rs` | `segment_key` and `manifest_key` gain `topic_uuid: &str`. |
| `kafkrs-server/src/metrics.rs` | 5 new constants + 5 describes. |
| `kafkrs-server/src/lib.rs` | `pub mod deletion;` `pub mod pending_deletes;`. |
| `kafkrs-server/src/main.rs` | On boot, `pending_deletes::load_all` + spawn sweep tasks. |
| `kafkrs-server/src/startup.rs` | `spawn_partition` accepts `topic_uuid`, plumbs to Uploader + PartitionWriter. |
| `kafkrs-server/src/partition_writer.rs` | Field `topic_uuid: String`. `PwMsg::Shutdown(oneshot::Sender<()>)` variant. Use UUID in key construction. |
| `kafkrs-server/src/uploader.rs` | Field `topic_uuid: String`. Use UUID in key construction. |
| `kafkrs-server/src/fetcher.rs` | Use UUID in key construction. Where the UUID comes from — currently `PartitionHandle` — is threaded through. |
| `kafkrs-server/src/wire/dispatch.rs` | `PartitionHandle.uuid: String`. `handle_delete_topic` handler. |
| `kafkrs-server/src/wire/connection.rs` | Dispatch match arm adds `Body::DeleteTopic` case. `count_rpc` gets a new `"delete_topic"` label value. |
| `kafkrs-server/tests/wire_e2e.rs` | Four new tests (see Test Plan). Every fixture updated for `PartitionHandle.uuid`. |
| `kafkrs-server/tests/storage_e2e.rs` | Fixture updated. |
| `kafkrs-python/kafkrs/__init__.py` | `Client.delete_topic`. |
| `kafkrs-python/kafkrs/wire/v1_pb2.py` | Regenerated. |
| Cargo.toml + pyproject.toml + __init__.py | 0.5.0 → 0.6.0. |
| Cargo.lock | Regenerated (uuid crate). |
| CHANGELOG.md (×3) | 0.6.0 entries. |

## Versioning

Bump all three crates from 0.5.0 to **0.6.0** in lockstep.

- **Breaking on-disk format:** object-store keys gain `v=<uuid>/`. `topics.json` schema changes.
- **Breaking config schema:** none (config unchanged).
- **Wire protocol:** additive-safe within v1 (new RPC in the previously-reserved range). No proto version bump.

The changelog will explicitly document that upgrading from 0.5.0 requires deleting `data_dir` and the object-store bucket.

## Test plan

### Unit tests (`kafkrs-server/src/deletion.rs`)

- `sweep_deletion_removes_all_manifest_segments` — synthetic manifest snapshot, filesystem-backed store, verify every listed segment key is absent after sweep.
- `sweep_deletion_removes_manifest_after_segments` — order check (segments first, then manifest).
- `sweep_deletion_list_sweeps_orphan` — pre-place an object under the topic UUID prefix that isn't in the snapshot manifest; verify the LIST-and-sweep catches it.
- `sweep_deletion_idempotent_on_replay` — run sweep twice against the same snapshot; second run is a no-op with no errors.

### Unit tests (`kafkrs-server/src/pending_deletes.rs`)

- `append_and_load_roundtrip`.
- `remove_removes_only_targeted_entry`.
- `atomic_write_preserves_prior_file_on_crash` — simulate crash between `.tmp` write and rename; confirm previous state readable.

### Unit tests (`kafkrs-server/src/topic_registry.rs`)

- `delete_topic_unknown_returns_error`.
- `create_topic_assigns_unique_uuid` — two consecutive creates get different UUIDs; both are valid v7.
- `delete_topic_removes_registry_entry_before_returning` — inspect in-memory state at each step via a controlled sequence.

### Integration tests (`kafkrs-server/tests/wire_e2e.rs`)

- `delete_topic_removes_partition_and_rejects_subsequent_produce` — create → produce 3 → delete (`delete_data=true`) → produce again returns `ERR_UNKNOWN_TOPIC`. Give the sweep 2 s wall-clock, then assert `data/wal/<topic>/` is gone and no objects remain under `<topic>/v=<uuid>/` in the filesystem store.
- `delete_topic_delete_data_false_preserves_object_store_data` — same shape with `delete_data=false`; after delete, assert files still present.
- `delete_then_recreate_same_name_uses_new_uuid_prefix` — delete → immediately recreate `orders` → produce/fetch on recreated topic succeed. Compare pre-delete UUID to post-recreate UUID: different values, disjoint prefixes.
- `pending_delete_survives_broker_restart` — arrange for a sweep to be interrupted mid-flight (e.g. inject a failing filesystem store), restart the broker fixture, confirm sweep completes on retry.

Every existing fixture that constructs a `PartitionHandle` gains `uuid: "<test-uuid>".to_string()`.

### Python integration test (`kafkrs-python/tests/test_client.py`)

- `test_delete_topic_removes_data` — create → produce → delete → confirm subsequent produce raises `WireError` with `ERR_UNKNOWN_TOPIC`.

## Out of scope

### Deferred to future work
- **Global rate-limiting of concurrent sweeps.** Currently N pending deletes = N tasks. If someone deletes 1 000 topics simultaneously, that's 1 000 tasks each self-throttled by the object store's per-prefix rate limits. Adding a shared semaphore (e.g. 20 concurrent sweeps) is a small future add if contention emerges.
- **Retry-with-backoff on sweep failure within a single process.** Currently a failed sweep is deferred until next broker restart. Adding an in-process retry timer is a small future add.
- **`AlterConfig`.** Next admin RPC in the 50-59 range. Independent feature; separate spec.
- **Sweep progress query API.** Metrics cover observability; a per-topic "is X still being swept" wire RPC is a future consideration.
- **Consumer group offset cleanup.** Consumer groups don't exist yet. When they do, `handle_delete_topic` will need to purge group offsets for the deleted topic.
- **Cascade rules** (e.g. "delete all topics matching prefix"). One RPC = one topic.
- **Dry-run mode.**

### Not in scope at all
- **Multi-broker coordination of deletion.** Single-broker only.
- **Undo/soft-delete.** Delete is final; if operators want reversibility they use `delete_data=false` and reconstruct externally.

## Invariants (for implementers)

1. **Registry entry removal is atomic with partition-handle removal from `SharedState.partitions`.** From that moment, no produce/fetch can reach the deleted topic's actors.
2. **Actor shutdown is awaited before the RPC responds.** WAL/manifest state is quiesced before the client sees success.
3. **Manifest snapshots for the sweep are captured AFTER actor shutdown.** No upload lands in the object store that isn't reflected in the snapshot.
4. **The sweep is idempotent.** Segments-first → manifest → LIST-and-sweep. All deletes tolerate not-found. Replays are safe.
5. **`data/pending_deletes.json` is the source of truth for in-flight sweeps.** Broker restart replays every entry. Sweep success removes the entry.
6. **`delete_data = false` never writes to `pending_deletes.json` and never spawns a sweep.** Fast path only.
7. **Topic UUIDs never repeat.** UUIDv7 gives collision resistance + time ordering; no reuse across delete + recreate of the same name.
8. **The one-shot LIST during sweep is the only place in the broker that lists the object store.** Documented in `deletion.rs`'s module doc.
9. **Object-store keys always include `v=<uuid>/` after the topic name.** No legacy layout — 0.5.0 data must be discarded before upgrade.
