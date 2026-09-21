# Retention (Segment Deletion) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land per-topic time-based and size-based retention that evicts uploaded Parquet segments and rewrites the partition manifest. Introduces the `RetentionSweeper` for idle partitions and embeds active retention in the Uploader (Option 1.5).

**Architecture:** Two triggers, one code path. After every successful `Uploader::upload_once`, retention runs against the just-updated manifest. A single broker-wide `RetentionSweeper` ticks on `broker.retention_sweep_interval_ms` (default 60 000 ms) and enqueues `UploaderMsg::RetentionKick` to every partition's Uploader. The Uploader remains the sole manifest writer. Manifest is rewritten first, then segments are DELETEd — orphans are accepted as a v1 limitation.

**Tech Stack:** Rust 1.x with `prost` (proto), `tokio::sync::mpsc` + `RwLock` (existing actor plumbing), `tokio::time::interval` (sweeper). No new external dependencies.

**Spec:** `docs/superpowers/specs/2026-09-21-retention-design.md`

---

## File structure

### kafkrs-models (proto + model)
- Modify: `kafkrs-models/proto/wire/v1.proto` — add `retention_ms = 9`, `retention_bytes = 10` to `TopicConfigOverrides`.
- Modify: `kafkrs-models/src/topic.rs` — `DEFAULT_RETENTION_MS`, `DEFAULT_RETENTION_BYTES`, new fields, `resolve()`, unit tests.
- Modify: `kafkrs-models/src/config.rs` — `BrokerConfig.retention_sweep_interval_ms: Option<u64>`, unit tests.

### kafkrs-server (retention engine)
- Modify: `kafkrs-server/src/wire/dispatch.rs` — extend `wire_overrides_to_model` / `model_overrides_to_wire`; add `uploader_tx: mpsc::Sender<UploaderMsg>` to `PartitionHandle`.
- Create: `kafkrs-server/src/retention.rs` — `pub fn evaluate_eviction(&Manifest, &ResolvedTopicConfig, i64) -> Vec<SegmentEntry>` + unit tests.
- Create: `kafkrs-server/src/retention_sweeper.rs` — `RetentionSweeper` actor.
- Modify: `kafkrs-server/src/object_store.rs` — `pub async fn delete(&Arc<...>, &ObjPath) -> Result<()>` + unit test.
- Modify: `kafkrs-server/src/uploader.rs` — `cfg: ResolvedTopicConfig` field on `Uploader`; `Uploader::new` gains `cfg`; `UploaderMsg::RetentionKick`; `retention_pass()` method invoked at end of `Upload` handling and on `RetentionKick`; two new unit tests.
- Modify: `kafkrs-server/src/startup.rs` — clone `utx` before moving into `PartitionWriter::new`; pass `cfg` to `Uploader::new`; insert `uploader_tx` clone into `PartitionHandle`.
- Modify: `kafkrs-server/src/main.rs` — spawn `RetentionSweeper` with `state.partitions.clone()` and resolved sweep interval.
- Modify: `kafkrs-server/src/lib.rs` — `pub mod retention;` and `pub mod retention_sweeper;`.
- Modify: `kafkrs-server/tests/wire_e2e.rs` — update the four `PartitionHandle { ... }` literals and Uploader::new calls in the four fixture helpers; add a new fixture + integration test.

### kafkrs-python (regen only)
- Modify: `kafkrs-python/kafkrs/wire/v1_pb2.py` — regenerated from updated proto.

### Release
- Modify: `kafkrs-models/Cargo.toml`, `kafkrs-server/Cargo.toml`, `kafkrs-python/pyproject.toml`, `kafkrs-python/kafkrs/__init__.py` — bump to `0.4.0`.
- Modify: all three `CHANGELOG.md` files — add `0.4.0` entries.

---

## Task 1: Proto + model additions

**Files:**
- Modify: `kafkrs-models/proto/wire/v1.proto`
- Modify: `kafkrs-models/src/topic.rs`

Follows the additive-proto pattern established in 0.3.1 (`max_fetch_wait_ms = 8`). After this task, the `kafkrs-server` build breaks because the dispatch translation functions have missing fields on the model struct — Task 4 closes the break. Verification here is `cargo test -p kafkrs-models` only.

- [ ] **Step 1: Add proto fields 9 and 10**

Edit `kafkrs-models/proto/wire/v1.proto`. Find `message TopicConfigOverrides`. The block currently ends with `optional uint64 max_fetch_wait_ms = 8;`. Add two lines before the closing brace:

```proto
message TopicConfigOverrides {
  optional uint64 segment_size_bytes        = 1;
  optional uint64 segment_seal_time_ms      = 2;
  optional uint32 max_key_size_bytes        = 3;
  optional uint32 max_value_size_bytes      = 4;
  optional uint64 group_commit_time_ms      = 5;
  optional uint64 group_commit_size_bytes   = 6;
  optional uint32 group_commit_record_count = 7;
  optional uint64 max_fetch_wait_ms         = 8;
  optional int64  retention_ms              = 9;
  optional int64  retention_bytes           = 10;
}
```

Both are signed `int64` to carry the `-1` sentinel.

- [ ] **Step 2: Add default constants and extend the model structs**

Edit `kafkrs-models/src/topic.rs`. Add two new default constants below the existing `DEFAULT_MAX_FETCH_WAIT_MS`:

```rust
pub const DEFAULT_RETENTION_MS: i64 = 7 * 24 * 3600 * 1000; // 7 days
pub const DEFAULT_RETENTION_BYTES: i64 = -1; // no size cap
```

Add fields to `TopicConfigOverrides` (last two fields):

```rust
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct TopicConfigOverrides {
    pub segment_size_bytes: Option<u64>,
    pub segment_seal_time_ms: Option<u64>,
    pub max_key_size_bytes: Option<u32>,
    pub max_value_size_bytes: Option<u32>,
    pub group_commit_time_ms: Option<u64>,
    pub group_commit_size_bytes: Option<usize>,
    pub group_commit_record_count: Option<usize>,
    pub max_fetch_wait_ms: Option<u64>,
    pub retention_ms: Option<i64>,
    pub retention_bytes: Option<i64>,
}
```

Add fields to `ResolvedTopicConfig` (last two fields):

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedTopicConfig {
    pub segment_size_bytes: u64,
    pub segment_seal_time_ms: u64,
    pub max_key_size_bytes: u32,
    pub max_value_size_bytes: u32,
    pub group_commit_time_ms: u64,
    pub group_commit_size_bytes: usize,
    pub group_commit_record_count: usize,
    pub max_fetch_wait_ms: u64,
    pub retention_ms: i64,
    pub retention_bytes: i64,
}
```

Extend `ResolvedTopicConfig::resolve` to populate both new fields at the end:

```rust
impl ResolvedTopicConfig {
    pub fn resolve(o: &TopicConfigOverrides, disk: DiskType) -> ResolvedTopicConfig {
        let p: GroupCommitProfile = disk.group_commit_profile();
        ResolvedTopicConfig {
            segment_size_bytes: o.segment_size_bytes.unwrap_or(DEFAULT_SEGMENT_SIZE_BYTES),
            segment_seal_time_ms: o
                .segment_seal_time_ms
                .unwrap_or(DEFAULT_SEGMENT_SEAL_TIME_MS),
            max_key_size_bytes: o.max_key_size_bytes.unwrap_or(DEFAULT_MAX_KEY_SIZE_BYTES),
            max_value_size_bytes: o
                .max_value_size_bytes
                .unwrap_or(DEFAULT_MAX_VALUE_SIZE_BYTES),
            group_commit_time_ms: o.group_commit_time_ms.unwrap_or(p.time_ms),
            group_commit_size_bytes: o.group_commit_size_bytes.unwrap_or(p.size_bytes),
            group_commit_record_count: o.group_commit_record_count.unwrap_or(p.record_count),
            max_fetch_wait_ms: o.max_fetch_wait_ms.unwrap_or(DEFAULT_MAX_FETCH_WAIT_MS),
            retention_ms: o.retention_ms.unwrap_or(DEFAULT_RETENTION_MS),
            retention_bytes: o.retention_bytes.unwrap_or(DEFAULT_RETENTION_BYTES),
        }
    }
}
```

- [ ] **Step 3: Extend the existing unit tests**

In `kafkrs-models/src/topic.rs`'s `#[cfg(test)] mod tests`, extend `resolved_defaults_when_no_overrides`:

