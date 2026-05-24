# Tier 1 Fixes — Design

**Status:** Draft for review
**Date:** 2026-05-24
**Scope:** Four targeted server-side fixes plus one mandatory Python regeneration. All bundled because they share a single plumbing change (`PartitionHandle` gains a `cfg` field) and a single proto evolution (`TopicConfigOverrides` gains `max_fetch_wait_ms`).

## Motivation

The wire-protocol implementation (0.3.0) left four follow-ups flagged by code review:

1. **`handle_create_topic` doesn't spawn partition workers.** Calling `CreateTopic` explicitly (not via auto-create) registers the topic but never brings up the per-partition `PartitionWriter`/`Uploader` actors. A subsequent `Produce` returns `ERR_UNKNOWN_TOPIC` — confusing and wrong. The auto-create path in `handle_produce` was fixed during the wire-protocol work; the explicit path was missed.

2. **`max_wait_ms` is unbounded.** `handle_fetch` passes the client-supplied `max_wait_ms` directly to the fetcher. A client sending `max_wait_ms = u32::MAX` (~49 days) holds a broker task open for that long. Resource-exhaustion vector once the broker faces any untrusted clients.

3. **`ERR_RECORD_TOO_LARGE` / `ERR_KEY_TOO_LARGE` defined but never emitted.** The proto declares these codes (203, 204) and the per-topic `ResolvedTopicConfig` carries `max_key_size_bytes` / `max_value_size_bytes` (defaults 1 KiB and 1 MiB), but no code path consults them. A client can produce records of any size up to the 4 MiB frame limit, bypassing the per-topic policy.

4. **Per-RPC tasks not aborted on connection teardown.** When a client disconnects, in-flight tasks (e.g., a long-poll `Fetch`) keep running until they try to send on a closed `resp_tx`. Combined with unbounded `max_wait_ms`, a single disconnected client could leak tasks for ~49 days each.

These are hygiene-grade fixes — small individually, valuable in aggregate. They are also the cheapest moment to address them, before more code accretes around the broken paths.

## Design choices, with rationale

### `PartitionHandle` gains `cfg: ResolvedTopicConfig`

Both the size-limit enforcement (fix 3) and the `max_fetch_wait_ms` cap (fix 2) need per-RPC handlers to read per-topic configuration. Today they have no way to.

Three options were considered:
- **Look up via the `TopicRegistry` actor per request.** Adds latency to every produce and fetch; a synchronous round-trip through an mpsc.
- **Cache resolved configs in `SharedState`** behind an `Arc<RwLock<HashMap<String, ResolvedTopicConfig>>>`. Shared, mutable, needs invalidation when topic config changes.
- **Store the resolved config directly on `PartitionHandle`** alongside `pw_tx` and `tail`. Chosen.

The third is chosen because:
- `ResolvedTopicConfig` is `#[derive(Copy)]` — cheap to clone, no synchronization.
- The partition handle is already populated at the same moment the partition is spawned (`startup::spawn_partition`), which is the exact moment the resolved config is computed. Wiring is trivial.
- Per-topic config is immutable in v1 (no `AlterTopicConfig` RPC), so there's no invalidation problem.

### `max_fetch_wait_ms` lives on `TopicConfigOverrides`, not on broker config

Two alternatives were considered: hardcoded constant, or `[broker] max_fetch_wait_ms = ...` in `config.toml`. Per-topic was chosen explicitly to demonstrate the wire-protocol spec's additive-evolution rule: this is the first concrete proto change since 0.3.0, and `TopicConfigOverrides` is the right shape to extend (`optional uint64 max_fetch_wait_ms = 8;` — field 8 is the next free slot).

Default value: 60 000 ms (60 s). Generous enough to never bite a sensibly-behaved client; tight enough to cap resource leaks within the same time horizon as a typical TCP keep-alive.

### Cap is silent (no error)

When `req.max_wait_ms` exceeds `cfg.max_fetch_wait_ms`, the broker silently clamps to the limit. No error, no client-visible feedback. This matches how TCP receive-buffer clamping works and avoids breaking existing clients that pick generous timeouts. A future Cancel RPC would be the right place to surface "your fetch was capped" if it ever becomes operationally important.

### Size limits enforced at the wire layer, not the partition_writer

