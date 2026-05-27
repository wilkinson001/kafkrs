# `spawn_partition` Idempotency Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the `EnsureExists` semantic that's causing steady-state actor churn on every auto-create produce, plus add per-key idempotency to `spawn_partition` as belt-and-braces defense against future caller mistakes.

**Architecture:** Two-layer defense. Layer 1: `TopicRegistry::EnsureExists` returns `Err(AlreadyExists)` for existing topics (2-line fix in `topic_registry.rs`). Layer 2: `spawn_partition` guards itself with a per-`(topic, partition)` `tokio::sync::Mutex` stored in a new `SharedState.spawn_locks` field; concurrent callers for the same key serialize, and the second caller no-ops after seeing the partition handle already in `state.partitions`. No proto changes; 0.3.2 patch release with the Python crate bumping for lockstep only.

**Tech Stack:** Rust 1.x with `std::sync::Mutex` + `tokio::sync::Mutex` + `tokio::sync::RwLock`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-24-spawn-partition-idempotency-design.md`

---

## File structure

### kafkrs-server
- Modify: `kafkrs-server/src/topic_registry.rs` — Fix 1 (2-line semantic flip); new unit test.
- Modify: `kafkrs-server/src/wire/dispatch.rs` — add `spawn_locks` field to `SharedState`; pass to all `spawn_partition` calls; update misleading comment.
- Modify: `kafkrs-server/src/startup.rs` — `spawn_partition` gains `spawn_locks` parameter; add per-key mutex acquire + idempotency check.
- Modify: `kafkrs-server/src/main.rs` — create `spawn_locks` at boot; pass to boot-time spawns and to `SharedState`.
- Modify: `kafkrs-server/tests/wire_e2e.rs` — update all `setup_broker*` helpers for the `SharedState.spawn_locks` field; add 2 new integration tests.

### Release
- Modify: `kafkrs-models/Cargo.toml`, `kafkrs-server/Cargo.toml`, `kafkrs-python/pyproject.toml`, `kafkrs-python/kafkrs/__init__.py` — bump to `0.3.2`.
- Modify: all three `CHANGELOG.md` files — add `0.3.2` entries.

---

## Task 1: Fix 1 — `EnsureExists` returns `Err(AlreadyExists)` for existing topics

**Files:**
- Modify: `kafkrs-server/src/topic_registry.rs`

This task follows strict TDD: write the failing unit test first, then apply the 2-line fix, then watch it pass.

- [ ] **Step 1: Write the failing unit test**

Edit `kafkrs-server/src/topic_registry.rs`. In the existing `#[cfg(test)] mod tests` block (around line 208), append this test after the existing two tests:

```rust
    #[tokio::test]
    async fn ensure_exists_returns_already_exists_for_existing_topic() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(8);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        // First EnsureExists creates the topic.
        let (r1, rr1) = oneshot::channel();
        tx.send(RegistryMsg::EnsureExists {
            name: "foo".into(),
            partition_count: 1,
            reply: r1,
        })
        .await
        .unwrap();
        assert!(rr1.await.unwrap().is_ok());

        // Second EnsureExists for the same topic must return Err(AlreadyExists),
        // matching Create's semantic. This is what handle_produce's auto-create
        // branch relies on to avoid re-spawning partition workers.
        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::EnsureExists {
            name: "foo".into(),
            partition_count: 1,
            reply: r2,
        })
        .await
        .unwrap();
        assert_eq!(
            rr2.await.unwrap().unwrap_err(),
            RegistryError::AlreadyExists,
        );
    }
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test -p kafkrs-server --lib topic_registry::tests::ensure_exists_returns_already_exists_for_existing_topic`
Expected: FAIL — the second EnsureExists call returns `Ok(())` (not `Err(AlreadyExists)`), so the `unwrap_err()` panics with `called Result::unwrap_err() on an Ok value`.

- [ ] **Step 3: Apply Fix 1**

Edit `kafkrs-server/src/topic_registry.rs`. Find the `EnsureExists` handler (around line 104-114). The current body:

```rust
                RegistryMsg::EnsureExists {
                    name,
                    partition_count,
                    reply,
                } => {
                    let r: Result<(), RegistryError> = if self.topics.contains_key(&name) {
                        Ok(())
                    } else {
                        self.create(&name, partition_count, TopicConfigOverrides::default())
                            .await
                    };
                    let _ = reply.send(r);
                }
```

Replace `Ok(())` (the existing-topic branch) with `Err(RegistryError::AlreadyExists)`:

```rust
                RegistryMsg::EnsureExists {
                    name,
                    partition_count,
                    reply,
                } => {
                    let r: Result<(), RegistryError> = if self.topics.contains_key(&name) {
                        Err(RegistryError::AlreadyExists)
                    } else {
                        self.create(&name, partition_count, TopicConfigOverrides::default())
                            .await
                    };
                    let _ = reply.send(r);
                }
```

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cargo test -p kafkrs-server --lib topic_registry::tests::ensure_exists_returns_already_exists_for_existing_topic`
Expected: PASS.

Then full server suite to confirm no regression:

Run: `cargo test -p kafkrs-server`
Expected: all tests pass (kafkrs-server tests as of 0.3.1 plus the new one).

- [ ] **Step 5: Commit**

For this 9-task subagent-driven execution, the operator has explicitly approved subagents committing per task — RUN the commit. Do NOT add any `Co-Authored-By` trailer.

```bash
git add kafkrs-server/src/topic_registry.rs
git commit -m "registry: EnsureExists returns Err(AlreadyExists) for existing topics"
```

---

## Task 2: Plumbing — `SharedState.spawn_locks` + `spawn_partition` signature + call sites + fixtures

**Files:**
- Modify: `kafkrs-server/src/wire/dispatch.rs`
- Modify: `kafkrs-server/src/startup.rs`
- Modify: `kafkrs-server/src/main.rs`
- Modify: `kafkrs-server/tests/wire_e2e.rs`

Structural change only — adds the parameter and the SharedState field, threads through the call sites, no behavior change yet. Verification is `cargo build` + existing tests pass. The idempotency guard itself lands in Task 3.

- [ ] **Step 1: Add the `PartitionSpawnLocks` type alias + `spawn_locks` field to `SharedState`**

Edit `kafkrs-server/src/wire/dispatch.rs`. At the top of the file, add the two new imports and define the type alias:

```rust
use std::sync::Mutex as StdMutex;
use tokio::sync::Mutex as TokioMutex;

/// Per-(topic, partition) locks coordinating concurrent `spawn_partition`
/// calls. Outer std::sync::Mutex guards the map (held briefly for entry
/// lookup/insert, never across await); per-key tokio::sync::Mutex is held
/// across the full spawn body (which awaits on recovery and channel setup).
pub type PartitionSpawnLocks =
    Arc<StdMutex<HashMap<(String, u32), Arc<TokioMutex<()>>>>>;
```

(Other `std::sync` items like `Arc` and `HashMap` are already imported via existing lines.)

Then find the `SharedState` struct (around line 38). The existing struct ends with `disk_type: DiskType`. Add `spawn_locks` as the last field using the new alias:

```rust
/// Shared state available to every per-connection task.
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

- [ ] **Step 2: Update `spawn_partition` signature in `startup.rs`**

Edit `kafkrs-server/src/startup.rs`. Import the alias from the wire module at the top:

```rust
use crate::wire::dispatch::PartitionSpawnLocks;
```