```rust
#[test]
fn resolved_defaults_when_no_overrides() {
    let r = ResolvedTopicConfig::resolve(&TopicConfigOverrides::default(), DiskType::Nvme);
    assert_eq!(r.segment_size_bytes, 128 * 1024 * 1024);
    assert_eq!(r.segment_seal_time_ms, 60_000);
    assert_eq!(r.max_key_size_bytes, 1024);
    assert_eq!(r.max_value_size_bytes, 1024 * 1024);
    assert_eq!(r.group_commit_time_ms, 5); // nvme profile
    assert_eq!(r.group_commit_record_count, 256);
    assert_eq!(r.max_fetch_wait_ms, 60_000);
    assert_eq!(r.retention_ms, 7 * 24 * 3600 * 1000);
    assert_eq!(r.retention_bytes, -1);
}
```

Add a new test asserting overrides win:

```rust
#[test]
fn retention_overrides_win() {
    let o = TopicConfigOverrides {
        retention_ms: Some(-1), // opt out
        retention_bytes: Some(1_000_000_000),
        ..Default::default()
    };
    let r = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
    assert_eq!(r.retention_ms, -1);
    assert_eq!(r.retention_bytes, 1_000_000_000);
}
```

- [ ] **Step 4: Build and test**

Run: `cargo test -p kafkrs-models`
Expected: all tests pass, including the two extended/new tests. The build regenerates the prost-generated Rust types with the two new fields on `TopicConfigOverrides`.

Do NOT run `cargo test -p kafkrs-server` yet — it will fail because dispatch's translation functions don't handle the new fields. Task 4 closes that break.

- [ ] **Step 5: Commit**

Per the operator's policy for this run, subagents commit per task. Do NOT add any `Co-Authored-By` trailer.

```bash
git add kafkrs-models/proto/wire/v1.proto kafkrs-models/src/topic.rs
git commit -m "wire: add retention_ms and retention_bytes to TopicConfigOverrides"
```

---

## Task 2: Regenerate Python protobuf bindings

**Files:**
- Modify: `kafkrs-python/kafkrs/wire/v1_pb2.py`

- [ ] **Step 1: Regenerate via protoc**

From the workspace root:

```bash
protoc --python_out=kafkrs-python/kafkrs \
       --proto_path=kafkrs-models/proto \
       kafkrs-models/proto/wire/v1.proto
```

If `protoc` is missing, install: `brew install protobuf` (macOS).

- [ ] **Step 2: Verify the new fields are accessible**

If the venv exists at `kafkrs-python/.venv`:

```bash
cd kafkrs-python && .venv/bin/python3 -c "from kafkrs.wire import v1_pb2; o = v1_pb2.TopicConfigOverrides(); o.retention_ms = -1; o.retention_bytes = 5000; print(o.retention_ms, o.retention_bytes)"
```

Expected output: `-1 5000`

If the venv is missing:

```bash
cd kafkrs-python && python3 -m venv .venv && .venv/bin/pip install -e ".[dev]"
```

Then re-run the verification.

- [ ] **Step 3: Commit**

```bash
git add kafkrs-python/kafkrs/wire/v1_pb2.py
git commit -m "python: regenerate v1_pb2.py with retention_ms and retention_bytes"
```

---

## Task 3: BrokerConfig gains `retention_sweep_interval_ms`

**Files:**
- Modify: `kafkrs-models/src/config.rs`

- [ ] **Step 1: Add the field with a serde default**

Edit `kafkrs-models/src/config.rs`. Extend `BrokerConfig` with an optional `retention_sweep_interval_ms`. `Option<u64>` with `#[serde(default)]` so an old `config.toml` without the field still parses; resolved at broker startup via `unwrap_or(60_000)`.

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
}
```

Update the `Default` impl to match:

```rust
impl Default for BrokerConfig {
    fn default() -> Self {
        BrokerConfig {
            disk_type: DiskType::default(),
            auto_create_topics: false,
            default_partition_count: default_partition_count(),
            retention_sweep_interval_ms: None,
        }
    }
}
```

- [ ] **Step 2: Extend the existing unit tests**

In `kafkrs-models/src/config.rs`'s `#[cfg(test)] mod tests`, extend `defaults_apply_when_optional_sections_absent`:

```rust
#[test]
fn defaults_apply_when_optional_sections_absent() {
    let toml = r#"
address = "127.0.0.1"
ports = [5432]
data_dir = "./data"
[object_store]
backend = "filesystem"
bucket = "b"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert!(!cfg.broker.auto_create_topics);
    assert_eq!(cfg.broker.default_partition_count, 1);
    assert_eq!(cfg.broker.disk_type, DiskType::Nvme);
    assert_eq!(cfg.broker.retention_sweep_interval_ms, None);
    assert_eq!(cfg.object_store.region, "us-east-1");
}
```

Add a new test asserting an explicit override parses correctly:

```rust
#[test]
fn retention_sweep_interval_ms_parses_when_set() {
    let toml = r#"
address = "127.0.0.1"
ports = [5432]
data_dir = "./data"
[broker]
retention_sweep_interval_ms = 30000
[object_store]
backend = "filesystem"
bucket = "b"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.broker.retention_sweep_interval_ms, Some(30_000));
}
```

- [ ] **Step 3: Build and test**

Run: `cargo test -p kafkrs-models`
Expected: all tests pass, including the two updated/new tests.

- [ ] **Step 4: Commit**

```bash
git add kafkrs-models/src/config.rs
git commit -m "config: add broker.retention_sweep_interval_ms"
```

---

## Task 4: Extend dispatch translation functions

**Files:**
- Modify: `kafkrs-server/src/wire/dispatch.rs`

Closes the kafkrs-server build break introduced by Task 1. Also structural: no behavior change beyond the two new fields being passed through in both directions.

- [ ] **Step 1: Extend `wire_overrides_to_model`**

Edit `kafkrs-server/src/wire/dispatch.rs`. Find `fn wire_overrides_to_model`. Add the two new fields as pass-throughs:

```rust
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
        retention_ms: w.retention_ms,
        retention_bytes: w.retention_bytes,
    }
}
```

- [ ] **Step 2: Extend `model_overrides_to_wire`**

Same file. Find `fn model_overrides_to_wire` and add the two pass-throughs:

```rust
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
        retention_ms: m.retention_ms,
        retention_bytes: m.retention_bytes,
    }
}
```

- [ ] **Step 3: Build and test**

Run: `cargo test -p kafkrs-server`
Expected: all existing tests pass (about 27 lib + 10 wire_e2e + 1 storage_e2e as of 0.3.2).

- [ ] **Step 4: Commit**

```bash
git add kafkrs-server/src/wire/dispatch.rs
git commit -m "wire: pass retention_ms/retention_bytes through overrides translation"
```

---

## Task 5: Pure `evaluate_eviction` function + unit tests

**Files:**
- Create: `kafkrs-server/src/retention.rs`
- Modify: `kafkrs-server/src/lib.rs`

