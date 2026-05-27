# `spawn_partition` Idempotency — Design

**Status:** Draft for review
**Date:** 2026-05-24
**Scope:** Two bug fixes in the broker. No proto changes, no wire-protocol changes, no version bump beyond a 0.3.2 patch.

## Motivation

The Tier 1 fixes (0.3.1) added an explicit `handle_create_topic` spawn loop. The post-implementation review surfaced a follow-up concern that turned out to be more severe than first thought:

1. **`EnsureExists` returns the wrong reply for existing topics.** `topic_registry.rs:109-110` returns `Ok(())` when the topic already exists. The only caller (`handle_produce`'s auto-create branch) treats `Ok(Ok(()))` as "newly created — spawn partition workers." The result: **every produce to an existing auto-created topic re-spawns the partition workers and overwrites the partition handle in `state.partitions`**, orphaning the previous `PartitionWriter` and `Uploader`. The actors shut down cleanly when their channels drop (verified — `PartitionWriter::run` has `Some(Shutdown) | None => { flush_commit; break }`; `Uploader::run` exits on `while let Some` close), so no data is lost, but file handles and tokio tasks churn on every produce. This is the steady-state behavior of auto-create today, not a rare race.

2. **`spawn_partition` is not idempotent.** Even with `EnsureExists` fixed, the function blindly inserts into `partitions` and starts a fresh `PartitionWriter` + `Uploader` per call. A future caller that forgets the partition-existence check (or a path that races through the registry's serialization) would still orphan actors. The current architecture relies on caller discipline.

3. **A misleading comment.** `handle_produce`'s auto-create `Err(AlreadyExists)` arm contains `/* partition workers already running */`. Today this arm is essentially unreachable; the comment is inaccurate. After Fix 1, the arm becomes the common case and the comment becomes accurate but still terse.

These fixes belong together because they touch the same code path and each one alone leaves a worse-shaped defense than the bundle.

## Design choices, with rationale

### Two-layered defense: registry semantics + spawn idempotency

The minimal correctness fix is registry-only — make `EnsureExists` return `Err(AlreadyExists)` when the topic exists. This alone eliminates the steady-state orphan reproduction. But the spawn pathway then depends on every caller correctly dispatching on the reply, and on future callers not racing through some new path.

Belt-and-braces idempotency in `spawn_partition` itself is the right defensive posture: the broker stays correct regardless of caller mistakes. This matches the project's "defensive at the right layer" pattern (e.g., per-partition actors own their offset counter; the wire layer validates inputs).

Both layers ship together. Either alone leaves a gap.

### Per-key Tokio mutex, never removed

The idempotency guard is a `Arc<std::sync::Mutex<HashMap<(String, u32), Arc<tokio::sync::Mutex<()>>>>>` on `SharedState`. The outer std mutex protects the map; entries are held briefly (lookup + insert) and never across `.await`. The per-key Tokio mutex serializes the actual spawn work, which contains awaits (recovery + actor channel setup).

Alternatives considered and rejected:
- **Single global spawn mutex.** Serializes spawns across all partitions. Simpler but throttles boot-time bring-up of many partitions. Unnecessary cost.
- **`tokio::task::JoinSet` instead of `AbortHandle` map for orphan cleanup.** This is the wrong layer — JoinSet helps cancel on disconnect, doesn't help with multi-caller spawn idempotency.
- **Pre-check then spawn then post-check via `Entry::Occupied`.** Safe (orphaned actors shut down via channel-drop), but wastes the recovery + actor allocation work when racing callers exist. The per-key mutex avoids this entirely.

### Cleanup policy: never remove entries

Lock-map entries are added on first `spawn_partition` for a given `(topic, partition)` and never removed in v1. Justification:

- **Memory cost is negligible at v1 scale.** Each entry is ~100–150 bytes; topic count is monotonic in v1 (no `DeleteTopic`); the lock map can't grow faster than the topic registry. A broker with 1000 topics × 10 partitions costs ~1.5 MB.
- **Cleanup-on-spawn is safe but produces churn without savings.** `Arc<TokioMutex>` keeps the mutex alive for in-flight callers across a remove, so removing after a successful spawn doesn't break correctness. But subsequent callers re-create the entry; net savings ≈ zero, code complexity > zero.
- **The natural cleanup hook is `DeleteTopic`.** When `DeleteTopic` lands (deferred per the storage spec), remove the partition's lock-map entries in the same transaction that removes the partition handle. This deferral is captured in operator memory (`project_spawn_locks_cleanup.md`) so a future implementer doesn't re-derive the trade-off.

### `EnsureExists` semantic alignment

Post-fix, `EnsureExists`'s reply has the same shape as `Create`'s: `Ok(())` ⟹ topic was newly created right now, `Err(AlreadyExists)` ⟹ it already existed before this call. The two RPC variants now differ only in **policy** (Create rejects on already-existing; EnsureExists treats it as a non-error from the producer's perspective and continues) — not in reply semantics.

The caller in `handle_produce` already has the correct match arms; only the comment changes.

## Architecture

No new modules. All changes localized:

```
kafkrs-server/src/topic_registry.rs       ← 2-line EnsureExists semantic fix
kafkrs-server/src/wire/dispatch.rs        ← SharedState.spawn_locks field; pass to spawn_partition;
                                            update misleading comment
kafkrs-server/src/startup.rs              ← spawn_partition signature: new spawn_locks param;
                                            per-key lock acquire + idempotency check
kafkrs-server/src/main.rs                  ← create spawn_locks at boot; pass to spawn_partition
                                            and SharedState
kafkrs-server/tests/wire_e2e.rs           ← setup_broker* helpers: add spawn_locks field to
                                            SharedState; two new integration tests
```

The lock-map type is verbose enough that we give it a public alias in `wire/dispatch.rs`, used by `SharedState`, the `spawn_partition` signature, and any test-fixture variable type annotations:

```rust
/// Per-(topic, partition) locks coordinating concurrent `spawn_partition`
/// calls. Outer std::sync::Mutex guards the map (held briefly for entry
/// lookup/insert, never across await); per-key tokio::sync::Mutex is held
/// across the full spawn body (which awaits on recovery and channel setup).
pub type PartitionSpawnLocks =
    Arc<StdMutex<HashMap<(String, u32), Arc<TokioMutex<()>>>>>;
```

## The three fixes

### Fix 1: `EnsureExists` returns `Err(AlreadyExists)` for existing topics

`kafkrs-server/src/topic_registry.rs:109-110`:

```rust
// Before:
let r: Result<(), RegistryError> = if self.topics.contains_key(&name) {
    Ok(())
} else {
    self.create(&name, partition_count, TopicConfigOverrides::default())
        .await
};

// After:
let r: Result<(), RegistryError> = if self.topics.contains_key(&name) {
    Err(RegistryError::AlreadyExists)
} else {
    self.create(&name, partition_count, TopicConfigOverrides::default())
        .await
};
```

No other changes in `topic_registry.rs`. The reply type is unchanged; only the value returned for the existing-topic case flips.

### Fix 2: `spawn_partition` idempotency

**`SharedState` gains a `spawn_locks` field:**

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as TokioMutex;

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

**`spawn_partition` gains a `spawn_locks` parameter and an idempotency guard:**

```rust
pub async fn spawn_partition(
    data_dir: &str,
    topic: &str,
    partition: u32,
    cfg: ResolvedTopicConfig,
    store: Arc<dyn object_store::ObjectStore>,
    prefix: String,
    partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
    spawn_locks: PartitionSpawnLocks,
) {
    let key = (topic.to_string(), partition);

    // Acquire (or create) the per-key Tokio mutex.
    // The std::sync::Mutex on the outer map is held only briefly here.
    let lock = {
        let mut locks = spawn_locks.lock().unwrap();
        locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    // Already present? Concurrent caller won the race; no-op.
    if partitions.read().await.contains_key(&key) {
        return;
    }

    // ... existing recovery + actor spawn work, unchanged ...

    partitions.write().await.insert(key, PartitionHandle { pw_tx, tail, cfg });
}
```

The `_guard` is held until function return, so the check-and-insert sequence is atomic per-key. Concurrent callers for the same key serialize; callers for different keys don't contend.

**Three call sites pass `spawn_locks`:**

- **`main.rs` boot loop:** creates the map fresh; passes a clone to each `spawn_partition` call in the bring-up loop; stores it in `SharedState` for runtime use:

  ```rust
  let spawn_locks: PartitionSpawnLocks = Arc::new(StdMutex::new(HashMap::new()));

  for (topic, pcount, rtc) in known {
      for p in 0..pcount {
          spawn_partition(
              &cfg.data_dir, &topic, p, rtc,
              store.clone(), prefix.clone(),
              partitions.clone(),
              spawn_locks.clone(),
          ).await;
      }
  }

  let state = SharedState {
      // ... existing fields ...
      spawn_locks: spawn_locks.clone(),
  };
  ```

- **`handle_create_topic`:** pass `state.spawn_locks.clone()` to each `spawn_partition` call in the partition spawn loop.
- **`handle_produce` auto-create branch:** pass `state.spawn_locks.clone()` to each `spawn_partition` call in the partition spawn loop.

### Fix 3: Misleading comment

In `wire::dispatch::handle_produce`'s auto-create branch:

```rust
// Before:
Ok(Err(RegistryError::AlreadyExists)) => { /* partition workers already running */ }

// After:
Ok(Err(RegistryError::AlreadyExists)) => {
    // Topic existed before this produce, so its partition workers were
    // spawned by a prior CreateTopic or EnsureExists call. Nothing to do.
}
```

After Fix 1, this arm fires for every produce to an existing auto-created topic — replacing the broken steady-state churn with a clean no-op.

## Impact on existing code

| Location | Change |
| --- | --- |
| `kafkrs-server/src/topic_registry.rs` | 2-line `EnsureExists` semantic flip. Unit test added. |
| `kafkrs-server/src/wire/dispatch.rs` | Add `spawn_locks` to `SharedState`. Pass `state.spawn_locks.clone()` in `handle_create_topic` and `handle_produce` spawn loops. Update the misleading comment. |
| `kafkrs-server/src/startup.rs` | `spawn_partition` gains the `spawn_locks` parameter and the per-key lock + idempotency check at the top. Existing spawn body unchanged. |
| `kafkrs-server/src/main.rs` | Create `spawn_locks` at boot; pass to all `spawn_partition` calls and to `SharedState`. |
| `kafkrs-server/tests/wire_e2e.rs` | All `setup_broker*` helpers add the `spawn_locks` field to their `SharedState` literal. Two new integration tests. |

No new dependencies. No proto changes. No Python-side changes.

## Versioning

This is a patch release: bump all three crates from 0.3.1 to **0.3.2** in lockstep, matching the project convention. The Python crate has no code changes but bumps to 0.3.2 to stay in lockstep with the broker's release line. Update the three changelogs with 0.3.2 entries.

## Test plan

### Unit tests (`kafkrs-server/src/topic_registry.rs`)

Extend or add a `#[cfg(test)]` module with a test for Fix 1:

```rust
#[tokio::test]
async fn ensure_exists_returns_already_exists_for_existing_topic() {
    // Set up a registry; call EnsureExists("foo", 1) twice.
    // First call: returns Ok(()).
    // Second call: returns Err(RegistryError::AlreadyExists).
}
```

### Integration tests (`kafkrs-server/tests/wire_e2e.rs`)

**1. `auto_create_existing_topic_does_not_respawn`**

Spin up a broker with `auto_create_topics = true`. Send two consecutive Produces for the same topic from one connection. Assert both succeed and offsets increment correctly (first → base 0, second → base 1). Pre-Fix-1 the second produce would re-spawn workers, replacing the handle and orphaning the actor that holds the offset counter — the new handle's writer starts from `next_offset = 0`, breaking offset continuity.

**2. `concurrent_create_topic_same_name_one_wins`**

Spin up `setup_broker_no_topics`. Open two TCP connections in parallel; each sends `CreateTopic("racey", 1)` immediately after Connect. Assert exactly one connection gets `CreateTopicResp` and the other gets `Error(ErrTopicAlreadyExists)`. Then send a `Produce` on either connection and assert success with `base_offset = 0`. Proves Fix 2: even with two concurrent `handle_create_topic` invocations racing on the same topic, only one partition handle exists in the map and produce works against it.

The second test is the closest external observability for "idempotency works" — we can't observe the lock map directly, but the produce-works assertion confirms no orphaning happened.

### What's NOT tested

- **Direct verification of per-key Tokio mutex serialization.** Race-free behavior is inferred from "no observable corruption + memory bound monotonic." A truly direct test would need a count-of-spawned-actors metric or tokio-test instrumentation; neither exists today and adding them solely for this is overkill.
- **`EnsureExists` returning `Ok(())` regression.** The unit test for Fix 1 covers the new behavior directly; the orphan was an emergent consequence and is harder to assert against without instrumentation.

## Out of scope

### Reserved for future work

- **Lock-map cleanup.** Entries in `SharedState.spawn_locks` are never removed in v1. When `DeleteTopic` lands (deferred per the storage spec), remove the partition's lock entries in the same transaction that removes the partition handle. Do NOT add a periodic sweeper or cleanup-on-release path — they add complexity without value. Captured in operator memory (`project_spawn_locks_cleanup.md`).
- **Direct idempotency observability.** A `broker_metrics` surface that exposes spawned-actor counts would let us write a direct "two concurrent spawns ⟹ one actor" assertion. Out of scope for this patch.
- **`AlterTopicConfig`.** If/when this lands, callers must re-evaluate whether `ResolvedTopicConfig` on `PartitionHandle` is still safe to treat as immutable. Currently it is.

### Not in scope at all

- Proto / wire protocol changes (none).
- Python client changes (no API impact; only a version bump to stay in lockstep).
- Schema registry, multi-broker, retention, consumer groups (orthogonal).

## Invariants (for implementers)

1. `EnsureExists`'s reply has the same semantic as `Create`'s reply: `Ok(())` ⟹ this call did the creation; `Err(AlreadyExists)` ⟹ topic existed before this call.
2. `spawn_partition` is idempotent: calling it N times for the same `(topic, partition)` results in exactly one `PartitionHandle` in `state.partitions` and at most one running `PartitionWriter` + `Uploader` actor pair (the first caller wins; subsequent callers no-op).
3. `SharedState.spawn_locks` entries are append-only in v1. Removal happens only with `DeleteTopic` (when it lands).
4. The outer `std::sync::Mutex` on `spawn_locks` is never held across `.await`.
5. The per-key `tokio::sync::Mutex` may be held across `.await` (it is held across the full spawn body) — that's exactly what it's for.