Change the `spawn_partition` signature to accept the lock map as the last parameter, using the alias:

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
    let _ = &spawn_locks; // unused in Task 2; Task 3 adds the idempotency guard.
    // ... existing body unchanged ...
}
```

Keep the function body unchanged for this task. The `let _ = &spawn_locks;` line suppresses the unused-parameter warning until Task 3 wires it up.

- [ ] **Step 3: Update `main.rs` to create + pass `spawn_locks`**

Edit `kafkrs-server/src/main.rs`. Add imports at the top:

```rust
use std::sync::Mutex as StdMutex;
use std::collections::HashMap;
use kafkrs_server::wire::dispatch::PartitionSpawnLocks;
```

(`HashMap` may already be imported; skip if so. `Arc` is also already in scope.)

Find the section that brings up partitions on boot (around line 55-70, right after `let known = registry.snapshot();`). Allocate `spawn_locks` BEFORE the boot loop:

```rust
    let spawn_locks: PartitionSpawnLocks = Arc::new(StdMutex::new(HashMap::new()));

    // Bring up each known partition independently...
    for (topic, pcount, rtc) in known {
        for p in 0..pcount {
            spawn_partition(
                &cfg.data_dir,
                &topic,
                p,
                rtc,
                store.clone(),
                prefix.clone(),
                partitions.clone(),
                spawn_locks.clone(),
            )
            .await;
        }
    }
```

Then find the `SharedState` construction (around line 75). Add the `spawn_locks` field:

```rust
    let state: SharedState = SharedState {
        partitions: partitions.clone(),
        registry: reg_tx.clone(),
        store: store.clone(),
        prefix: prefix.clone(),
        auto_create: cfg.broker.auto_create_topics,
        default_partition_count: cfg.broker.default_partition_count,
        data_dir: cfg.data_dir.clone(),
        disk_type: cfg.broker.disk_type.clone(),
        spawn_locks: spawn_locks.clone(),
    };
```

- [ ] **Step 4: Pass `spawn_locks` from `handle_create_topic`**

Edit `kafkrs-server/src/wire/dispatch.rs`. In `handle_create_topic`, the `Ok(Ok(())) =>` arm contains a `for p in 0..partition_count` loop with `spawn_partition(...)` inside. Update each call to pass `state.spawn_locks.clone()` as the new last argument:

```rust
                for p in 0..partition_count {
                    spawn_partition(
                        &state.data_dir,
                        &topic_name,
                        p,
                        resolved_cfg,
                        state.store.clone(),
                        state.prefix.clone(),
                        state.partitions.clone(),
                        state.spawn_locks.clone(),
                    )
                    .await;
                }
```

- [ ] **Step 5: Pass `spawn_locks` from `handle_produce` auto-create**

Same file. In `handle_produce`, the auto-create branch has a `for p in 0..state.default_partition_count` loop with `spawn_partition(...)` inside. Update each call to pass `state.spawn_locks.clone()`:

```rust
                for p in 0..state.default_partition_count {
                    spawn_partition(
                        &state.data_dir,
                        &topic,
                        p,
                        cfg,
                        state.store.clone(),
                        state.prefix.clone(),
                        state.partitions.clone(),
                        state.spawn_locks.clone(),
                    )
                    .await;
                }
```

- [ ] **Step 6: Update `wire_e2e.rs` fixtures**

Edit `kafkrs-server/tests/wire_e2e.rs`. There are (currently) three fixture helpers that construct `SharedState`: `setup_broker`, `setup_broker_no_topics`, and `setup_broker_with_max_fetch_wait`. Each needs the `spawn_locks` field added.

Add the import at the top of the file:

```rust
use std::sync::Mutex as StdMutex;
```

(`Arc` and `HashMap` are already imported. The `PartitionSpawnLocks` alias is not referenced by name in the fixture body — the field's type is inferred from the `SharedState` field — so it doesn't need to be imported here.)

For each `SharedState { ... }` literal in the file, add `spawn_locks: Arc::new(StdMutex::new(HashMap::new())),` as the last field. Example:

```rust
    let state = SharedState {
        partitions: partitions.clone(),
        registry: reg_tx,
        store,
        prefix: "".into(),
        auto_create: false,
        default_partition_count: 1,
        data_dir: dd.into(),
        disk_type: DiskType::Nvme,
        spawn_locks: Arc::new(StdMutex::new(HashMap::new())),
    };