TDD: write the six unit tests first, then implement.

- [ ] **Step 1: Register the new module**

Edit `kafkrs-server/src/lib.rs`. Add `pub mod retention;` in the module list (keep alphabetical order with the existing modules):

```rust
pub mod config;
pub mod fetcher;
pub mod object_store;
pub mod partition_writer;
pub mod recovery;
pub mod retention;
pub mod segment;
pub mod startup;
pub mod topic_registry;
pub mod uploader;
pub mod wal_writer;
pub mod wire;
```

- [ ] **Step 2: Create retention.rs with failing tests + stub function**

Create `kafkrs-server/src/retention.rs`:

```rust
//! Retention policy evaluation. Pure function: given a manifest, resolved
//! per-topic config, and wall-clock now, return the segments to evict.

use kafkrs_models::manifest::{Manifest, SegmentEntry};
use kafkrs_models::topic::ResolvedTopicConfig;

/// Compute the set of segments to evict. Pure. Never returns the last
/// (tail) segment even if it would be eligible.
pub fn evaluate_eviction(
    _manifest: &Manifest,
    _cfg: &ResolvedTopicConfig,
    _now_ns: i64,
) -> Vec<SegmentEntry> {
    unimplemented!("written in Step 4")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kafkrs_models::config::DiskType;
    use kafkrs_models::topic::TopicConfigOverrides;

    fn seg(base: i64, last: i64, last_ts_ns: i64, byte_size: u64) -> SegmentEntry {
        SegmentEntry {
            base_offset: base,
            last_offset: last,
            base_timestamp_ns: last_ts_ns - 1_000_000, // 1ms span
            last_timestamp_ns: last_ts_ns,
            record_count: (last - base + 1) as u64,
            byte_size,
            object_key: format!("segment-{:020}.parquet", base),
        }
    }

    fn cfg(retention_ms: i64, retention_bytes: i64) -> ResolvedTopicConfig {
        let o = TopicConfigOverrides {
            retention_ms: Some(retention_ms),
            retention_bytes: Some(retention_bytes),
            ..Default::default()
        };
        ResolvedTopicConfig::resolve(&o, DiskType::Nvme)
    }

    #[test]
    fn no_eviction_when_all_infinite() {
        let mut m = Manifest::empty("t", 0);
        m.segments = vec![
            seg(0, 99, 1_000_000_000, 100),
            seg(100, 199, 2_000_000_000, 100),
        ];
        let out = evaluate_eviction(&m, &cfg(-1, -1), 10_000_000_000);
        assert!(out.is_empty());
    }

    #[test]
    fn no_eviction_when_manifest_has_one_segment() {
        let mut m = Manifest::empty("t", 0);
        m.segments = vec![seg(0, 99, 1_000_000_000, 100)];
        // Everything says evict, but the tail is never eligible.
        let out = evaluate_eviction(&m, &cfg(1, 1), 10_000_000_000);
        assert!(out.is_empty());
    }

    #[test]
    fn time_based_evicts_expired_segments() {
        let now_ns = 100_000_000_000; // 100s in ns
        let mut m = Manifest::empty("t", 0);
        // Three segments with last_timestamp_ns at 8s, 4s, and 99s.
        m.segments = vec![
            seg(0, 99, 8_000_000_000, 100),
            seg(100, 199, 4_000_000_000_i64 * 25, 100), // 100s? no; use exact numbers below
            seg(200, 299, 99_000_000_000, 100),
        ];
        // Rewrite the middle segment with a clear number: 96s.
        m.segments[1] = seg(100, 199, 96_000_000_000, 100);
        // retention_ms = 10_000 → cutoff at now - 10s = 90s.
        // Segment 0 (last=8s)  → older than cutoff → evict.
        // Segment 1 (last=96s) → newer than cutoff → keep.
        // Segment 2 (last=99s) → tail, never evicted regardless.
        let out = evaluate_eviction(&m, &cfg(10_000, -1), now_ns);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].base_offset, 0);
    }

    #[test]
    fn size_based_evicts_oldest_first() {
        let mut m = Manifest::empty("t", 0);
        m.segments = vec![
            seg(0, 99, 1_000_000_000, 100),
            seg(100, 199, 2_000_000_000, 100),
            seg(200, 299, 3_000_000_000, 100),
        ];
        // total = 300 bytes; cap = 250 → over by 50 → evict oldest 100-byte segment.
        let out = evaluate_eviction(&m, &cfg(-1, 250), 10_000_000_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].base_offset, 0);
    }

    #[test]
    fn either_dimension_triggers_eviction() {
        let now_ns = 100_000_000_000;
        let mut m = Manifest::empty("t", 0);
        // Two 100-byte segments; oldest is age-expired, newest is not,
        // and total size is under cap.
        m.segments = vec![
            seg(0, 99, 1_000_000_000, 100),  // 1s → expired
            seg(100, 199, 99_000_000_000, 100), // 99s → tail
        ];
        // Time cutoff at now - 10s = 90s; segment 0 evicted, segment 1 is tail.
        let out = evaluate_eviction(&m, &cfg(10_000, 1_000_000), now_ns);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].base_offset, 0);
    }

    #[test]
    fn tail_segment_never_evicted() {
        let now_ns = 100_000_000_000;
        let mut m = Manifest::empty("t", 0);
        m.segments = vec![
            seg(0, 99, 1_000_000_000, 100_000),
            seg(100, 199, 2_000_000_000, 100_000), // also old + big → tail, kept.
        ];
        // Both dimensions say evict everything.
        let out = evaluate_eviction(&m, &cfg(10, 1), now_ns);
        // Only segment 0 should evict; segment 1 is the tail.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].base_offset, 0);
    }
}
```

- [ ] **Step 3: Run the tests to confirm they fail with unimplemented!**

Run: `cargo test -p kafkrs-server --lib retention::tests`
Expected: all six tests FAIL with `not implemented: written in Step 4`.

- [ ] **Step 4: Implement `evaluate_eviction`**

Replace the `unimplemented!` body:

```rust
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
            break;
        }
    }
    evict
}
```

- [ ] **Step 5: Run the tests to confirm they pass**

Run: `cargo test -p kafkrs-server --lib retention::tests`
Expected: all six tests PASS.

Run: `cargo test -p kafkrs-server`
Expected: all tests still pass (no regression).

- [ ] **Step 6: Commit**

```bash
git add kafkrs-server/src/retention.rs kafkrs-server/src/lib.rs
git commit -m "retention: pure evaluate_eviction function with unit tests"
```

---

## Task 6: Object-store `delete` helper

**Files:**
- Modify: `kafkrs-server/src/object_store.rs`

- [ ] **Step 1: Write the failing test**

Edit `kafkrs-server/src/object_store.rs`. Find `#[cfg(test)] mod tests`. Add a new test at the end of the test block:

```rust
    #[tokio::test]
    async fn delete_removes_object_and_get_fails_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(
            &ObjectStoreConfig {
                backend: "filesystem".into(),
                bucket: "b".into(),
                prefix: "".into(),
                endpoint: "".into(),
                region: "us-east-1".into(),
            },
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        let key = segment_key("", "t", 0, 42);
        put(&store, &key, Bytes::from_static(b"hi")).await.unwrap();
        assert_eq!(get(&store, &key).await.unwrap(), Bytes::from_static(b"hi"));
        delete(&store, &key).await.unwrap();
        assert!(get(&store, &key).await.is_err());
    }
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test -p kafkrs-server --lib object_store::tests::delete_removes_object_and_get_fails_afterwards`
Expected: FAIL — `delete` is not defined.

- [ ] **Step 3: Add the `delete` helper**