The size check could live in `handle_produce` (the wire layer, closest to the client) or in `PartitionWriter::handle_produce_msg` (the actor). The wire layer is chosen because:
- It rejects oversized requests before sending anything to the actor — no wasted mpsc queue space.
- The slice-math in `handle_produce` already validates per-record sizes against the payload length; the size cap is one more check in the same place.
- Per-record metadata (`InRecordMeta.key_len`, `.value_len`) is already in hand at this point — no decode needed.

The actor remains the durability boundary; the wire layer remains the validation boundary. Each has one responsibility.

### Per-RPC abort via `AbortHandle` map keyed by `correlation_id`

Two alternatives: `tokio::task::JoinSet` (drops abort all), or per-task `AbortHandle` in a HashMap.

The HashMap is chosen because it future-supports a Cancel RPC: a client sending `Cancel { correlation_id }` would let the dispatcher abort exactly that one task. JoinSet doesn't support selective cancellation. The code cost difference is small (~20 lines either way), so we pay it now for the future flexibility.

Concurrency: the map is shared between the dispatcher loop (inserts on spawn) and per-RPC tasks (remove on completion). Wrapped in `Arc<std::sync::Mutex<HashMap<u64, AbortHandle>>>`. Std mutex is correct here — locks are held briefly (one insert or one remove or one drain), never across await.

## Architecture

No new modules. All changes localized:

```
kafkrs-models/proto/wire/v1.proto        ← add field 8 to TopicConfigOverrides
kafkrs-models/src/topic.rs               ← add max_fetch_wait_ms to overrides + resolved
kafkrs-server/src/wire/dispatch.rs       ← PartitionHandle.cfg; size checks; fetch cap;
                                           handle_create_topic spawn loop;
                                           translation fn updates
kafkrs-server/src/wire/connection.rs     ← AbortHandle map + spawn/remove/drain wiring
kafkrs-server/src/startup.rs             ← populate PartitionHandle.cfg on spawn
kafkrs-python/kafkrs/wire/v1_pb2.py      ← regenerated from updated .proto
```

## The four fixes

### Fix 1: `handle_create_topic` spawns partition workers

In `wire::dispatch::handle_create_topic`, after `RegistryMsg::Create` returns `Ok(Ok(()))` and before returning the success response: resolve `req.overrides` against `state.disk_type` and call `spawn_partition` for each partition `0..req.partition_count`, exactly as the `handle_produce` auto-create branch does.

Difference from the auto-create branch: `handle_create_topic` has the user-supplied overrides in hand, so it passes them through to `ResolvedTopicConfig::resolve`. `handle_produce`'s auto-create uses defaults (it has no access to the resolved overrides) — that's a separate latent bug and out of scope here.

### Fix 2: Cap `max_wait_ms` at per-topic `max_fetch_wait_ms`

**Proto change** (`kafkrs-models/proto/wire/v1.proto`):

```proto
message TopicConfigOverrides {
  optional uint64 segment_size_bytes        = 1;
  optional uint64 segment_seal_time_ms      = 2;
  optional uint32 max_key_size_bytes        = 3;
  optional uint32 max_value_size_bytes      = 4;
  optional uint64 group_commit_time_ms      = 5;
  optional uint64 group_commit_size_bytes   = 6;
  optional uint32 group_commit_record_count = 7;
  optional uint64 max_fetch_wait_ms         = 8;   // NEW
}
```

**Model change** (`kafkrs-models/src/topic.rs`):

- Add `pub const DEFAULT_MAX_FETCH_WAIT_MS: u64 = 60_000;`
- Add `max_fetch_wait_ms: Option<u64>` to `TopicConfigOverrides`.
- Add `max_fetch_wait_ms: u64` to `ResolvedTopicConfig`.
- In `ResolvedTopicConfig::resolve`, populate via `o.max_fetch_wait_ms.unwrap_or(DEFAULT_MAX_FETCH_WAIT_MS)`.

**Dispatch change** (`kafkrs-server/src/wire/dispatch.rs`):

- Extend `wire_overrides_to_model` and `model_overrides_to_wire` with the new field (direct pass-through; both sides are `Option<u64>`).
- In `handle_fetch`, after resolving `handle`, compute:

  ```rust
  let effective_wait = (req.max_wait_ms as u64).min(handle.cfg.max_fetch_wait_ms);
  ```

  and pass `effective_wait` (not `req.max_wait_ms as u64`) to `fetch()`.

### Fix 3: Enforce `max_key_size_bytes` / `max_value_size_bytes`

In `wire::dispatch::handle_produce`, immediately after the `records_meta.is_empty()` guard and before the payload-slicing loop, iterate the metas:

```rust
for m in &records_meta {
    if m.key_len > handle.cfg.max_key_size_bytes {
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
```

Subtlety: this check needs the partition handle, which is resolved later in the current code. Restructure `handle_produce` so partition-handle resolution happens earlier, before payload validation. The auto-create branch still runs before resolution; size limits are checked after auto-create (since the resolved limits live on the partition handle).

### Fix 4: Per-RPC task abort on disconnect

In `wire::connection::run_connection`:

- Allocate `let inflight: Arc<Mutex<HashMap<u64, AbortHandle>>> = Arc::new(Mutex::new(HashMap::new()));` before the dispatcher loop. Use `std::sync::Mutex` (not tokio's).
- In the `(true, Some(body))` arm, change the spawn site to:

  ```rust
  let resp_tx = resp_tx.clone();
  let state = state.clone();
  let payload = frame.payload.clone();
  let inflight = inflight.clone();
  let join = tokio::spawn(async move {
      let response = dispatch_one(cid, body, payload, &state).await;
      let _ = resp_tx.send(response).await;
      inflight.lock().unwrap().remove(&cid);
  });
  inflight.lock().unwrap().insert(cid, join.abort_handle());
  ```

- After the dispatcher loop exits (replacing the existing comment/drop sequence), drain and abort:

  ```rust
  for (_cid, handle) in inflight.lock().unwrap().drain() {
      handle.abort();
  }
  drop(resp_tx);
  let _ = reader.await;
  let _ = writer.await;
  ```

Abort is fire-and-forget (`AbortHandle::abort` is non-blocking). Aborted tasks may have held broadcast subscriptions or oneshot receivers — dropping those is benign; the partition_writer's `tail_tx.send` drops messages with no subscribers, and pending oneshot acks already-sent into `pw_tx` complete normally with their replies discarded.

Remove the existing TODO comment at the spawn site.

### Required Python change

After Fix 2's proto change, regenerate `kafkrs-python/kafkrs/wire/v1_pb2.py` from the updated `.proto`. Commit the regenerated file. No `client.py` changes needed — the new field becomes accessible on `v1_pb2.TopicConfigOverrides` automatically.

Regeneration command (matching the project's existing approach):

```bash
protoc --python_out=kafkrs-python/kafkrs \
       --proto_path=kafkrs-models/proto \
       kafkrs-models/proto/wire/v1.proto
```

(`buf generate` is the intended path but the remote `paths=source_relative` plugin currently rejects the option — `protoc` is the working fallback today.)

## Impact on existing code

| Location | Change |
| --- | --- |
| `kafkrs-models/proto/wire/v1.proto` | Add `optional uint64 max_fetch_wait_ms = 8;` to `TopicConfigOverrides`. Additive — no version bump. |
| `kafkrs-models/src/topic.rs` | Add `DEFAULT_MAX_FETCH_WAIT_MS` constant; add `max_fetch_wait_ms` to both `TopicConfigOverrides` and `ResolvedTopicConfig`; extend `resolve()`. Update existing unit tests. |
| `kafkrs-server/src/wire/dispatch.rs` | Add `cfg: ResolvedTopicConfig` to `PartitionHandle`. Extend `wire_overrides_to_model` / `model_overrides_to_wire`. In `handle_produce`: restructure handle-resolution-then-size-check ordering. In `handle_fetch`: cap `max_wait_ms`. In `handle_create_topic`: spawn partition workers after registry success. |
| `kafkrs-server/src/wire/connection.rs` | Add `AbortHandle` map; modify the spawn site; drain-and-abort on dispatcher exit. Remove the TODO comment. |
| `kafkrs-server/src/startup.rs` | `spawn_partition` already receives `cfg: ResolvedTopicConfig`; pass it through to the `PartitionHandle` it inserts into the map. |
| `kafkrs-python/kafkrs/wire/v1_pb2.py` | Regenerated. |

No new dependencies. No deleted files. No new modules.

## Versioning

These are non-breaking changes (additive proto field; bug fixes; behavior tightening that doesn't break working clients). Bump all three crates from 0.3.0 to **0.3.1** at the end:

- `kafkrs-models/Cargo.toml` → 0.3.1
- `kafkrs-server/Cargo.toml` → 0.3.1
- `kafkrs-python/pyproject.toml` → 0.3.1

Update the three CHANGELOG.md files with a 0.3.1 entry summarizing the fixes.

## Test plan

### Unit tests

`kafkrs-models/src/topic.rs::tests`:
- Extend `resolved_defaults_when_no_overrides` to assert `max_fetch_wait_ms == 60_000`.
- Extend `per_topic_override_wins` (or add a new test) that overrides `max_fetch_wait_ms` and verifies the resolved value.

### Integration tests (`kafkrs-server/tests/wire_e2e.rs`)

Four new test functions:

1. **`create_topic_then_produce_succeeds`** — call `CreateTopic`, then `Produce` to the same topic. Expect `ProduceResp { base_offset: 0, last_offset: 0 }`. Without Fix 1 this returns `ErrUnknownTopic`.

2. **`oversize_key_returns_err_key_too_large`** — Produce with a key larger than the default `max_key_size_bytes` (1 KiB). Expect `Error { code: 204 }` (`ErrKeyTooLarge`). Analogous `oversize_value_returns_err_record_too_large` for a value larger than 1 MiB (code 203 / `ErrRecordTooLarge`).

3. **`fetch_max_wait_ms_is_capped`** — Create a topic with `max_fetch_wait_ms = 100`, Fetch with `max_wait_ms = 5_000`. Time the response using `tokio::time::Instant`; assert it completes within ~300 ms (generous slack for CI flake but well under 5 s).

4. **`disconnect_aborts_inflight_long_poll`** — Connect, start a long-poll Fetch with `max_wait_ms = 30_000`, drop the connection. Wait ~200 ms and confirm the broker is still serving other connections (open a second connection and complete a round-trip). Indirect but adequate. We can't easily observe the abort directly from outside the broker.

### Python integration test (`kafkrs-python/tests/test_client.py`)

One new test:

5. **`test_create_topic_then_produce`** — Configure broker with `auto_create_topics = false`. Connect with the Python client, call `client.create_topic("demo", partition_count=1)`, then `client.produce("demo", 0, [(b"k", b"v")])`. Assert it succeeds (returns `(0, 0)`). Pre-Fix-1 this raises `WireError(200, ...)`.

### Manual smoke test

After the implementation lands and `kafkrs-python` is regenerated:

```python
import asyncio
from kafkrs import Client
from kafkrs.wire import v1_pb2

async def main():
    async with Client("127.0.0.1", 5432) as c:
        overrides = v1_pb2.TopicConfigOverrides()
        overrides.max_fetch_wait_ms = 200
        await c.create_topic("smoke", partition_count=1, overrides=overrides)
        try:
            await c.produce("smoke", 0, [(b"k" * 2048, b"v")])  # 2 KiB key > 1 KiB limit
        except Exception as e:
            print(f"key-too-large: {e}")

asyncio.run(main())
```

Expected: prints `wire error 204: key 2048 bytes exceeds topic limit 1024 bytes`.

## Out of scope

Deliberate non-goals for this batch — to be tracked as separate follow-ups:

- **`handle_produce` auto-create using user overrides.** The auto-create branch in `handle_produce` builds a `ResolvedTopicConfig` from `TopicConfigOverrides::default()` because it doesn't have the persisted overrides for the just-created topic. Fixing this needs the registry to return the resolved config from `EnsureExists`. Separate concern, separate fix.
- **Python `WireError.code` enum re-export.** The followup that would let Python users write `if err.code == ErrorCode.KEY_TOO_LARGE:` instead of `if err.code == 204:`. Operator declined to bundle.
- **CI enforcement of the proto evolution rules.** `buf lint` + `buf breaking` exist as config but aren't wired into CI yet. Separate work tied to the broader CI setup.
- **A `Cancel { correlation_id }` RPC.** The per-task `AbortHandle` map is positioned for it but no RPC exists yet. Awaits a real use case.
- **`max_fetch_wait_ms` enforcement on the long-poll fetcher path inside `fetcher::fetch`.** The cap is applied at the dispatch layer (the only entry point today). If a future code path bypasses dispatch, the cap won't apply — but no such path exists in v1.

## Invariants (for implementers)

1. The wire protocol version remains `1`. No proto removals, no renumbering, no type changes — only the additive `max_fetch_wait_ms` field.
2. `ResolvedTopicConfig` stays `Copy`. If `max_fetch_wait_ms` is added as anything other than a plain `u64`, the `Copy` invariant must be re-justified.
3. The wire-layer validation boundary (size limits + max_wait_ms cap) sits in `wire::dispatch`. The actor-layer durability boundary (WAL fsync gating producer ack) remains in `partition_writer`. Each has one responsibility.
4. The per-RPC `AbortHandle` map is per-connection; one map per connection, dropped when the connection task exits.
