# Retention (Segment Deletion) — Design

**Status:** Draft for review
**Date:** 2026-09-21
**Scope:** Per-topic, time-based and size-based deletion of uploaded Parquet segments plus the corresponding manifest updates. No proto-version bump; additive changes only. Object-store LIST-based orphan reclamation is explicitly deferred. Compaction (Kafka's `cleanup.policy=compact`) is out of scope.

## Motivation

Every sustained run of the broker to date has accumulated Parquet segments indefinitely. The storage spec (`docs/superpowers/specs/2026-05-18-storage-model-design.md`, "Out of scope" section) explicitly deferred retention with the note "v1 keeps everything." That deferral has now become the single largest concrete blocker to running kafkrs for more than a demo session: disk-cost, bucket-cost, and manifest-size all grow monotonically. This spec closes that gap.

The design keeps four properties from prior specs load-bearing:

- **No object-store LIST on the hot path or on startup.** The manifest remains the sole authoritative index; retention operates on and rewrites the manifest, never scans the bucket.
- **The Uploader is the sole writer of the manifest.** Retention is embedded in the Uploader, not a parallel writer. Preserves the multi-broker leadership constraint the storage spec anticipates.
- **Producer/consumer visibility invariants unchanged.** Retention only touches uploaded segments — data that has already been visible to consumers for however long the retention policy specifies. Nothing in-flight is affected.
- **Additive proto evolution.** `TopicConfigOverrides` gains two fields; no field-number changes, no protocol_version bump.

## Design choices, with rationale

### Two policy dimensions, both opt-out-able

`retention_ms` and `retention_bytes`, both `Option<i64>` on `TopicConfigOverrides`, both defaulted to `-1` sentinel = "no limit". A segment is deletion-eligible if **either** dimension says so. Matches Kafka's default semantics; covers the two most common ops shapes without introducing a third policy axis.

Defaults:
- `retention_ms = 7 * 24 * 3600 * 1000` (604 800 000 ms = 7 days). Matches Kafka's default.
- `retention_bytes = -1`. Size-based is opt-in per topic when disk pressure matters.

`-1` sentinel chosen over `Option::None` at the resolved layer because the resolved layer needs a single unambiguous value; the `Option` distinction only matters at the override layer (unset → inherit broker default) and stays there.

### Uploader-embedded retention + broker-wide idle sweeper (Option 1.5)

Two triggers, one code path:

**Active path.** After every successful `upload_once` (Parquet PUT + manifest update), the Uploader runs a retention pass against its just-updated manifest before returning to the message loop. The manifest bytes are already in memory; the cost is a scan of `segments`, computation of the eviction set, N object-store DELETEs, and one manifest rewrite. Steady-state cost per non-trivial upload: proportional to the number of segments *actually evicted* on that pass, which is bounded.

**Idle path.** One broker-wide `RetentionSweeper` actor. Ticks every `broker.retention_sweep_interval_ms` (default 60 000). Each tick, walks `state.partitions`; for any partition whose last retention run is older than the sweep interval, sends a `UploaderMsg::RetentionKick` to that partition's Uploader. The Uploader handles the kick by running the same retention pass.

The alternatives considered — pure Uploader-embedded (Option 1) and per-partition retention actor (Option 2) — were rejected because:
- Pure Option 1 leaves the "idle partition never evicts" hole. In practice, any partition that stops receiving writes pins its data forever regardless of the topic's `retention_ms`. The user's mental model breaks silently.
- Option 2 adds an actor per partition, another manifest-adjacent writer per partition, and doesn't collapse cleanly under multi-broker leadership (the retention actor and the Uploader would both need co-location with the leader). Option 1.5 keeps manifest writers per partition at 1.

Option 1.5 also gives a natural home for cross-partition rate limiting later (a `tokio::sync::Semaphore` held on the Sweeper), without forcing that concern into v1.

### Manifest-first ordering, orphans accepted

Deletion order:
1. Compute eviction set from manifest.
2. Rewrite manifest **first**, dropping evicted entries. PUT.
3. Then DELETE each evicted segment object.

If the broker crashes between step 2 and step 3, the object store contains Parquet files not referenced by any manifest — orphans. They cost money but do not affect correctness: no fetch will ever reference them (the manifest is authoritative), and startup recovery does not enumerate them (spec invariant: no object-store LIST).

Orphan reclamation is documented as an accepted v1 limitation. Two future paths (both deferred):
- A `DeleteTopic` implementation naturally cleans up all orphans for the deleted topic (it would list-and-delete the partition prefix).
- A future `kafkrs-cli reap-orphans` command for manual GC.

Reverse ordering (DELETE-first, manifest-last) is explicitly wrong: it opens a window where the manifest references non-existent segments, breaking active fetches.

### Retention runs after each upload, not before

Alternative considered: run the retention pass before the manifest rewrite that adds the new segment, so a single manifest PUT handles both the append and the evictions. Rejected because:
- Bloats the "add segment" critical path with retention decision logic.
- Makes retention failures conflate with upload failures.
- Fails the invariant that the upload path stays as narrow as possible.

Running retention after upload — as a separate manifest read-modify-PUT — is slightly more work per upload (two manifest PUTs instead of one) but cleanly separates concerns. The retention PUT is idempotent by construction; a crash between the two PUTs leaves a consistent manifest with only the append, and retention picks up on the next trigger.

### Config-driven sweep interval

`broker.retention_sweep_interval_ms` is a real config field, not a hardcoded constant, from day one. The reasoning: the interval is the primary lever for trading off eviction latency against manifest-read cost at high partition counts, and operators will want to tune it before we can predict what value to bake in.

### `TopicConfigOverrides` fields 9 and 10

Field numbering continues the additive pattern established in 0.3.1 (field 8 = `max_fetch_wait_ms`). Both new fields are `optional int64` in the proto — signed to carry the `-1` sentinel. Because 0.3.1's `max_fetch_wait_ms` is `optional uint64`, this establishes the mixed-signedness precedent explicitly.

## Architecture

No new modules; no new files beyond a new source file for the sweeper. All changes localized:

```
kafkrs-models/proto/wire/v1.proto        ← add retention_ms (9), retention_bytes (10)
kafkrs-models/src/topic.rs               ← DEFAULT_RETENTION_MS/BYTES; new fields on
                                            TopicConfigOverrides and ResolvedTopicConfig
kafkrs-models/src/config.rs              ← BrokerConfig.retention_sweep_interval_ms
kafkrs-models/src/manifest.rs            ← (no changes to Manifest itself; new helpers
                                            live in the server-side retention module)

kafkrs-server/src/object_store.rs        ← add pub async fn delete(&Arc<...>, &ObjPath)
kafkrs-server/src/uploader.rs            ← UploaderMsg::RetentionKick; handle_retention_pass();
                                            call handle_retention_pass at end of upload_once
kafkrs-server/src/retention.rs           ← NEW: evaluate_eviction(&Manifest, &ResolvedTopicConfig, now_ns)
                                            → Vec<SegmentEntry> (pure function; unit-testable)
kafkrs-server/src/retention_sweeper.rs   ← NEW: RetentionSweeper actor (per-broker)
kafkrs-server/src/wire/dispatch.rs       ← extend the two overrides translation fns with
                                            retention_ms / retention_bytes
kafkrs-server/src/wire/dispatch.rs       ← SharedState gains uploader_txs so the sweeper can
                                            enqueue kicks (see below)
kafkrs-server/src/main.rs                ← spawn RetentionSweeper at boot
kafkrs-server/src/startup.rs             ← spawn_partition returns/registers the
                                            uploader mpsc handle in SharedState
```

### `SharedState` gains an uploader-handle map

The RetentionSweeper needs to send `UploaderMsg::RetentionKick` to each partition's Uploader. Today `state.partitions` carries `PartitionHandle { pw_tx, tail, cfg }` — the Uploader's mpsc is owned only inside `spawn_partition` and never re-exposed.

Two options:
1. Add `uploader_tx: mpsc::Sender<UploaderMsg>` to `PartitionHandle`.
2. Add a parallel `Arc<RwLock<HashMap<(String, u32), mpsc::Sender<UploaderMsg>>>>` on `SharedState`.

Chosen: **option 1** (extend `PartitionHandle`). Keeps everything about a partition's actor topology in one place. `PartitionHandle` already has a `cfg` field added in 0.3.1 — this is the same shape of extension.

## The three components

### Component 1 — `evaluate_eviction` (pure function)

`kafkrs-server/src/retention.rs`:

```rust
use kafkrs_models::manifest::{Manifest, SegmentEntry};
use kafkrs_models::topic::ResolvedTopicConfig;

/// Compute the set of segments to evict. Pure: given a manifest, config,
/// and current wall-clock time, returns the segments that fall outside
/// both retention policies. Never returns the last segment even if it
/// would be eligible (see rationale below).
pub fn evaluate_eviction(
    manifest: &Manifest,
    cfg: &ResolvedTopicConfig,
    now_ns: i64,
) -> Vec<SegmentEntry> {
    if manifest.segments.len() <= 1 {
        return Vec::new();
    }

    // Time-based eligibility: last_timestamp_ns older than the threshold.
    let time_cutoff_ns: Option<i64> = if cfg.retention_ms < 0 {
        None
    } else {
        Some(now_ns - cfg.retention_ms.saturating_mul(1_000_000))
    };

    // Size-based eligibility: evict oldest first until total size <= cap.
    let total_bytes: u64 = manifest.segments.iter().map(|s| s.byte_size).sum();
    let mut over_size_by: i64 = if cfg.retention_bytes < 0 {
        0
    } else {
        (total_bytes as i64) - cfg.retention_bytes
    };

    let mut evict = Vec::new();
    // Segments are stored in base_offset order (Uploader sorts on insert).
    // Iterate all but the last — never evict the tail segment.
    for seg in &manifest.segments[..manifest.segments.len() - 1] {
        let time_says_evict =
            time_cutoff_ns.map_or(false, |cutoff| seg.last_timestamp_ns < cutoff);
        let size_says_evict = over_size_by > 0;
        if time_says_evict || size_says_evict {
            over_size_by -= seg.byte_size as i64;
            evict.push(seg.clone());
        } else {
            break; // segments are age-ordered; once one survives, all newer ones do too
        }
    }
    evict
}
```

Why never evict the last segment: it's the one the Uploader most recently appended. Deleting it immediately after appending would produce a weird "produce succeeded but immediately vanished" experience for any consumer reading the tail. Practically, `retention_ms` policies of days/weeks are meaningless against a segment sealed seconds ago; keeping the tail unconditionally is the safest floor. This mirrors Kafka's default behavior (the active segment is never eligible for deletion).

Note the "once one survives, all newer ones do too" short-circuit: age is monotonic across segments (base_offset order = time order), so an older-than-cutoff segment guarantees no older segments remain. The size-based check breaks this monotonicity slightly if `retention_bytes` is the binding constraint — but the loop is `O(segments)` either way, and the break-on-survivor is correct because both conditions must be false for a segment to survive.

**Unit-testable in isolation.** No IO, no async, no actors.

### Component 2 — Uploader integration

`UploaderMsg` gains one variant:

```rust
pub enum UploaderMsg {
    Upload(SealedBatch),
    RetentionKick,
}
```

`Uploader::run` handles both:

```rust
pub async fn run(mut self) {
    while let Some(msg) = self.rx.recv().await {
        match msg {
            UploaderMsg::Upload(batch) => {
                // ... existing upload_once retry loop ...
                self.durable_tx.send(SegmentDurable { base_offset }).await;
                let _ = self.retention_pass().await;   // NEW
            }
            UploaderMsg::RetentionKick => {
                let _ = self.retention_pass().await;
            }
        }
    }
}
```

`Uploader` gains a new field `cfg: ResolvedTopicConfig` (passed to `Uploader::new`; already resolved at `spawn_partition` time). `retention_pass()`:

1. Fetch the current manifest (single GET, same as `upload_once`).
2. Call `evaluate_eviction(&manifest, &self.cfg, now_ns)`.
3. If empty → return early.
4. Otherwise: remove the evicted entries from `manifest.segments`, PUT the manifest.
5. Then loop over the evicted set and issue `store.delete()` for each.
6. On any error in step 5, log-and-continue. Orphans are accepted.

Failure of the manifest rewrite (step 4) skips the DELETEs. Retention will retry on the next trigger; the manifest still has the segments, so consumers keep working normally.

### Component 3 — `RetentionSweeper`

`kafkrs-server/src/retention_sweeper.rs`:

```rust
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

pub struct RetentionSweeper {
    partitions: SharedPartitionsHandle,   // Arc<RwLock<HashMap<..., PartitionHandle>>>
    sweep_interval: Duration,
}

impl RetentionSweeper {
    pub async fn run(self) {
        let mut ticker = interval(self.sweep_interval);
        loop {
            ticker.tick().await;
            let snapshot: Vec<mpsc::Sender<UploaderMsg>> = {
                let guard = self.partitions.read().await;
                guard.values().map(|h| h.uploader_tx.clone()).collect()
            };
            for tx in snapshot {
                // Best-effort; drop the kick if the Uploader is busy.
                // Retention is idempotent, so lost kicks are harmless.
                let _ = tx.try_send(UploaderMsg::RetentionKick);
            }
        }
    }
}
```

Design points worth calling out:

- **Reads the partitions map under `RwLock`, releases immediately, then sends kicks outside the lock.** No lock held across `.await`.
- **`try_send` not `send`.** If the Uploader is backed up (e.g., the active path just fired retention itself), the kick is dropped rather than blocking the Sweeper. Retention is idempotent; a dropped kick is fine.
- **No per-partition "last sweep" tracking.** Sweeper unconditionally kicks every partition every tick. The Uploader's retention pass is a fast no-op when there's nothing to evict (one manifest GET + empty eviction set). Simpler than tracking timestamps and defensible: at 60s intervals and typical partition counts, the extra GETs are noise.
- **One shutdown consideration.** The Sweeper runs forever; broker shutdown drops the whole tokio runtime, which terminates it. No explicit shutdown message needed. If we later want graceful shutdown (finish the current tick, then exit), we'll add a shutdown channel then.

### Config resolution flow

- Startup: `Config` parsed → `broker.retention_sweep_interval_ms.unwrap_or(60_000)` produces the effective duration → passed to `RetentionSweeper::new`.
- `spawn_partition` receives `ResolvedTopicConfig` (already resolved from topic overrides + defaults) → passes it to `Uploader::new` → Uploader retains it as `self.cfg` for retention passes.
- If a topic's config changes at runtime (future: `AlterTopicConfig`): out of scope for this spec. The current model is that per-topic config is set at `CreateTopic` and immutable. Retention will use whatever `cfg` the Uploader was spawned with.

## Object-store DELETE helper

`kafkrs-server/src/object_store.rs` gains:

```rust
pub async fn delete(store: &Arc<dyn ObjectStore>, key: &ObjPath) -> Result<()> {
    store.delete(key).await?;
    Ok(())
}
```

Mirrors the existing `put` / `get` / `get_range` helpers exactly.

## Impact on existing code

| Location | Change |
| --- | --- |
| `kafkrs-models/proto/wire/v1.proto` | Add `optional int64 retention_ms = 9;` and `optional int64 retention_bytes = 10;` to `TopicConfigOverrides`. |
| `kafkrs-models/src/topic.rs` | Add `DEFAULT_RETENTION_MS = 7 days`, `DEFAULT_RETENTION_BYTES = -1`. Extend `TopicConfigOverrides` (Option<i64> both) and `ResolvedTopicConfig` (i64 both). Extend `resolve()`. Extend the two existing unit tests. |
| `kafkrs-models/src/config.rs` | Add `retention_sweep_interval_ms: Option<u64>` to `BrokerConfig` with `#[serde(default)]`. Extend the existing config-parsing unit tests. |
| `kafkrs-server/src/object_store.rs` | New `pub async fn delete(...)` helper. |
| `kafkrs-server/src/retention.rs` | New file. `evaluate_eviction` pure function + unit tests. |
| `kafkrs-server/src/retention_sweeper.rs` | New file. `RetentionSweeper` actor. |
| `kafkrs-server/src/uploader.rs` | Add `cfg: ResolvedTopicConfig` field to `Uploader`. Extend `Uploader::new` signature. Add `UploaderMsg::RetentionKick`. Add `retention_pass()` method. Call `retention_pass()` at end of `Upload` handling and on `RetentionKick`. |
| `kafkrs-server/src/wire/dispatch.rs` | Add `uploader_tx: mpsc::Sender<UploaderMsg>` to `PartitionHandle`. Extend `wire_overrides_to_model` / `model_overrides_to_wire` with the two new fields. |
| `kafkrs-server/src/startup.rs` | Pass `cfg` to `Uploader::new`. Insert `uploader_tx` into the `PartitionHandle` literal. |
| `kafkrs-server/src/main.rs` | After `SharedState` construction, spawn the `RetentionSweeper` with `state.partitions.clone()` and the resolved sweep interval. |
| `kafkrs-server/src/lib.rs` | `pub mod retention;` and `pub mod retention_sweeper;`. |
| `kafkrs-server/tests/wire_e2e.rs` | Update every `setup_broker*` fixture: the four fixtures each construct a `PartitionHandle` literal; they gain `uploader_tx`. Introduce a fixture variant with tight retention thresholds for the new integration test. |
| `kafkrs-python/kafkrs/wire/v1_pb2.py` | Regenerated (proto changed). |

No new dependencies. No breaking changes.

## Versioning

Bump all three crates from 0.3.2 to **0.4.0** in lockstep. Rationale for a minor rather than a patch: retention introduces user-facing default behavior change (data now expires after 7 days by default!). Existing operators upgrading from 0.3.x should see this as a semver-worthy signal. Wire protocol version stays at `1` — proto changes are additive.

Python crate bumps for lockstep. Its regenerated `v1_pb2.py` picks up the two new fields on `TopicConfigOverrides`; client API is unchanged; existing scripts continue to work.

## Test plan

### Unit tests (`kafkrs-server/src/retention.rs`)

- `no_eviction_when_all_infinite` — cfg `retention_ms = -1`, `retention_bytes = -1`; return empty.
- `no_eviction_when_manifest_has_one_segment` — never evict the last segment.
- `time_based_evicts_expired_segments` — manifest with 3 segments spanning `now - 8h`, `now - 4h`, `now - 30m`; cfg `retention_ms = 1h`. Assert: only the first is evicted (the middle survives because `last_timestamp_ns` is at `now - 4h + segment span` = within 1h; the tail is always kept).
- `size_based_evicts_oldest_first` — 3 × 100-byte segments, `retention_bytes = 250`. Only the oldest is evicted.
- `either_dimension_triggers_eviction` — `retention_ms = 1h` and `retention_bytes = 250`; construct a case where time says yes but size says no, verify eviction happens.
- `tail_segment_never_evicted` — construct a manifest where the tail is old and small; cfg says both time and size should evict everything; assert the tail survives.

### Unit tests (`kafkrs-models/src/topic.rs`)

Extend `resolved_defaults_when_no_overrides`:
```rust
assert_eq!(r.retention_ms, 7 * 24 * 3600 * 1000);
assert_eq!(r.retention_bytes, -1);
```
Add `retention_overrides_win` — set `retention_ms = Some(-1)` (infinite) and `retention_bytes = Some(1_000_000_000)`; assert resolved values match.

### Unit tests (`kafkrs-models/src/config.rs`)

Extend `defaults_apply_when_optional_sections_absent`:
```rust
assert_eq!(cfg.broker.retention_sweep_interval_ms, None);
```
Add case where the field is explicitly set in TOML and parses correctly.

### Uploader tests (`kafkrs-server/src/uploader.rs`)

Add:
- `upload_then_retention_evicts_expired` — set up a manifest with an old segment already present + a small `retention_ms`; produce a new segment via the Uploader; assert the manifest after upload contains only the new segment and the old segment's key is gone from the object store.
- `retention_kick_evicts_without_upload` — pre-populate a manifest with an old segment; send `UploaderMsg::RetentionKick`; assert the segment is gone.

### Integration test (`kafkrs-server/tests/wire_e2e.rs`)

- `retention_evicts_old_segments_via_sweeper` — spin up a broker with `retention_sweep_interval_ms = 100`, create a topic with `retention_ms = 200`, produce enough records to seal at least 2 segments, sleep 500ms, produce one more (to keep tail active), assert an early fetch from offset 0 returns `ErrOffsetOutOfRange` because the segment was evicted. Wall-clock-based; keep tolerances generous.

### Python integration test

None needed. The Python client isn't affected by retention behavior; it only sees `ErrOffsetOutOfRange` (which it already handles correctly). Skipping avoids adding a flaky wall-clock test at the Python layer.

## Out of scope

### Deferred to future work
- **Object-store orphan reclamation.** Documented in the "Manifest-first ordering" section. Accepted as a v1 limitation. Natural future hooks: `DeleteTopic` cleans partition prefixes; `kafkrs-cli reap-orphans` for manual GC.
- **Compaction** (Kafka's `cleanup.policy=compact`). Separate concern; needs a different algorithm entirely (key-based dedup rather than segment-based deletion).
- **`AlterTopicConfig`.** The Uploader's `cfg` is set at spawn time. Config changes need a broker restart until AlterTopicConfig lands. Separate spec.
- **Per-broker retention rate limiting.** Sweeper is the natural place for a `Semaphore` when object-store request budget matters. Not needed at v1 scale.
- **Retention metrics.** No `broker_metrics` surface exists yet; when it does, retention should report eviction counts and byte totals.
- **Graceful sweeper shutdown.** Current design terminates the sweeper along with the tokio runtime at broker shutdown. If future work requires "finish current sweep before exiting," add a shutdown channel.

### Not in scope at all
- **Consumer group offset management.** A consumer that falls behind retention gets `ERR_OFFSET_OUT_OF_RANGE` on next fetch. Consumer groups (when they land) will need to define policy for this — either reset to earliest-available or fail — but that's the consumer-groups spec's concern.
- **Multi-broker retention coordination.** Option 1.5 preserves per-partition leadership shape; when multi-broker lands, retention will remain leader-embedded (i.e., wherever the Uploader lives). No cross-broker coordination needed.

## Invariants (for implementers)

1. **The Uploader is the only manifest writer per partition.** Retention adds a second reason for the Uploader to rewrite the manifest, but doesn't add a new writer.
2. **Manifest rewrite always precedes segment DELETEs.** The reverse ordering is a correctness bug.
3. **The last (tail) segment is never eligible for eviction.** Regardless of policy configuration.
4. **`RetentionSweeper.run()` holds no locks across await.** The partitions map is read under `RwLock`, collected, released, then sends happen outside the lock.
5. **Retention kicks are idempotent and best-effort.** `try_send` failures are dropped; the next tick retries.
6. **No object-store LIST.** Retention operates strictly on the manifest. Orphans in the object store are accepted.
7. **`retention_ms = -1` and `retention_bytes = -1` mean "never expire on that dimension."** Both `-1` = keep forever, same as pre-0.4.0 behavior.