In the same file, add the helper alongside the other put/get/get_range functions:

```rust
pub async fn delete(store: &Arc<dyn ObjectStore>, key: &ObjPath) -> Result<()> {
    store.delete(key).await?;
    Ok(())
}
```

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cargo test -p kafkrs-server --lib object_store::tests::delete_removes_object_and_get_fails_afterwards`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add kafkrs-server/src/object_store.rs
git commit -m "object_store: add delete helper"
```

---

## Task 7: Uploader integration — `cfg` field, `RetentionKick`, `retention_pass()`

**Files:**
- Modify: `kafkrs-server/src/uploader.rs`
- Modify: `kafkrs-server/src/startup.rs`
- Modify: `kafkrs-server/tests/wire_e2e.rs`

Adds `cfg: ResolvedTopicConfig` to the `Uploader`, extends `Uploader::new`'s signature, adds `UploaderMsg::RetentionKick`, and adds the retention pass. Fixture Uploader::new calls in tests get the new argument. Two new Uploader-level tests exercise both trigger paths.

- [ ] **Step 1: Extend `Uploader` and `UploaderMsg`**

Edit `kafkrs-server/src/uploader.rs`. Add the import at the top:

```rust
use kafkrs_models::topic::ResolvedTopicConfig;
```

Extend `UploaderMsg`:

```rust
pub enum UploaderMsg {
    Upload(SealedBatch),
    RetentionKick,
}
```

Extend `Uploader` with `cfg`:

```rust
pub struct Uploader {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    topic: String,
    partition: u32,
    cfg: ResolvedTopicConfig,
    rx: mpsc::Receiver<UploaderMsg>,
    durable_tx: mpsc::Sender<SegmentDurable>,
}
```

Extend `Uploader::new` signature and body (add `cfg: ResolvedTopicConfig` as the 5th parameter, immediately after `partition`):

```rust
impl Uploader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn ObjectStore>,
        prefix: String,
        topic: String,
        partition: u32,
        cfg: ResolvedTopicConfig,
        rx: mpsc::Receiver<UploaderMsg>,
        durable_tx: mpsc::Sender<SegmentDurable>,
    ) -> Uploader {
        Uploader {
            store,
            prefix,
            topic,
            partition,
            cfg,
            rx,
            durable_tx,
        }
    }
```

- [ ] **Step 2: Extend `Uploader::run` to handle `RetentionKick` and to run retention after uploads**

Replace `Uploader::run` with:

```rust
    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                UploaderMsg::Upload(batch) => {
                    // Retry indefinitely: WAL retains the data (spec risk note).
                    loop {
                        match self.upload_once(&batch).await {
                            Ok(()) => break,
                            Err(e) => {
                                log::error!(
                                    "upload failed for base_offset={}: {e:?}; retrying",
                                    batch.base_offset
                                );
                                tokio::time::sleep(std::time::Duration::from_millis(500))
                                    .await;
                            }
                        }
                    }
                    let _ = self
                        .durable_tx
                        .send(SegmentDurable {
                            base_offset: batch.base_offset,
                        })
                        .await;
                    if let Err(e) = self.retention_pass().await {
                        log::warn!(
                            "retention_pass after upload failed for base_offset={}: {e:?}",
                            batch.base_offset
                        );
                    }
                }
                UploaderMsg::RetentionKick => {
                    if let Err(e) = self.retention_pass().await {
                        log::warn!("retention_pass on kick failed: {e:?}");
                    }
                }
            }
        }
    }
```

- [ ] **Step 3: Implement `retention_pass()`**

Add the method inside `impl Uploader` (after `upload_once`):

```rust
    async fn retention_pass(&self) -> Result<()> {
        use crate::object_store::delete;
        use crate::retention::evaluate_eviction;

        let m_key: ObjPath = manifest_key(&self.prefix, &self.topic, self.partition);
        let raw: Bytes = get(&self.store, &m_key).await?;
        let mut manifest: Manifest = serde_json::from_slice(&raw)?;

        let now_ns: i64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let evict = evaluate_eviction(&manifest, &self.cfg, now_ns);
        if evict.is_empty() {
            return Ok(());
        }

        let evict_keys: Vec<String> = evict.iter().map(|s| s.object_key.clone()).collect();
        manifest
            .segments
            .retain(|s| !evict_keys.iter().any(|k| k == &s.object_key));

        // Rewrite manifest FIRST so no fetch can reference a deleted segment.
        let body: Vec<u8> = serde_json::to_vec(&manifest)?;
        put(&self.store, &m_key, Bytes::from(body)).await?;

        // Then delete the segment objects. Orphans on partial failure are
        // accepted (spec §"Manifest-first ordering, orphans accepted").
        for seg in &evict {
            let seg_key: ObjPath =
                segment_key(&self.prefix, &self.topic, self.partition, seg.base_offset);
            if let Err(e) = delete(&self.store, &seg_key).await {
                log::warn!(
                    "delete failed for segment base_offset={}: {e:?}; orphan accepted",
                    seg.base_offset
                );
            }
        }
        Ok(())
    }
```

- [ ] **Step 4: Update `spawn_partition` to pass `cfg` to `Uploader::new`**

Edit `kafkrs-server/src/startup.rs`. Find the `Uploader::new(...)` call (around lines 52-62). Insert `cfg` as the 5th argument:

```rust
    tokio::spawn(
        Uploader::new(
            store.clone(),
            prefix.clone(),
            topic.to_string(),
            partition,
            cfg,
            urx,
            dtx,
        )
        .run(),
    );
```

(`cfg` is already a parameter of `spawn_partition` and is `Copy`, so no clone needed.)

- [ ] **Step 5: Update the four fixture `Uploader::new` calls in wire_e2e.rs**

Edit `kafkrs-server/tests/wire_e2e.rs`. Find all `Uploader::new(...)` calls (there are three of them across the fixture helpers `setup_broker`, `setup_broker_with_max_fetch_wait`, and any others introduced in 0.3.x — use `grep -n "Uploader::new" kafkrs-server/tests/wire_e2e.rs` to enumerate). Each looks like:

```rust
tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, urx, dtx).run());
```

Change to (inserting `cfg` as the 5th argument):

```rust
tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, cfg, urx, dtx).run());
```

`cfg: ResolvedTopicConfig` is already a local variable at that point in each fixture (built via `ResolvedTopicConfig::resolve(&o, DiskType::Nvme)`), so it's in scope. Also update the `storage_e2e.rs` test fixture the same way — `grep -n "Uploader::new" kafkrs-server/tests/storage_e2e.rs` to find the call.

- [ ] **Step 6: Add two Uploader-level integration tests**

In `kafkrs-server/src/uploader.rs`'s `#[cfg(test)] mod tests`, add:

```rust
    fn cfg_with_retention_ms(ms: i64) -> ResolvedTopicConfig {
        use kafkrs_models::config::DiskType;
        use kafkrs_models::topic::TopicConfigOverrides;
        let o = TopicConfigOverrides {
            retention_ms: Some(ms),
            ..Default::default()
        };
        ResolvedTopicConfig::resolve(&o, DiskType::Nvme)
    }

    #[tokio::test]
    async fn upload_then_retention_evicts_expired() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(&fs_cfg(), dir.path().to_str().unwrap()).unwrap();

        // Pre-populate manifest with an old segment (its timestamp is
        // 100 years in the past) and pre-populate its Parquet object.
        let old_seg = SegmentEntry {
            base_offset: 0,
            last_offset: 9,
            base_timestamp_ns: 0,
            last_timestamp_ns: 1_000_000, // 1ms after epoch
            record_count: 10,
            byte_size: 42,
            object_key: "segment-00000000000000000000.parquet".into(),
        };
        let old_key = segment_key("", "t", 0, 0);
        put(&store, &old_key, Bytes::from_static(b"placeholder")).await.unwrap();

        let mut m = Manifest::empty("t", 0);
        m.segments.push(old_seg);
        put(
            &store,
            &manifest_key("", "t", 0),
            Bytes::from(serde_json::to_vec(&m).unwrap()),
        )
        .await
        .unwrap();

        // Spawn Uploader with retention_ms = 1000 (1 second) so the pre-
        // populated segment is old enough to evict.
        let (tx, rx) = mpsc::channel(4);
        let (dtx, mut drx) = mpsc::channel(4);
        let up = Uploader::new(
            store.clone(),
            "".into(),
            "t".into(),
            0,
            cfg_with_retention_ms(1_000),
            rx,
            dtx,
        );
        tokio::spawn(up.run());

        // Send a fresh upload so the new segment becomes the tail; retention
        // then runs against the manifest.
        tx.send(UploaderMsg::Upload(SealedBatch {
            records: vec![rec(10)],
            base_offset: 10,
            last_offset: 10,
            base_timestamp_ns: 999_000_000_000_000_000, // ~year 33_662
            last_timestamp_ns: 999_000_000_000_000_000,
        }))
        .await
        .unwrap();
        drx.recv().await.unwrap();

        // Give the post-upload retention_pass a moment to complete.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let raw = get(&store, &manifest_key("", "t", 0)).await.unwrap();
        let m: Manifest = serde_json::from_slice(&raw).unwrap();
        assert_eq!(m.segments.len(), 1, "old segment should have been evicted");
        assert_eq!(m.segments[0].base_offset, 10);
        // The old object should be gone.
        assert!(get(&store, &old_key).await.is_err(), "old segment object should be deleted");
    }

    #[tokio::test]
    async fn retention_kick_evicts_without_upload() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(&fs_cfg(), dir.path().to_str().unwrap()).unwrap();

        // Two pre-existing segments: one very old, one recent (tail).
        let old_seg = SegmentEntry {
            base_offset: 0,
            last_offset: 9,
            base_timestamp_ns: 0,
            last_timestamp_ns: 1_000_000, // near epoch
            record_count: 10,
            byte_size: 42,
            object_key: "segment-00000000000000000000.parquet".into(),
        };
        let tail_seg = SegmentEntry {
            base_offset: 10,
            last_offset: 19,
            base_timestamp_ns: 999_000_000_000_000_000,
            last_timestamp_ns: 999_000_000_000_000_000,
            record_count: 10,
            byte_size: 42,
            object_key: "segment-00000000000000000010.parquet".into(),
        };
        let old_key = segment_key("", "t", 0, 0);
        put(&store, &old_key, Bytes::from_static(b"placeholder")).await.unwrap();

        let mut m = Manifest::empty("t", 0);
        m.segments.push(old_seg);
        m.segments.push(tail_seg);
        put(
            &store,
            &manifest_key("", "t", 0),
            Bytes::from(serde_json::to_vec(&m).unwrap()),
        )
        .await
        .unwrap();

        let (tx, rx) = mpsc::channel(4);
        let (dtx, _drx) = mpsc::channel(4);
        let up = Uploader::new(
            store.clone(),
            "".into(),
            "t".into(),
            0,
            cfg_with_retention_ms(1_000),
            rx,
            dtx,
        );
        tokio::spawn(up.run());

        tx.send(UploaderMsg::RetentionKick).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let raw = get(&store, &manifest_key("", "t", 0)).await.unwrap();
        let m: Manifest = serde_json::from_slice(&raw).unwrap();
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].base_offset, 10);
        assert!(get(&store, &old_key).await.is_err());
    }
```

- [ ] **Step 7: Build and test**

Run: `cargo build -p kafkrs-server`
Expected: success.

Run: `cargo test -p kafkrs-server`
Expected: all tests pass, including the two new uploader tests.

- [ ] **Step 8: Commit**

```bash
git add kafkrs-server/src/uploader.rs kafkrs-server/src/startup.rs kafkrs-server/tests/wire_e2e.rs kafkrs-server/tests/storage_e2e.rs
git commit -m "uploader: embed retention_pass after upload and on RetentionKick"
```

---

## Task 8: `PartitionHandle.uploader_tx` + startup wiring

**Files:**
- Modify: `kafkrs-server/src/wire/dispatch.rs`
- Modify: `kafkrs-server/src/startup.rs`
- Modify: `kafkrs-server/tests/wire_e2e.rs`

Adds `uploader_tx` to `PartitionHandle` so the RetentionSweeper (Task 9) can enqueue kicks. Structural — no behavior change. `spawn_partition` needs to clone `utx` BEFORE moving it into `PartitionWriter::new` (`startup.rs:92` currently moves it).

- [ ] **Step 1: Add `uploader_tx` to `PartitionHandle`**

Edit `kafkrs-server/src/wire/dispatch.rs`. Update `PartitionHandle`:

```rust
/// Handle to a partition's actor: an mpsc sender for the PartitionWriter,
/// a broadcast sender for tail subscribers, the resolved per-topic config
/// (for wire-layer limit enforcement), and an mpsc sender for the Uploader
/// (used by the RetentionSweeper to enqueue kicks).
#[derive(Clone)]
pub struct PartitionHandle {
    pub pw_tx: mpsc::Sender<PwMsg>,
    pub tail: broadcast::Sender<i64>,
    pub cfg: ResolvedTopicConfig,
    pub uploader_tx: mpsc::Sender<crate::uploader::UploaderMsg>,
}
```

- [ ] **Step 2: Wire `uploader_tx` through `spawn_partition`**

Edit `kafkrs-server/src/startup.rs`. Find where `utx` is created (currently around line 46) and where it's moved into `PartitionWriter::new` (currently around line 92). Clone `utx` before the `PartitionWriter::new` call so the clone can be inserted into `PartitionHandle`.

The current shape (around lines 46-95):

```rust
    let (utx, urx): (mpsc::Sender<UploaderMsg>, mpsc::Receiver<UploaderMsg>) =
        mpsc::channel::<UploaderMsg>(64);
    // ... Uploader spawn ...
    // ... orphan segment loop ...
    let pw = PartitionWriter::new(
        data_dir.to_string(),
        topic.to_string(),
        partition,
        cfg,
        rec.next_offset,
        rec.active_records,
        pw_rx,
        utx,   // ← moved here
        tail.clone(),
    )
    .expect("partition writer");
```

Change the `utx` argument in `PartitionWriter::new` to `utx.clone()`, so the original `utx` remains usable after this call:

```rust
    let pw = PartitionWriter::new(
        data_dir.to_string(),
        topic.to_string(),
        partition,
        cfg,
        rec.next_offset,
        rec.active_records,
        pw_rx,
        utx.clone(),
        tail.clone(),
    )
    .expect("partition writer");
```

Then update the final `partitions.write().await.insert(...)` call to include `uploader_tx: utx`:

```rust
    tokio::spawn(pw.run());
    partitions.write().await.insert(
        (topic.to_string(), partition),
        PartitionHandle {
            pw_tx,
            tail,
            cfg,
            uploader_tx: utx,
        },
    );
}
```

- [ ] **Step 3: Update the four `PartitionHandle { ... }` literals in wire_e2e.rs fixtures**

Edit `kafkrs-server/tests/wire_e2e.rs`. Find every `PartitionHandle { pw_tx, tail, cfg }` literal (use `grep -n "PartitionHandle {" kafkrs-server/tests/wire_e2e.rs` to enumerate — expect four across `setup_broker`, `setup_broker_no_topics`, `setup_broker_auto_create`, and `setup_broker_with_max_fetch_wait`). Each fixture has already declared a `utx` local for its Uploader spawn (or does after Task 7). In each fixture:

- If the fixture spawns an Uploader via `tokio::spawn(Uploader::new(...).run())`, the `utx` is created just above; clone it before spawning and stash a clone in the `PartitionHandle`. Change the spawn to `tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, cfg, urx, dtx).run());` (already done in Task 7) and add `uploader_tx: utx.clone()` to the `PartitionHandle` literal.
- For `setup_broker_no_topics` (which doesn't spawn an Uploader because no partition is pre-created), no `PartitionHandle` literal exists to update. Skip that fixture.

Example for `setup_broker`:

```rust
    partitions
        .write()
        .await
        .insert(
            ("t".into(), 0),
            PartitionHandle {
                pw_tx,
                tail,
                cfg,
                uploader_tx: utx.clone(),
            },
        );
```

To avoid the "moved out of `utx`" compile error, ensure `utx` is `clone()`d wherever it's already being passed elsewhere. The pattern in the fixtures is that `utx` was previously consumed by `tokio::spawn(Uploader::new(..., urx, dtx).run());` — but that call took `urx`, not `utx`. `urx` is the receiver; `utx` is the sender. So `utx` was already unmoved and can be moved into the `PartitionHandle` directly, or cloned if you want a copy for anything else. Simplest: pass `utx: utx` into the `PartitionHandle` literal directly (no clone needed) since `utx` isn't used again in these fixtures.

- [ ] **Step 4: Build and test**

Run: `cargo build -p kafkrs-server`
Expected: success.

Run: `cargo test -p kafkrs-server`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add kafkrs-server/src/wire/dispatch.rs kafkrs-server/src/startup.rs kafkrs-server/tests/wire_e2e.rs
git commit -m "wire: add uploader_tx to PartitionHandle for retention kicks"
```

---

## Task 9: `RetentionSweeper` actor + `main.rs` spawn

**Files:**
- Create: `kafkrs-server/src/retention_sweeper.rs`
- Modify: `kafkrs-server/src/lib.rs`
- Modify: `kafkrs-server/src/main.rs`

- [ ] **Step 1: Register the new module**

Edit `kafkrs-server/src/lib.rs`. Add `pub mod retention_sweeper;` after the `retention` module:

```rust
pub mod config;
pub mod fetcher;
pub mod object_store;
pub mod partition_writer;
pub mod recovery;
pub mod retention;
pub mod retention_sweeper;
pub mod segment;
pub mod startup;
pub mod topic_registry;
pub mod uploader;
pub mod wal_writer;
pub mod wire;
```

- [ ] **Step 2: Create the sweeper**

Create `kafkrs-server/src/retention_sweeper.rs`:

```rust
//! Broker-wide RetentionSweeper. Ticks on a configurable interval; for each
//! known partition, enqueues an UploaderMsg::RetentionKick so idle partitions
//! (those not currently receiving writes) still evict expired segments.

use crate::uploader::UploaderMsg;
use crate::wire::PartitionHandle;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};

pub struct RetentionSweeper {
    partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
    sweep_interval: Duration,
}

impl RetentionSweeper {
    pub fn new(
        partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
        sweep_interval: Duration,
    ) -> RetentionSweeper {
        RetentionSweeper {
            partitions,
            sweep_interval,
        }
    }

    pub async fn run(self) {
        let mut ticker = interval(self.sweep_interval);
        // Skip the immediate first tick: interval() fires immediately on the
        // first .tick() call, which would race with startup. Wait one interval
        // before the first sweep.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let snapshot: Vec<mpsc::Sender<UploaderMsg>> = {
                let guard = self.partitions.read().await;
                guard.values().map(|h| h.uploader_tx.clone()).collect()
            };
            for tx in snapshot {
                // Best-effort; drop the kick if the Uploader is busy.
                // Retention is idempotent so lost kicks are harmless.
                let _ = tx.try_send(UploaderMsg::RetentionKick);
            }
        }
    }
}
```

- [ ] **Step 3: Spawn the sweeper in `main.rs`**

Edit `kafkrs-server/src/main.rs`. Add imports at the top:

```rust
use kafkrs_server::retention_sweeper::RetentionSweeper;
use tokio::time::Duration;
```

Find the `SharedState` construction (currently around lines 74-84). Immediately after `let state: SharedState = SharedState { ... };`, add the sweeper spawn:

```rust
    let sweep_interval = Duration::from_millis(
        cfg.broker.retention_sweep_interval_ms.unwrap_or(60_000),
    );
    tokio::spawn(RetentionSweeper::new(partitions.clone(), sweep_interval).run());