```

Use `grep -n 'SharedState {' kafkrs-server/tests/wire_e2e.rs` to find every occurrence (expect three). Each one gets the same single-line addition.

- [ ] **Step 7: Build and test**

Run: `cargo build -p kafkrs-server`
Expected: success.

Run: `cargo test -p kafkrs-server`
Expected: all tests still pass (the structural change doesn't alter behavior).

- [ ] **Step 8: Commit**

```bash
git add kafkrs-server/src/wire/dispatch.rs kafkrs-server/src/startup.rs kafkrs-server/src/main.rs kafkrs-server/tests/wire_e2e.rs
git commit -m "wire: thread spawn_locks through SharedState and spawn_partition"
```

---

## Task 3: Fix 2 — Add per-key idempotency guard inside `spawn_partition`

**Files:**
- Modify: `kafkrs-server/src/startup.rs`

Structural plumbing is done; this task wires the per-key Tokio mutex into the spawn body. No new tests in this task — verification is "existing tests still pass." The integration tests in Tasks 5–6 verify the broader behavior.

- [ ] **Step 1: Add the per-key lock acquire + idempotency check at the top of `spawn_partition`**

Edit `kafkrs-server/src/startup.rs`. Find the top of the `spawn_partition` body (just after the function signature opens with `{`). Replace the `let _ = &spawn_locks;` placeholder from Task 2 with:

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

    // Acquire (or create) the per-key Tokio mutex. The outer std::sync::Mutex
    // is held only briefly here for the entry lookup/insert; never across await.
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

    // ... existing body continues unchanged ...
```

The existing body continues from `let rec = recover_partition(...)` onwards. Do NOT modify it. The final `partitions.write().await.insert(...)` call at the end keeps its original form — it now runs under the `_guard` lock, so the read-then-insert sequence is atomic per-key.

- [ ] **Step 2: Build and test**

Run: `cargo build -p kafkrs-server`
Expected: success.

Run: `cargo test -p kafkrs-server`
Expected: all tests pass (existing behavior unchanged for the common case — the idempotency guard only fires when concurrent callers race, which isn't exercised by current tests).

- [ ] **Step 3: Commit**

```bash
git add kafkrs-server/src/startup.rs
git commit -m "startup: spawn_partition is idempotent via per-key Tokio mutex"
```

---

## Task 4: Fix 3 — Update misleading comment in `handle_produce` auto-create

**Files:**
- Modify: `kafkrs-server/src/wire/dispatch.rs`

- [ ] **Step 1: Replace the comment**

Edit `kafkrs-server/src/wire/dispatch.rs`. In `handle_produce`'s auto-create branch, find the `Ok(Err(RegistryError::AlreadyExists)) =>` arm. The current code:

```rust
            Ok(Err(RegistryError::AlreadyExists)) => { /* partition workers already running */ }
```

Replace with:

```rust
            Ok(Err(RegistryError::AlreadyExists)) => {
                // Topic existed before this produce, so its partition workers were
                // spawned by a prior CreateTopic or EnsureExists call. Nothing to do.
            }
```

- [ ] **Step 2: Verify it builds**

Run: `cargo check -p kafkrs-server`
Expected: success.

- [ ] **Step 3: Commit**

```bash
git add kafkrs-server/src/wire/dispatch.rs
git commit -m "wire: clarify auto-create AlreadyExists arm comment"
```

---

## Task 5: Integration test — `auto_create_existing_topic_does_not_respawn`

**Files:**
- Modify: `kafkrs-server/tests/wire_e2e.rs`

Regression guard against future re-introduction of the EnsureExists bug. Note: this test may pass even *before* Fix 1 because the second produce's recovered state would still produce correct-looking offsets (recovery reads the WAL). It's documented here as a smoke-grade test that locks in the desired end-to-end behavior.

- [ ] **Step 1: Add a setup helper for auto-create-enabled broker**

Edit `kafkrs-server/tests/wire_e2e.rs`. The existing `setup_broker` uses `auto_create: false`. Add a sibling helper that flips auto-create on. Place it after the existing `setup_broker` definition:

```rust
async fn setup_broker_auto_create(dd: &str) -> u16 {
    let store = build_store(
        &ObjectStoreConfig {
            backend: "filesystem".into(),
            bucket: "b".into(),
            prefix: "".into(),
            endpoint: "".into(),
            region: "us-east-1".into(),
        },
        dd,
    )
    .unwrap();

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));

    let (reg_tx, reg_rx) = mpsc::channel(8);
    let registry = TopicRegistry::load(
        dd.into(),
        DiskType::Nvme,
        store.clone(),
        "".into(),
        reg_rx,
    )
    .unwrap();
    tokio::spawn(registry.run());

    let state = SharedState {
        partitions: partitions.clone(),
        registry: reg_tx,
        store,
        prefix: "".into(),
        auto_create: true,
        default_partition_count: 1,
        data_dir: dd.into(),
        disk_type: DiskType::Nvme,
        spawn_locks: Arc::new(StdMutex::new(HashMap::new())),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(accept_loop(listener, state));
    port
}
```

- [ ] **Step 2: Add the test**

Append to `kafkrs-server/tests/wire_e2e.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_create_existing_topic_does_not_respawn() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker_auto_create(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect.
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "t".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // First produce auto-creates "demo" and writes offset 0.
    let produce1 = Command {
        correlation_id: 2,
        body: Some(Body::Produce(ProduceRequest {
            topic: "demo".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 1,
                value_len: 1,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce1, b"ab")).await.unwrap();
    let (resp1, _) = read_frame(&mut sock).await;
    match resp1.body {
        Some(Body::ProduceResp(r)) => assert_eq!(r.base_offset, 0),
        other => panic!("expected ProduceResp, got {other:?}"),
    }

    // Second produce to the SAME topic must not re-spawn workers and must
    // advance the offset to 1.
    let produce2 = Command {
        correlation_id: 3,
        body: Some(Body::Produce(ProduceRequest {
            topic: "demo".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 1,
                value_len: 1,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce2, b"cd")).await.unwrap();
    let (resp2, _) = read_frame(&mut sock).await;
    match resp2.body {
        Some(Body::ProduceResp(r)) => assert_eq!(r.base_offset, 1),
        other => panic!("expected ProduceResp, got {other:?}"),
    }
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p kafkrs-server --test wire_e2e auto_create_existing_topic_does_not_respawn`
Expected: PASS. The test verifies the steady-state behavior: produce 1 → offset 0, produce 2 → offset 1, no errors.

- [ ] **Step 4: Run full suite**

Run: `cargo test -p kafkrs-server`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add kafkrs-server/tests/wire_e2e.rs
git commit -m "wire: regression test for auto-create offset continuity"
```

---

## Task 6: Integration test — `concurrent_create_topic_same_name_one_wins`

**Files:**
- Modify: `kafkrs-server/tests/wire_e2e.rs`

Verifies that two concurrent `CreateTopic` RPCs for the same name produce one success + one `ErrTopicAlreadyExists`, and that a subsequent `Produce` against the topic works. The race is decided at the registry actor layer (already serialized in current code); spawn_partition's idempotency is exercised indirectly by ensuring no orphaning is externally visible.

- [ ] **Step 1: Add the test**

Append to `kafkrs-server/tests/wire_e2e.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_topic_same_name_one_wins() {
    use kafkrs_models::wire::v1::{CreateTopicRequest, ErrorCode};

    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker_no_topics(dir.path().to_str().unwrap()).await;

    // Helper to drive a single CreateTopic and return the resulting Body.
    async fn create(port: u16) -> Option<Body> {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let connect = Command {
            correlation_id: 1,
            body: Some(Body::Connect(ConnectRequest {
                protocol_version: 1,
                client_id: "racer".into(),
                auth_data: vec![],
            })),
        };
        sock.write_all(&encode(&connect, b"")).await.unwrap();
        let _ = read_frame(&mut sock).await;
        let create = Command {
            correlation_id: 2,
            body: Some(Body::CreateTopic(CreateTopicRequest {
                topic: "racey".into(),
                partition_count: 1,
                overrides: None,
            })),
        };
        sock.write_all(&encode(&create, b"")).await.unwrap();
        let (resp, _) = read_frame(&mut sock).await;
        resp.body
    }

    // Fire both CreateTopic RPCs concurrently.
    let (a, b) = tokio::join!(create(port), create(port));

    // Exactly one CreateTopicResp and exactly one Error(ErrTopicAlreadyExists).
    let codes: Vec<_> = [a, b]
        .into_iter()
        .map(|body| match body {
            Some(Body::CreateTopicResp(_)) => "ok",
            Some(Body::Error(e)) if e.code == ErrorCode::ErrTopicAlreadyExists as i32 => {
                "already_exists"
            }
            other => panic!("unexpected response body: {other:?}"),
        })
        .collect();
    let mut sorted = codes.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["already_exists", "ok"]);

    // Produce against the topic — confirms no orphaning is externally visible.
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "producer".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    let produce = Command {
        correlation_id: 2,
        body: Some(Body::Produce(ProduceRequest {
            topic: "racey".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 1,
                value_len: 1,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce, b"kv")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::ProduceResp(r)) => assert_eq!(r.base_offset, 0),
        other => panic!("expected ProduceResp, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p kafkrs-server --test wire_e2e concurrent_create_topic_same_name_one_wins`
Expected: PASS.

- [ ] **Step 3: Run full suite**

Run: `cargo test -p kafkrs-server`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add kafkrs-server/tests/wire_e2e.rs
git commit -m "wire: concurrent CreateTopic race test"
```

---

## Task 7: Version bumps to 0.3.2

**Files:**
- Modify: `kafkrs-models/Cargo.toml`
- Modify: `kafkrs-server/Cargo.toml`
- Modify: `kafkrs-python/pyproject.toml`
- Modify: `kafkrs-python/kafkrs/__init__.py`
- Modify: `Cargo.lock` (regenerated)

- [ ] **Step 1: Bump kafkrs-models**

Edit `kafkrs-models/Cargo.toml`: change `version = "0.3.1"` to `version = "0.3.2"`.

- [ ] **Step 2: Bump kafkrs-server**

Edit `kafkrs-server/Cargo.toml`: change `version = "0.3.1"` to `version = "0.3.2"`.

- [ ] **Step 3: Bump kafkrs-python**

Edit `kafkrs-python/pyproject.toml`: change the `version = "0.3.1"` line under `[project]` to `version = "0.3.2"`.

Edit `kafkrs-python/kafkrs/__init__.py`: change `__version__ = "0.3.1"` to `__version__ = "0.3.2"`.

- [ ] **Step 4: Regenerate Cargo.lock**

Run: `cargo build`
Expected: success. Cargo.lock updates with the new versions.

- [ ] **Step 5: Commit**

```bash
git add kafkrs-models/Cargo.toml kafkrs-server/Cargo.toml kafkrs-python/pyproject.toml kafkrs-python/kafkrs/__init__.py Cargo.lock
git commit -m "release: bump all three crates to 0.3.2"
```

---

## Task 8: Update changelogs

**Files:**
- Modify: `kafkrs-models/CHANGELOG.md`
- Modify: `kafkrs-server/CHANGELOG.md`
- Modify: `kafkrs-python/CHANGELOG.md`

- [ ] **Step 1: Prepend 0.3.2 entry to `kafkrs-models/CHANGELOG.md`**

Insert this block between the preamble and the existing `## [0.3.1]` heading:

```markdown
## [0.3.2] — 2026-05-24

Version bump only — kafkrs-models has no code changes. Stays in lockstep with the broker's 0.3.2 release.
```

- [ ] **Step 2: Prepend 0.3.2 entry to `kafkrs-server/CHANGELOG.md`**

Insert this block between the preamble and the existing `## [0.3.1]` heading:

```markdown
## [0.3.2] — 2026-05-24

Two bug fixes uncovered during the post-0.3.1 review. See `docs/superpowers/specs/2026-05-24-spawn-partition-idempotency-design.md`.

### Fixed
- `TopicRegistry::EnsureExists` now returns `Err(AlreadyExists)` when the topic already exists, matching `Create`'s semantic. Previously it returned `Ok(())`, which caused `handle_produce`'s auto-create branch to re-spawn partition workers on every produce to an existing auto-created topic — orphaning the prior `PartitionWriter` and `Uploader` actors. The actors shut down cleanly (no data loss), but the churn was the steady-state behavior.
- `spawn_partition` is now idempotent: per-key `tokio::sync::Mutex` guards (stored in `SharedState.spawn_locks`) serialize concurrent callers for the same `(topic, partition)`; the second caller sees the partition handle already in `state.partitions` and no-ops. Belt-and-braces defense against future callers that might race through the registry's serialization.

### Changed
- `SharedState` gains a `spawn_locks: PartitionSpawnLocks` field (new public type alias in `wire::dispatch` for `Arc<StdMutex<HashMap<(String, u32), Arc<TokioMutex<()>>>>>`). Lock-map entries are never removed in v1 (cleanup deferred to a future `DeleteTopic` implementation).
- `spawn_partition` signature gains a `spawn_locks` parameter; all three call sites (boot loop, `handle_create_topic`, `handle_produce` auto-create) updated.
- Clarified the misleading comment in `handle_produce`'s auto-create `Err(AlreadyExists)` arm.

### Added
- Two integration tests in `tests/wire_e2e.rs`: `auto_create_existing_topic_does_not_respawn` (regression guard for the EnsureExists fix), `concurrent_create_topic_same_name_one_wins` (external smoke for the idempotency fix).
- Unit test in `topic_registry.rs::tests`: `ensure_exists_returns_already_exists_for_existing_topic`.
```

- [ ] **Step 3: Prepend 0.3.2 entry to `kafkrs-python/CHANGELOG.md`**

Insert this block between the preamble and the existing `## [0.3.1]` heading:

```markdown
## [0.3.2] — 2026-05-24

Version bump only — kafkrs-python has no code changes. Stays in lockstep with the broker's 0.3.2 release.
```

- [ ] **Step 4: Commit**

```bash
git add kafkrs-models/CHANGELOG.md kafkrs-server/CHANGELOG.md kafkrs-python/CHANGELOG.md
git commit -m "changelog: 0.3.2 entries for spawn_partition idempotency"
```

---

## Task 9: Final verification

Verification only — no code changes.

- [ ] **Step 1: Run the full Rust test suite**

Run: `cargo test`
Expected: all tests pass — kafkrs-models unit + wire_compile, kafkrs-server unit (now includes the new EnsureExists test) + storage_e2e + wire_e2e (now 10 tests).

Report pass counts per binary.

- [ ] **Step 2: Run the Python tests**

Run: `cd kafkrs-python && .venv/bin/pytest -v`
Expected: 3 tests pass (no changes vs 0.3.1 — Python wasn't touched).

If `.venv` doesn't exist, create it: `python3 -m venv .venv && .venv/bin/pip install -e ".[dev]"`.

- [ ] **Step 3: Confirm git status is clean**

Run: `git status`
Expected: clean working tree. Stray files (rust_out, stdin, etc.) — remove them.

- [ ] **Step 4: Commit history check**

Run: `git log master..HEAD --oneline`
Expected: 8 new commits on top of the previous 0.3.1 work, one per task above (Tasks 1-8).

- [ ] **Step 5: Manual sanity check — auto-create churn is gone**

Terminal 1 (broker with auto-create):

```bash
cd /Users/owilkinson/repos/personal/kafkrs
cargo build --bin kafkrs-server 2>&1 | tail -3
rm -rf /tmp/kafkrs-idem-check && mkdir -p /tmp/kafkrs-idem-check/data
cat > /tmp/kafkrs-idem-check/config.toml <<'EOF'
address = "127.0.0.1"
ports = [15434]
data_dir = "/tmp/kafkrs-idem-check/data"
[broker]
disk_type = "nvme"
auto_create_topics = true
default_partition_count = 1
[object_store]
backend = "filesystem"
bucket = "test"
prefix = ""
endpoint = ""
region = "us-east-1"
EOF
RUST_LOG=info target/debug/kafkrs-server /tmp/kafkrs-idem-check/config.toml > /tmp/kafkrs-idem-check/broker.log 2>&1 &
BROKER_PID=$!
sleep 1
```

Terminal 2 (drive 5 consecutive produces):

```bash
cd kafkrs-python && .venv/bin/python3 -c "
import asyncio
from kafkrs import Client

async def main():
    async with Client('127.0.0.1', 15434) as c:
        for i in range(5):
            base, last = await c.produce('demo', 0, [(b'k', b'v')])
            print(f'produce {i}: base={base} last={last}')

asyncio.run(main())
"
```

Then:

```bash
kill $BROKER_PID 2>/dev/null || true
rm -rf /tmp/kafkrs-idem-check
```

Expected stdout:

```
produce 0: base=0 last=0
produce 1: base=1 last=1
produce 2: base=2 last=2
produce 3: base=3 last=3
produce 4: base=4 last=4
```

Offsets advance monotonically — proves the partition worker is not being re-spawned (which would either restart the counter or hit a transient broker-not-ready state during recovery).

## Spec self-review (done at plan-writing time)

**Spec coverage:**
- Fix 1 (EnsureExists semantic flip) → Task 1
- Fix 2 (spawn_partition idempotency via per-key Tokio mutex + SharedState.spawn_locks + signature change + 3 call sites) → Tasks 2 + 3
- Fix 3 (misleading-comment cleanup) → Task 4
- Test plan: unit test for Fix 1 → Task 1; integration test #1 → Task 5; integration test #2 → Task 6
- Versioning (0.3.2 across all three crates including Python lockstep) → Task 7
- Changelogs (0.3.2 entries in all three) → Task 8
- Final verification (full test suites + manual smoke) → Task 9

All spec requirements have an implementing task. Nothing in the spec lacks a task.

**Type consistency:**
- `PartitionSpawnLocks` is the spawn_locks type — a public alias defined in `wire::dispatch` for `Arc<StdMutex<HashMap<(String, u32), Arc<TokioMutex<()>>>>>`. Appears identically in Task 2 (struct field), Task 2 (signature), Task 3 (signature in the spawn body code block). Task 5 and Task 6 fixtures construct an `Arc::new(StdMutex::new(HashMap::new()))` value without referencing the alias by name (type is inferred from the `SharedState` field).
- `state.spawn_locks.clone()` pattern — consistent across handle_create_topic and handle_produce.
- `_guard` (Tokio mutex guard) and `lock` (Arc<TokioMutex>) naming — consistent in Task 3.

**Open items the implementing engineer should know:**
- Task 5's test (`auto_create_existing_topic_does_not_respawn`) is documented as a smoke test that may pass even pre-Fix-1 because recovery reads the WAL and reconstructs offsets correctly. The test is a regression guard for the desired end-to-end behavior, not a strict TDD red-green cycle. The unit test in Task 1 is the direct TDD test for the EnsureExists fix.
- Task 6's test (`concurrent_create_topic_same_name_one_wins`) tests the registry actor's serialization more than spawn_partition's idempotency. Spec explicitly acknowledges in "What's NOT tested" that direct verification of the per-key mutex is out of scope without internal instrumentation.
- The `wire_e2e.rs` fixture updates in Task 2 add `spawn_locks` to three `SharedState` literals. Use `grep -n 'SharedState {' kafkrs-server/tests/wire_e2e.rs` to find them all — there should be exactly three as of 0.3.1.