```

- [ ] **Step 4: Build and test**

Run: `cargo build -p kafkrs-server`
Expected: success.

Run: `cargo test -p kafkrs-server`
Expected: all tests pass. The sweeper is not exercised by the existing tests (it only runs when `main` is invoked); the next task adds coverage.

- [ ] **Step 5: Commit**

```bash
git add kafkrs-server/src/retention_sweeper.rs kafkrs-server/src/lib.rs kafkrs-server/src/main.rs
git commit -m "retention_sweeper: broker-wide actor kicks partition Uploaders on interval"
```

---

## Task 10: Integration test `retention_evicts_old_segments_via_sweeper`

**Files:**
- Modify: `kafkrs-server/tests/wire_e2e.rs`

Wall-clock-based end-to-end test. Uses a tight sweep interval and short retention_ms to keep test time bounded.

- [ ] **Step 1: Add a fixture with retention configured**

Edit `kafkrs-server/tests/wire_e2e.rs`. Add a new fixture below the existing `setup_broker_with_max_fetch_wait`. The fixture spins up the sweeper with a tight interval:

```rust
async fn setup_broker_with_retention(
    dd: &str,
    retention_ms: i64,
    sweep_interval_ms: u64,
) -> (u16, Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>) {
    use kafkrs_server::retention_sweeper::RetentionSweeper;
    use tokio::time::Duration;

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
    put(
        &store,
        &manifest_key("", "t", 0),
        Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap()),
    )
    .await
    .unwrap();

    let (utx, urx) = mpsc::channel(64);
    let (dtx, mut drx) = mpsc::channel(64);
    let o = TopicConfigOverrides {
        segment_size_bytes: Some(1), // seal aggressively so retention sees real segments
        group_commit_record_count: Some(1),
        retention_ms: Some(retention_ms),
        ..Default::default()
    };
    let cfg = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
    tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, cfg, urx, dtx).run());
    let (pw_tx, pw_rx) = mpsc::channel(256);
    let (tail, _) = broadcast::channel(1024);
    let pw_tx_d = pw_tx.clone();
    tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let _ = pw_tx_d.send(PwMsg::SegmentDurable(d)).await;
        }
    });
    let pw = PartitionWriter::new(
        dd.into(),
        "t".into(),
        0,
        cfg,
        0,
        vec![],
        pw_rx,
        utx.clone(),
        tail.clone(),
    )
    .unwrap();
    tokio::spawn(pw.run());

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));
    partitions.write().await.insert(
        ("t".into(), 0),
        PartitionHandle {
            pw_tx,
            tail,
            cfg,
            uploader_tx: utx,
        },
    );

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
        auto_create: false,
        default_partition_count: 1,
        data_dir: dd.into(),
        disk_type: DiskType::Nvme,
        spawn_locks: Arc::new(StdMutex::new(HashMap::new())),
    };

    tokio::spawn(
        RetentionSweeper::new(partitions.clone(), Duration::from_millis(sweep_interval_ms))
            .run(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(accept_loop(listener, state));
    (port, partitions)
}
```

- [ ] **Step 2: Add the test**

Append to `kafkrs-server/tests/wire_e2e.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_evicts_old_segments_via_sweeper() {
    use kafkrs_models::wire::v1::ErrorCode;

    let dir = tempfile::tempdir().unwrap();
    // retention_ms = 200, sweep every 100ms.
    let (port, _partitions) =
        setup_broker_with_retention(dir.path().to_str().unwrap(), 200, 100).await;
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

    // Produce 3 records; segment_size_bytes = 1 forces sealing after each.
    for i in 0..3u64 {
        let produce = Command {
            correlation_id: 2 + i,
            body: Some(Body::Produce(ProduceRequest {
                topic: "t".into(),
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
        assert!(matches!(resp.body, Some(Body::ProduceResp(_))));
    }

    // Wait past retention_ms + several sweep intervals so old segments expire
    // and the sweeper has had time to trigger retention on the partition.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    // Produce one more record to keep the tail active and to guarantee
    // the sweeper's kick doesn't race the produce path.
    let produce_tail = Command {
        correlation_id: 100,
        body: Some(Body::Produce(ProduceRequest {
            topic: "t".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 1,
                value_len: 1,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce_tail, b"kv")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::ProduceResp(_))));

    // Fetch from offset 0. Since older segments have been evicted, this
    // should return ErrOffsetOutOfRange.
    let fetch = Command {
        correlation_id: 200,
        body: Some(Body::Fetch(FetchRequest {
            topic: "t".into(),
            partition: 0,
            from_offset: 0,
            max_records: 10,
            max_wait_ms: 0,
        })),
    };
    sock.write_all(&encode(&fetch, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::Error(e)) => {
            assert_eq!(e.code, ErrorCode::ErrOffsetOutOfRange as i32);
        }
        other => panic!("expected Error(ErrOffsetOutOfRange), got {other:?}"),
    }
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p kafkrs-server --test wire_e2e retention_evicts_old_segments_via_sweeper`
Expected: PASS (may take ~1.5-2 seconds due to the deliberate wall-clock waits).

Run full suite:

Run: `cargo test -p kafkrs-server`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add kafkrs-server/tests/wire_e2e.rs
git commit -m "wire: e2e test for retention via RetentionSweeper"
```

---

## Task 11: Version bumps to 0.4.0

**Files:**
- Modify: `kafkrs-models/Cargo.toml`
- Modify: `kafkrs-server/Cargo.toml`
- Modify: `kafkrs-python/pyproject.toml`
- Modify: `kafkrs-python/kafkrs/__init__.py`
- Modify: `Cargo.lock` (regenerated)

- [ ] **Step 1: Bump all four version strings**

- `kafkrs-models/Cargo.toml`: `version = "0.3.2"` → `version = "0.4.0"`
- `kafkrs-server/Cargo.toml`: `version = "0.3.2"` → `version = "0.4.0"`
- `kafkrs-python/pyproject.toml`: under `[project]`, `version = "0.3.2"` → `version = "0.4.0"`
- `kafkrs-python/kafkrs/__init__.py`: `__version__ = "0.3.2"` → `__version__ = "0.4.0"`

- [ ] **Step 2: Regenerate `Cargo.lock`**

Run: `cargo build`
Expected: success; `Cargo.lock` updates with the new versions.

- [ ] **Step 3: Commit**

```bash
git add kafkrs-models/Cargo.toml kafkrs-server/Cargo.toml kafkrs-python/pyproject.toml kafkrs-python/kafkrs/__init__.py Cargo.lock
git commit -m "release: bump all three crates to 0.4.0"
```

---

## Task 12: Update changelogs

**Files:**
- Modify: `kafkrs-models/CHANGELOG.md`
- Modify: `kafkrs-server/CHANGELOG.md`
- Modify: `kafkrs-python/CHANGELOG.md`

- [ ] **Step 1: Prepend 0.4.0 entry to `kafkrs-models/CHANGELOG.md`**

Insert between the preamble and the existing `## [0.3.2]` heading:

```markdown
## [0.4.0] — 2026-09-21

Retention support lands. Additive proto change; see `docs/superpowers/specs/2026-09-21-retention-design.md`.

### Added
- `TopicConfigOverrides.retention_ms` (proto field 9, `optional int64`) and `TopicConfigOverrides.retention_bytes` (proto field 10, `optional int64`). Both use `-1` as an infinite-retention sentinel.
- `ResolvedTopicConfig.retention_ms: i64` and `ResolvedTopicConfig.retention_bytes: i64`.
- `DEFAULT_RETENTION_MS = 7 * 24 * 3600 * 1000` (7 days) and `DEFAULT_RETENTION_BYTES = -1` (no size cap).
- `BrokerConfig.retention_sweep_interval_ms: Option<u64>` (default 60_000 ms when absent).
```

- [ ] **Step 2: Prepend 0.4.0 entry to `kafkrs-server/CHANGELOG.md`**

Insert between the preamble and the existing `## [0.3.2]` heading:

```markdown
## [0.4.0] — 2026-09-21

Retention support: time-based and size-based deletion of uploaded Parquet segments, per-topic. See `docs/superpowers/specs/2026-09-21-retention-design.md`.

**Behaviour change:** with default configuration, segments older than 7 days are now automatically deleted from the object store. Operators upgrading from 0.3.x should review per-topic retention settings.

### Added
- `retention` module with pure `evaluate_eviction(&Manifest, &ResolvedTopicConfig, i64) -> Vec<SegmentEntry>` function.
- `retention_sweeper` module with the broker-wide `RetentionSweeper` actor. Ticks on `broker.retention_sweep_interval_ms` (default 60s) and enqueues `UploaderMsg::RetentionKick` to every partition's Uploader so idle partitions still evict.
- `UploaderMsg::RetentionKick` variant.
- `object_store::delete` helper.
- `Uploader::retention_pass()` method invoked at end of every successful Upload and on RetentionKick. Rewrites manifest first, then DELETEs segment objects.
- `PartitionHandle.uploader_tx` field so the sweeper can enqueue kicks.
- Two integration tests in `tests/wire_e2e.rs`: `retention_evicts_old_segments_via_sweeper`.
- Two Uploader-level tests: `upload_then_retention_evicts_expired`, `retention_kick_evicts_without_upload`.
- Six unit tests in `retention::tests` covering time-based, size-based, either-dimension, tail-never-evicted, and single-segment cases.

### Changed
- `Uploader::new` signature gains `cfg: ResolvedTopicConfig` as the 5th parameter (immediately after `partition`).
- `spawn_partition` clones `utx` before moving it into `PartitionWriter::new` so a clone can be stashed in `PartitionHandle`.

### Not implemented
- Object-store orphan reclamation on partial deletion failure (accepted v1 limitation; documented in the spec).
- Compaction (Kafka's `cleanup.policy=compact`); separate concern for a future spec.
```

- [ ] **Step 3: Prepend 0.4.0 entry to `kafkrs-python/CHANGELOG.md`**

Insert between the preamble and the existing `## [0.3.2]` heading:

```markdown
## [0.4.0] — 2026-09-21

Tracks the broker's 0.4.0 release. See `docs/superpowers/specs/2026-09-21-retention-design.md`.

### Changed
- Regenerated `kafkrs/wire/v1_pb2.py` to include `TopicConfigOverrides.retention_ms` (field 9) and `TopicConfigOverrides.retention_bytes` (field 10). Both are `int64`; `-1` means "no limit on that dimension". Users can now set them when calling `Client.create_topic(...)` with a `v1_pb2.TopicConfigOverrides` argument.
```

- [ ] **Step 4: Commit**

```bash
git add kafkrs-models/CHANGELOG.md kafkrs-server/CHANGELOG.md kafkrs-python/CHANGELOG.md
git commit -m "changelog: 0.4.0 entries for retention"
```

---

## Task 13: Final verification

Verification only — no code changes.

- [ ] **Step 1: Run the full Rust test suite**

Run: `cargo test`
Expected: all tests pass. Counts should be:
- `kafkrs-models` (unit): 19 (17 pre-existing + 1 `retention_overrides_win` + 1 `retention_sweep_interval_ms_parses_when_set`)
- `kafkrs-models` (wire_compile): 2
- `kafkrs-server` (unit): 27 + 6 retention + 1 object_store delete + 2 uploader retention = 36
- `kafkrs-server` (storage_e2e): 1
- `kafkrs-server` (wire_e2e): 10 + 1 retention e2e = 11

Report per-binary pass counts.

- [ ] **Step 2: Run the Python tests**

Run: `cd kafkrs-python && .venv/bin/pytest -v`
Expected: 3 tests pass (Python-side unchanged).

- [ ] **Step 3: Confirm git status is clean**

Run: `git status`
Expected: clean working tree apart from intentionally-untracked spec + plan files.

- [ ] **Step 4: Commit history check**

Run: `git log master..HEAD --oneline`
Expected: 12 new commits on top of the 0.3.2 work, one per Task 1-12.

- [ ] **Step 5: Manual smoke test — retention evicts old data end-to-end**

Terminal 1 (broker with tight retention + tight sweep):

```bash
cd /Users/owilkinson/repos/personal/kafkrs
cargo build --bin kafkrs-server 2>&1 | tail -3
rm -rf /tmp/kafkrs-retention-check && mkdir -p /tmp/kafkrs-retention-check/data
cat > /tmp/kafkrs-retention-check/config.toml <<'EOF'
address = "127.0.0.1"
ports = [15450]
data_dir = "/tmp/kafkrs-retention-check/data"
[broker]
disk_type = "nvme"
auto_create_topics = true
default_partition_count = 1
retention_sweep_interval_ms = 500
[object_store]
backend = "filesystem"
bucket = "test"
prefix = ""
endpoint = ""
region = "us-east-1"
EOF
RUST_LOG=info target/debug/kafkrs-server /tmp/kafkrs-retention-check/config.toml > /tmp/kafkrs-retention-check/broker.log 2>&1 &
BROKER_PID=$!
sleep 1
```

Terminal 2 (drive some traffic, wait past retention, verify eviction):

```bash
cd kafkrs-python && .venv/bin/python3 -c "
import asyncio, time
from kafkrs import Client
from kafkrs.wire import v1_pb2

async def main():
    async with Client('127.0.0.1', 15450) as c:
        # Configure a short retention on this topic explicitly, then produce.
        overrides = v1_pb2.TopicConfigOverrides()
        overrides.segment_size_bytes = 1  # aggressive seal
        overrides.retention_ms = 500       # 500ms
        await c.create_topic('smoke-retention', partition_count=1, overrides=overrides)
        for i in range(3):
            base, last = await c.produce('smoke-retention', 0, [(b'k', b'v')])
            print(f'produced {i}: offsets {base}..{last}')
        # Wait past retention + a few sweep ticks.
        await asyncio.sleep(2)
        # Produce a fresh tail record.
        base, last = await c.produce('smoke-retention', 0, [(b'k', b'v')])
        print(f'produced tail: offsets {base}..{last}')
        # Attempt to fetch offset 0 — should be evicted.
        try:
            recs, hwm = await c.fetch('smoke-retention', 0, from_offset=0, max_wait_ms=200)
            print(f'unexpected: got {len(recs)} records, hwm={hwm}')
        except Exception as e:
            print(f'expected offset-out-of-range: {e}')

asyncio.run(main())
"
```

Then:

```bash
kill $BROKER_PID 2>/dev/null || true
rm -rf /tmp/kafkrs-retention-check
```

Expected output (offsets may vary):

```
produced 0: offsets 0..0
produced 1: offsets 1..1
produced 2: offsets 2..2
produced tail: offsets 3..3
expected offset-out-of-range: wire error 202: ...
```

If offset 0 fetch succeeds instead of returning `ErrOffsetOutOfRange` (code 202), retention isn't running — investigate the broker log at `/tmp/kafkrs-retention-check/broker.log`.

---

## Spec self-review (at plan-writing time)

**Spec coverage.** Every spec requirement maps to a task:

- Proto fields 9 and 10 → Task 1.
- `ResolvedTopicConfig` + `TopicConfigOverrides` + defaults → Task 1.
- Python protobuf regen → Task 2.
- `BrokerConfig.retention_sweep_interval_ms` → Task 3.
- Dispatch translation extensions → Task 4.
- Pure `evaluate_eviction` + 6 unit tests → Task 5.
- `object_store::delete` helper → Task 6.
- `Uploader.cfg`, `UploaderMsg::RetentionKick`, `Uploader::retention_pass`, integration into `run` → Task 7.
- `PartitionHandle.uploader_tx` → Task 8.
- `RetentionSweeper` actor → Task 9.
- Spawn sweeper in `main.rs` from resolved sweep interval → Task 9.
- E2E integration test → Task 10.
- Version bumps 0.4.0 → Task 11.
- Changelog entries → Task 12.
- Manifest-first ordering with orphan tolerance → implemented in Task 7's `retention_pass()` (manifest PUT before segment DELETEs; DELETE failures are log-and-continue).
- Tail-segment-never-evicted invariant → implemented in `evaluate_eviction` (Task 5) and asserted by `tail_segment_never_evicted` and `no_eviction_when_manifest_has_one_segment` tests.

**Placeholder scan.** No "TBD" / "TODO" / "handle edge cases" / "similar to Task N" patterns. Every step shows the actual code.

**Type consistency.**
- `DEFAULT_RETENTION_MS: i64 = 7 * 24 * 3600 * 1000` — consistent across Task 1, Task 5's test helper, and Task 12's changelog.
- `DEFAULT_RETENTION_BYTES: i64 = -1` — consistent.
- `-1` sentinel semantics — consistent.
- `Uploader::new` signature (with `cfg` as 5th positional parameter) — matches across Tasks 7, 8, and 10's fixture.
- `PartitionHandle` field name `uploader_tx: mpsc::Sender<crate::uploader::UploaderMsg>` — consistent across Task 8 (definition), Task 8 (startup insertion), Task 9 (sweeper snapshot), and Task 10 (fixture insertion).
- `evaluate_eviction(&Manifest, &ResolvedTopicConfig, i64) -> Vec<SegmentEntry>` — consistent between Task 5 (definition + tests) and Task 7 (retention_pass caller).
- `RetentionSweeper::new(partitions, sweep_interval: Duration)` — consistent between Task 9 (definition) and Task 10 (test fixture instantiation).

**Open items the implementing engineer should know:**
- Task 5's `time_based_evicts_expired_segments` test has a slight code-style wart: it declares three segments then rewrites the middle one to a clearer number. Left this way to keep the test readable rather than trying to cram all timestamps into `seg(...)` calls; agent can inline or leave as-is.
- Task 7's integration tests use unrealistic timestamps (year ~33_000) to guarantee the "tail" segment is unambiguously newer than the pre-populated old one, regardless of when the test runs. Deliberate.
- Task 8's fixture-update instructions warn about `utx` vs `urx` — `utx` is the Sender (not moved by the existing fixture code) and `urx` is the Receiver (moved into Uploader::new). The subtle bit is that after this task, `utx` gets moved into the `PartitionHandle` literal.
- Task 10's e2e test uses ~1.5s of wall-clock sleep. Kept short enough that it doesn't materially slow CI, generous enough to avoid flake on a busy runner.
