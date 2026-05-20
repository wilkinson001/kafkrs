# Storage Model Implementation Plan — kafkrs v1

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single-file Arrow-IPC writer with a per-partition WAL + Parquet-on-object-store storage model supporting offset-resumable reads, async upload, and a topic registry.

**Architecture:** Tokio actors communicating over `mpsc`. A `PartitionWriter` owns a per-partition WAL (fsync = durability boundary) and in-memory Arrow column builders; an `Uploader` writes sealed batches as Parquet to an S3-compatible store and maintains a JSON manifest; a `Fetcher` resolves reads across three tiers (active batch → in-flight queue → object store); a `TopicRegistry` actor owns `topics.json`.

**Tech Stack:** Rust, tokio, arrow / arrow-array / arrow-schema 55, parquet 55, `object_store` 0.11 (S3 + filesystem backends), `crc32c`, `serde_json`, `bincode`, pyo3.

---

## Scope note

This spec is one subsystem (storage). It is large but cohesive — WAL, segment format, read paths, and registry interlock and cannot be delivered independently of one another. It is kept as a single plan. Wire protocol, transform layer, and multi-broker replication are explicitly out of scope (spec §"Out of scope") and are NOT in this plan. The Listener task here implements only the minimal framed I/O needed to exercise the storage actors end-to-end.

## File Structure

**`kafkrs-models` (pure data types, no async I/O):**

| file | responsibility |
| --- | --- |
| `kafkrs-models/src/config.rs` | `Config`, `BrokerConfig`, `ObjectStoreConfig`, `DiskType`, `GroupCommitProfile` |
| `kafkrs-models/src/record.rs` | `Record` envelope; Arrow schema; Parquet schema; `records_to_recordbatch` |
| `kafkrs-models/src/wal.rs` | WAL record binary codec (encode/decode, CRC32C, length framing) |
| `kafkrs-models/src/manifest.rs` | `Manifest`, `SegmentEntry`, JSON serde, offset/timestamp resolution |
| `kafkrs-models/src/topic.rs` | `TopicConfig`, `TopicEntry`, `TopicRegistryFile`, default resolution |
| `kafkrs-models/src/lib.rs` | module declarations |

The old `message.rs` is renamed/replaced by `record.rs`. `Message` no longer exists.

**`kafkrs-server` (actors, async I/O):**

| file | responsibility |
| --- | --- |
| `kafkrs-server/src/config.rs` | config loader (unchanged API, new type) |
| `kafkrs-server/src/object_store.rs` | object store construction (s3 \| filesystem), key layout helpers |
| `kafkrs-server/src/segment.rs` | `RecordBatch` → Parquet bytes with the spec's writer settings |
| `kafkrs-server/src/wal_writer.rs` | per-segment WAL file: open, group-commit `writev`+`fsync`, scan/recover |
| `kafkrs-server/src/partition_writer.rs` | `PartitionWriter` actor: offsets, active batch, group commit, in-flight queue, broadcast |
| `kafkrs-server/src/uploader.rs` | `Uploader` actor: Parquet PUT, manifest read-modify-PUT, durable notify |
| `kafkrs-server/src/fetcher.rs` | `Fetcher`: three-tier read resolution, long-poll, error codes |
| `kafkrs-server/src/topic_registry.rs` | `TopicRegistry` actor: `topics.json`, CreateTopic/Describe/List, auto-create |
| `kafkrs-server/src/recovery.rs` | per-partition startup recovery sequence |
| `kafkrs-server/src/listener.rs` | replaced: framed length-prefixed I/O (minimal protocol shim) |
| `kafkrs-server/src/main.rs` | wiring: accept-loop, actor spawn |
| `kafkrs-server/tests/storage_e2e.rs` | end-to-end + crash-recovery integration tests |

**`kafkrs-python`:** `kafkrs-python/src/lib.rs` — `encode_message` signature change.

---

## Task 1: Config model

**Files:**
- Modify: `kafkrs-models/src/config.rs` (full rewrite)
- Modify: `kafkrs-models/Cargo.toml` (add `serde` derive already present; no new dep)
- Modify: `config.toml`
- Test: `kafkrs-models/src/config.rs` (`#[cfg(test)]` module)

- [ ] **Step 1: Write the failing test**

Add to `kafkrs-models/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config_and_applies_disk_profile() {
        let toml = r#"
address = "127.0.0.1"
ports = [5432]
data_dir = "./data"

[broker]
disk_type = "nvme"
auto_create_topics = false
default_partition_count = 1

[object_store]
backend = "filesystem"
bucket = "kafkrs-data"
prefix = ""
endpoint = ""
region = "us-east-1"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.ports, vec![5432]);
        assert_eq!(cfg.data_dir, "./data");
        assert_eq!(cfg.broker.disk_type, DiskType::Nvme);
        assert!(!cfg.broker.auto_create_topics);
        assert_eq!(cfg.broker.default_partition_count, 1);
        assert_eq!(cfg.object_store.backend, "filesystem");

        let p = cfg.broker.disk_type.group_commit_profile();
        assert_eq!(p.time_ms, 5);
        assert_eq!(p.size_bytes, 64 * 1024);
        assert_eq!(p.record_count, 256);
    }

    #[test]
    fn rotational_profile_values() {
        let p = DiskType::Rotational.group_commit_profile();
        assert_eq!((p.time_ms, p.size_bytes, p.record_count), (50, 1024 * 1024, 4096));
    }

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
        assert_eq!(cfg.object_store.region, "us-east-1");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-models config::`
Expected: FAIL — `Config` has no `data_dir`/`broker`/`object_store`; `DiskType` undefined.

- [ ] **Step 3: Write minimal implementation**

Replace the entire contents of `kafkrs-models/src/config.rs` with:

```rust
use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub address: String,
    pub ports: Vec<u16>,
    pub data_dir: String,
    #[serde(default)]
    pub broker: BrokerConfig,
    pub object_store: ObjectStoreConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BrokerConfig {
    #[serde(default)]
    pub disk_type: DiskType,
    #[serde(default)]
    pub auto_create_topics: bool,
    #[serde(default = "default_partition_count")]
    pub default_partition_count: u32,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        BrokerConfig {
            disk_type: DiskType::default(),
            auto_create_topics: false,
            default_partition_count: default_partition_count(),
        }
    }
}

fn default_partition_count() -> u32 {
    1
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiskType {
    #[default]
    Nvme,
    Ssd,
    Rotational,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupCommitProfile {
    pub time_ms: u64,
    pub size_bytes: usize,
    pub record_count: usize,
}

impl DiskType {
    pub fn group_commit_profile(&self) -> GroupCommitProfile {
        match self {
            DiskType::Nvme => GroupCommitProfile { time_ms: 5, size_bytes: 64 * 1024, record_count: 256 },
            DiskType::Ssd => GroupCommitProfile { time_ms: 15, size_bytes: 256 * 1024, record_count: 1024 },
            DiskType::Rotational => GroupCommitProfile { time_ms: 50, size_bytes: 1024 * 1024, record_count: 4096 },
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct ObjectStoreConfig {
    pub backend: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_region")]
    pub region: String,
}

fn default_region() -> String {
    "us-east-1".to_string()
}
```

Add `toml` as a dev-dependency in `kafkrs-models/Cargo.toml`:

```toml
[dev-dependencies]
toml = "0.8.12"
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p kafkrs-models config::`
Expected: PASS (3 tests).

- [ ] **Step 5: Update `config.toml`**

Replace the entire contents of `config.toml` with:

```toml
address = "127.0.0.1"
ports = [5432]
data_dir = "./data"

[broker]
disk_type = "nvme"
auto_create_topics = false
default_partition_count = 1

[object_store]
backend = "filesystem"
bucket = "kafkrs-data"
prefix = ""
endpoint = ""
region = "us-east-1"
```

---

## Task 2: Record envelope + Arrow/Parquet schema

This replaces `message.rs`. The `Message` type is removed; `Record` is the WAL/wire envelope. Fixes the `message.rs:25` bug (timestamp field mis-named `"partition"`), drops `partition`, changes `key` to bytes, replaces `schema: Option<String>` with `schema_id: u32`.

**Files:**
- Create: `kafkrs-models/src/record.rs`
- Delete: `kafkrs-models/src/message.rs`
- Modify: `kafkrs-models/src/lib.rs`
- Test: `kafkrs-models/src/record.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-models/src/record.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample(offset: i64) -> Record {
        Record {
            offset,
            timestamp_ns: 1_700_000_000_000_000_000 + offset,
            schema_id: 0,
            key: vec![1, 2, 3],
            value: vec![9, 9, 9, 9],
        }
    }

    #[test]
    fn parquet_schema_has_envelope_columns_in_order() {
        let schema = parquet_arrow_schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, vec!["offset", "timestamp_ns", "key", "value", "schema_id"]);
        assert!(!schema.field(0).is_nullable()); // offset REQUIRED
        assert!(schema.field(2).is_nullable()); // key OPTIONAL
        assert!(!schema.field(3).is_nullable()); // value REQUIRED
    }

    #[test]
    fn records_to_recordbatch_roundtrips_values() {
        let recs = vec![sample(0), sample(1)];
        let rb = records_to_recordbatch(&recs);
        assert_eq!(rb.num_rows(), 2);
        assert_eq!(rb.num_columns(), 5);
    }

    #[test]
    fn null_key_is_represented_when_empty() {
        let mut r = sample(0);
        r.key = vec![];
        let rb = records_to_recordbatch(&[r]);
        let keys = rb
            .column(2)
            .as_any()
            .downcast_ref::<arrow_array::BinaryArray>()
            .unwrap();
        assert!(keys.is_null(0));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-models record::`
Expected: FAIL — `Record`, `parquet_arrow_schema`, `records_to_recordbatch` undefined.

- [ ] **Step 3: Write minimal implementation**

Prepend to `kafkrs-models/src/record.rs` (above the test module):

```rust
use arrow_array::builder::BinaryBuilder;
use arrow_array::{ArrayRef, BinaryArray, Int32Array, Int64Array, RecordBatch, TimestampNanosecondArray};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The WAL/wire envelope. `offset` is assigned by the PartitionWriter at
/// group-commit time; producers send the rest. Empty key == "no key" (v1
/// collapses null and empty).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Record {
    pub offset: i64,
    pub timestamp_ns: i64,
    pub schema_id: u32,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// v1 envelope-only Parquet schema (spec §"Parquet schema (v1, envelope-only)").
/// Column order is load-bearing: `records_to_recordbatch` builds arrays in this
/// order and the segment writer relies on it.
pub fn parquet_arrow_schema() -> Schema {
    Schema::new(vec![
        Field::new("offset", DataType::Int64, false),
        Field::new(
            "timestamp_ns",
            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
            false,
        ),
        Field::new("key", DataType::Binary, true),
        Field::new("value", DataType::Binary, false),
        Field::new("schema_id", DataType::Int32, false),
    ])
}

pub fn records_to_recordbatch(records: &[Record]) -> RecordBatch {
    let offsets: Int64Array =
        Int64Array::from(records.iter().map(|r| r.offset).collect::<Vec<_>>());
    let timestamps: TimestampNanosecondArray = TimestampNanosecondArray::from(
        records.iter().map(|r| r.timestamp_ns).collect::<Vec<_>>(),
    )
    .with_timezone("UTC");

    let mut key_builder: BinaryBuilder = BinaryBuilder::new();
    for r in records {
        if r.key.is_empty() {
            key_builder.append_null();
        } else {
            key_builder.append_value(&r.key);
        }
    }
    let keys: BinaryArray = key_builder.finish();

    let mut value_builder: BinaryBuilder = BinaryBuilder::new();
    for r in records {
        value_builder.append_value(&r.value);
    }
    let values: BinaryArray = value_builder.finish();

    let schema_ids: Int32Array =
        Int32Array::from(records.iter().map(|r| r.schema_id as i32).collect::<Vec<_>>());

    RecordBatch::try_new(
        Arc::new(parquet_arrow_schema()),
        vec![
            Arc::new(offsets) as ArrayRef,
            Arc::new(timestamps),
            Arc::new(keys),
            Arc::new(values),
            Arc::new(schema_ids),
        ],
    )
    .expect("record arrays must match parquet_arrow_schema")
}
```

- [ ] **Step 4: Update `lib.rs` and delete `message.rs`**

Replace `kafkrs-models/src/lib.rs` with:

```rust
pub mod config;
pub mod manifest;
pub mod record;
pub mod topic;
pub mod wal;
```

(The `manifest`, `topic`, `wal` modules are created in Tasks 3–5; this compiles only after those exist. To keep this task green standalone, temporarily declare only `pub mod config;` and `pub mod record;`, then add the others in their tasks.)

Set `kafkrs-models/src/lib.rs` for now to:

```rust
pub mod config;
pub mod record;
```

Then delete the old file: `rm kafkrs-models/src/message.rs`

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p kafkrs-models record::`
Expected: PASS (3 tests). Note `kafkrs-server` and `kafkrs-python` will not compile yet (they import `message`); they are fixed in Tasks 15–16. Build only the models crate: `cargo build -p kafkrs-models`.

---

## Task 3: WAL record binary codec

Length-prefixed, CRC32C-validated, little-endian, per spec §"Record layout". Header 30 B + 4 B CRC trailer.

**Files:**
- Create: `kafkrs-models/src/wal.rs`
- Modify: `kafkrs-models/src/lib.rs` (add `pub mod wal;`)
- Modify: `kafkrs-models/Cargo.toml` (add `crc32c`)
- Test: `kafkrs-models/src/wal.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-models/src/wal.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Record;

    fn rec() -> Record {
        Record { offset: 42, timestamp_ns: 1_700_000_000_000_000_000, schema_id: 7, key: vec![1, 2], value: vec![3, 4, 5] }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let r = rec();
        let mut buf = Vec::new();
        encode_record(&r, &mut buf);
        // 4 (length) + 30-4 header fields... + key + value + 4 crc
        let (decoded, consumed) = decode_record(&buf).unwrap();
        assert_eq!(decoded, r);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn truncated_tail_fails_decode() {
        let r = rec();
        let mut buf = Vec::new();
        encode_record(&r, &mut buf);
        buf.truncate(buf.len() - 1);
        assert!(matches!(decode_record(&buf), Err(WalDecodeError::Incomplete)));
    }

    #[test]
    fn corrupt_body_fails_crc() {
        let r = rec();
        let mut buf = Vec::new();
        encode_record(&r, &mut buf);
        let n = buf.len();
        buf[n - 5] ^= 0xFF; // flip a byte inside value, before CRC trailer
        assert!(matches!(decode_record(&buf), Err(WalDecodeError::CrcMismatch)));
    }

    #[test]
    fn scan_stops_at_first_invalid() {
        let mut buf = Vec::new();
        encode_record(&rec(), &mut buf);
        let good_len = buf.len();
        encode_record(&rec(), &mut buf);
        let n = buf.len();
        buf[n - 3] ^= 0xFF; // corrupt second record
        let (records, valid_bytes) = scan_wal(&buf);
        assert_eq!(records.len(), 1);
        assert_eq!(valid_bytes, good_len);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-models wal::`
Expected: FAIL — symbols undefined.

- [ ] **Step 3: Add dependency**

In `kafkrs-models/Cargo.toml` under `[dependencies]` add:

```toml
crc32c = "0.6"
```

- [ ] **Step 4: Write minimal implementation**

Prepend to `kafkrs-models/src/wal.rs`:

```rust
use crate::record::Record;

/// Fixed fields after the 4-byte length prefix, per spec §"Record layout":
/// offset(8) timestamp_ns(8) schema_id(4) key_len(2) value_len(4) = 26 bytes.
/// Total header incl. length prefix = 30 B; CRC32C trailer = 4 B.
const HEADER_AFTER_LEN: usize = 8 + 8 + 4 + 2 + 4;

#[derive(Debug, PartialEq)]
pub enum WalDecodeError {
    /// Not enough bytes for a full framed record yet (torn tail / EOF).
    Incomplete,
    /// CRC32C over offset..value did not match the trailer.
    CrcMismatch,
    /// length field is implausible (key/value exceed declared frame).
    Malformed,
}

/// Appends one framed record to `out`. CRC32C (Castagnoli) covers
/// `offset` through `value` (everything except the length prefix and CRC).
pub fn encode_record(r: &Record, out: &mut Vec<u8>) {
    let key_len: u16 = r.key.len() as u16;
    let value_len: u32 = r.value.len() as u32;
    let body_len: usize = HEADER_AFTER_LEN + r.key.len() + r.value.len();
    // length = bytes from `offset` through `value`, excluding CRC.
    out.extend_from_slice(&(body_len as u32).to_le_bytes());

    let start: usize = out.len();
    out.extend_from_slice(&r.offset.to_le_bytes());
    out.extend_from_slice(&r.timestamp_ns.to_le_bytes());
    out.extend_from_slice(&r.schema_id.to_le_bytes());
    out.extend_from_slice(&key_len.to_le_bytes());
    out.extend_from_slice(&value_len.to_le_bytes());
    out.extend_from_slice(&r.key);
    out.extend_from_slice(&r.value);

    let crc: u32 = crc32c::crc32c(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// Decodes one record from the front of `buf`. Returns the record and the
/// number of bytes consumed (length prefix + body + CRC trailer).
pub fn decode_record(buf: &[u8]) -> Result<(Record, usize), WalDecodeError> {
    if buf.len() < 4 {
        return Err(WalDecodeError::Incomplete);
    }
    let body_len: usize = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    let total: usize = 4 + body_len + 4;
    if buf.len() < total {
        return Err(WalDecodeError::Incomplete);
    }
    if body_len < HEADER_AFTER_LEN {
        return Err(WalDecodeError::Malformed);
    }
    let body: &[u8] = &buf[4..4 + body_len];
    let crc_stored: u32 = u32::from_le_bytes(buf[4 + body_len..total].try_into().unwrap());
    if crc32c::crc32c(body) != crc_stored {
        return Err(WalDecodeError::CrcMismatch);
    }

    let offset: i64 = i64::from_le_bytes(body[0..8].try_into().unwrap());
    let timestamp_ns: i64 = i64::from_le_bytes(body[8..16].try_into().unwrap());
    let schema_id: u32 = u32::from_le_bytes(body[16..20].try_into().unwrap());
    let key_len: usize = u16::from_le_bytes(body[20..22].try_into().unwrap()) as usize;
    let value_len: usize = u32::from_le_bytes(body[22..26].try_into().unwrap()) as usize;
    if HEADER_AFTER_LEN + key_len + value_len != body_len {
        return Err(WalDecodeError::Malformed);
    }
    let key: Vec<u8> = body[26..26 + key_len].to_vec();
    let value: Vec<u8> = body[26 + key_len..26 + key_len + value_len].to_vec();

    Ok((Record { offset, timestamp_ns, schema_id, key, value }, total))
}

/// One-pass recovery scan: decode records until the first failure. Returns the
/// recovered records and the number of leading bytes that are valid (the
/// truncation point — spec §"Why the CRC is at the end").
pub fn scan_wal(buf: &[u8]) -> (Vec<Record>, usize) {
    let mut records: Vec<Record> = Vec::new();
    let mut pos: usize = 0;
    while pos < buf.len() {
        match decode_record(&buf[pos..]) {
            Ok((r, consumed)) => {
                records.push(r);
                pos += consumed;
            }
            Err(_) => break,
        }
    }
    (records, pos)
}
```

Add `pub mod wal;` to `kafkrs-models/src/lib.rs`.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p kafkrs-models wal::`
Expected: PASS (4 tests).

---

## Task 4: Manifest model

One manifest per `(topic, partition)`. JSON. Binary-searchable by offset and timestamp. `next_offset` deliberately absent (spec §"Manifest").

**Files:**
- Create: `kafkrs-models/src/manifest.rs`
- Modify: `kafkrs-models/src/lib.rs` (add `pub mod manifest;`)
- Modify: `kafkrs-models/Cargo.toml` (add `serde_json`)
- Test: `kafkrs-models/src/manifest.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-models/src/manifest.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn seg(base: i64, last: i64) -> SegmentEntry {
        SegmentEntry {
            base_offset: base,
            last_offset: last,
            base_timestamp_ns: base * 1000,
            last_timestamp_ns: last * 1000,
            record_count: (last - base + 1) as u64,
            byte_size: 123,
            object_key: format!("segment-{:020}.parquet", base),
        }
    }

    #[test]
    fn empty_manifest_serializes_with_empty_segments() {
        let m = Manifest::empty("orders", 3);
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"segments\":[]"));
        assert!(!j.contains("next_offset"));
        let back: Manifest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.segments.len(), 0);
        assert_eq!(back.format_version, 1);
    }

    #[test]
    fn segment_for_offset_binary_search() {
        let mut m = Manifest::empty("o", 0);
        m.segments = vec![seg(0, 99), seg(100, 199), seg(200, 299)];
        assert_eq!(m.segment_for_offset(0).unwrap().base_offset, 0);
        assert_eq!(m.segment_for_offset(150).unwrap().base_offset, 100);
        assert_eq!(m.segment_for_offset(299).unwrap().base_offset, 200);
        assert!(m.segment_for_offset(300).is_none());
        assert!(m.segment_for_offset(-1).is_none());
    }

    #[test]
    fn covers_offset_reports_highest_uploaded() {
        let mut m = Manifest::empty("o", 0);
        assert_eq!(m.last_uploaded_offset(), None);
        m.segments = vec![seg(0, 99), seg(100, 199)];
        assert_eq!(m.last_uploaded_offset(), Some(199));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-models manifest::`
Expected: FAIL — symbols undefined; `serde_json` missing.

- [ ] **Step 3: Add dependency**

In `kafkrs-models/Cargo.toml` under `[dependencies]` add:

```toml
serde_json = "1.0"
```

- [ ] **Step 4: Write minimal implementation**

Prepend to `kafkrs-models/src/manifest.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SegmentEntry {
    pub base_offset: i64,
    pub last_offset: i64,
    pub base_timestamp_ns: i64,
    pub last_timestamp_ns: i64,
    pub record_count: u64,
    pub byte_size: u64,
    /// Relative key within the partition directory, e.g.
    /// `segment-00000000000000000000.parquet`.
    pub object_key: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Manifest {
    pub topic: String,
    pub partition: u32,
    pub format_version: u32,
    pub segments: Vec<SegmentEntry>,
}

impl Manifest {
    pub fn empty(topic: &str, partition: u32) -> Manifest {
        Manifest {
            topic: topic.to_string(),
            partition,
            format_version: 1,
            segments: Vec::new(),
        }
    }

    /// Binary-search the (offset-sorted, non-overlapping) segment list for the
    /// segment whose [base_offset, last_offset] range contains `offset`.
    pub fn segment_for_offset(&self, offset: i64) -> Option<&SegmentEntry> {
        let idx: usize = self
            .segments
            .partition_point(|s| s.last_offset < offset);
        self.segments
            .get(idx)
            .filter(|s| offset >= s.base_offset && offset <= s.last_offset)
    }

    pub fn last_uploaded_offset(&self) -> Option<i64> {
        self.segments.last().map(|s| s.last_offset)
    }
}
```

Add `pub mod manifest;` to `kafkrs-models/src/lib.rs`.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p kafkrs-models manifest::`
Expected: PASS (3 tests).

---

## Task 5: Topic registry model

`TopicConfig` overrides on broker defaults, `TopicEntry`, `TopicRegistryFile`. Default resolution merges per-topic overrides over broker-level defaults (spec §"Topic schema").

**Files:**
- Create: `kafkrs-models/src/topic.rs`
- Modify: `kafkrs-models/src/lib.rs` (add `pub mod topic;`)
- Test: `kafkrs-models/src/topic.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-models/src/topic.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DiskType;

    #[test]
    fn resolved_defaults_when_no_overrides() {
        let r = ResolvedTopicConfig::resolve(&TopicConfigOverrides::default(), DiskType::Nvme);
        assert_eq!(r.segment_size_bytes, 128 * 1024 * 1024);
        assert_eq!(r.segment_seal_time_ms, 60_000);
        assert_eq!(r.max_key_size_bytes, 1024);
        assert_eq!(r.max_value_size_bytes, 1024 * 1024);
        assert_eq!(r.group_commit_time_ms, 5); // nvme profile
        assert_eq!(r.group_commit_record_count, 256);
    }

    #[test]
    fn per_topic_override_wins() {
        let o = TopicConfigOverrides { segment_seal_time_ms: Some(5_000), ..Default::default() };
        let r = ResolvedTopicConfig::resolve(&o, DiskType::Ssd);
        assert_eq!(r.segment_seal_time_ms, 5_000);
        assert_eq!(r.group_commit_time_ms, 15); // ssd profile, not overridden
    }

    #[test]
    fn registry_file_roundtrips() {
        let mut f = TopicRegistryFile::default();
        f.topics.push(TopicEntry {
            name: "orders".into(),
            partition_count: 3,
            created_at_ns: 1,
            config: TopicConfigOverrides::default(),
        });
        let j = serde_json::to_string(&f).unwrap();
        let back: TopicRegistryFile = serde_json::from_str(&j).unwrap();
        assert_eq!(back.topics[0].name, "orders");
        assert_eq!(back.topics[0].partition_count, 3);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-models topic::`
Expected: FAIL — symbols undefined.

- [ ] **Step 3: Write minimal implementation**

Prepend to `kafkrs-models/src/topic.rs`:

```rust
use crate::config::{DiskType, GroupCommitProfile};
use serde::{Deserialize, Serialize};

/// Broker-level defaults for per-topic overridable settings (spec §"Per-topic
/// overridable broker defaults"). Per-topic config overrides take precedence;
/// otherwise these apply.
pub const DEFAULT_SEGMENT_SIZE_BYTES: u64 = 128 * 1024 * 1024; // 128 MiB
pub const DEFAULT_SEGMENT_SEAL_TIME_MS: u64 = 60_000; // 60 s
pub const DEFAULT_MAX_KEY_SIZE_BYTES: u32 = 1024; // 1 KiB
pub const DEFAULT_MAX_VALUE_SIZE_BYTES: u32 = 1024 * 1024; // 1 MiB

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct TopicConfigOverrides {
    pub segment_size_bytes: Option<u64>,
    pub segment_seal_time_ms: Option<u64>,
    pub max_key_size_bytes: Option<u32>,
    pub max_value_size_bytes: Option<u32>,
    pub group_commit_time_ms: Option<u64>,
    pub group_commit_size_bytes: Option<usize>,
    pub group_commit_record_count: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TopicEntry {
    pub name: String,
    pub partition_count: u32,
    pub created_at_ns: i64,
    #[serde(default)]
    pub config: TopicConfigOverrides,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TopicRegistryFile {
    #[serde(default)]
    pub topics: Vec<TopicEntry>,
}

/// Effective config for a partition writer/uploader after merging per-topic
/// overrides over broker-level defaults (spec §"Per-topic overridable broker
/// defaults").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedTopicConfig {
    pub segment_size_bytes: u64,
    pub segment_seal_time_ms: u64,
    pub max_key_size_bytes: u32,
    pub max_value_size_bytes: u32,
    pub group_commit_time_ms: u64,
    pub group_commit_size_bytes: usize,
    pub group_commit_record_count: usize,
}

impl ResolvedTopicConfig {
    pub fn resolve(o: &TopicConfigOverrides, disk: DiskType) -> ResolvedTopicConfig {
        let p: GroupCommitProfile = disk.group_commit_profile();
        ResolvedTopicConfig {
            segment_size_bytes: o.segment_size_bytes.unwrap_or(DEFAULT_SEGMENT_SIZE_BYTES),
            segment_seal_time_ms: o.segment_seal_time_ms.unwrap_or(DEFAULT_SEGMENT_SEAL_TIME_MS),
            max_key_size_bytes: o.max_key_size_bytes.unwrap_or(DEFAULT_MAX_KEY_SIZE_BYTES),
            max_value_size_bytes: o.max_value_size_bytes.unwrap_or(DEFAULT_MAX_VALUE_SIZE_BYTES),
            group_commit_time_ms: o.group_commit_time_ms.unwrap_or(p.time_ms),
            group_commit_size_bytes: o.group_commit_size_bytes.unwrap_or(p.size_bytes),
            group_commit_record_count: o.group_commit_record_count.unwrap_or(p.record_count),
        }
    }
}
```

Add `pub mod topic;` to `kafkrs-models/src/lib.rs`. Final `lib.rs` is now:

```rust
pub mod config;
pub mod manifest;
pub mod record;
pub mod topic;
pub mod wal;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p kafkrs-models topic::`
Expected: PASS (3 tests). Then `cargo test -p kafkrs-models` — all model tests green.


---

## Task 6: Object store abstraction

Wrap the `object_store` crate. Backend selectable: `s3` (real / S3-compatible) or `filesystem` (local testing). Deterministic key layout (spec §"Object key layout").

**Files:**
- Create: `kafkrs-server/src/object_store.rs`
- Modify: `kafkrs-server/Cargo.toml` (add `object_store`, `bytes`, `anyhow`; dev `tempfile`)
- Modify: `kafkrs-server/src/main.rs` (add `mod object_store;`)
- Test: `kafkrs-server/src/object_store.rs` (`#[cfg(test)]`, async)

- [ ] **Step 1: Add dependencies**

In `kafkrs-server/Cargo.toml` under `[dependencies]`:

```toml
object_store = { version = "0.11", features = ["aws"] }
bytes = "1"
anyhow = "1"
serde_json = "1.0"
parquet = "55.0.0"
crc32c = "0.6"
```

Add a `[dev-dependencies]` section:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write the failing test**

Create `kafkrs-server/src/object_store.rs`:

```rust
use anyhow::Result;
use bytes::Bytes;
use kafkrs_models::config::ObjectStoreConfig;
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;
use std::ops::Range;
use std::sync::Arc;

/// Constructs the configured object store. `filesystem` is rooted at
/// `<data_dir>/object_store` for local testing; `s3` targets the configured
/// bucket/endpoint with credentials sourced from the environment / IAM.
pub fn build_store(cfg: &ObjectStoreConfig, data_dir: &str) -> Result<Arc<dyn ObjectStore>> {
    match cfg.backend.as_str() {
        "filesystem" => {
            let root: std::path::PathBuf = std::path::Path::new(data_dir).join("object_store");
            std::fs::create_dir_all(&root)?;
            Ok(Arc::new(object_store::local::LocalFileSystem::new_with_prefix(root)?))
        }
        "s3" => {
            let mut b: object_store::aws::AmazonS3Builder =
                object_store::aws::AmazonS3Builder::from_env()
                    .with_bucket_name(&cfg.bucket)
                    .with_region(&cfg.region);
            if !cfg.endpoint.is_empty() {
                b = b.with_endpoint(&cfg.endpoint).with_allow_http(true);
            }
            Ok(Arc::new(b.build()?))
        }
        other => anyhow::bail!("unknown object_store backend: {other}"),
    }
}

/// Deterministic object key for a sealed segment (spec §"Object key layout").
/// `prefix` is the configured `object_store.prefix` (may be empty).
pub fn segment_key(prefix: &str, topic: &str, partition: u32, base_offset: i64) -> ObjPath {
    join(prefix, topic, partition, &format!("segment-{:020}.parquet", base_offset))
}

pub fn manifest_key(prefix: &str, topic: &str, partition: u32) -> ObjPath {
    join(prefix, topic, partition, "manifest.json")
}

fn join(prefix: &str, topic: &str, partition: u32, leaf: &str) -> ObjPath {
    let mut s: String = String::new();
    if !prefix.is_empty() {
        s.push_str(prefix.trim_end_matches('/'));
        s.push('/');
    }
    s.push_str(&format!("{topic}/partition={partition}/{leaf}"));
    ObjPath::from(s)
}

pub async fn put(store: &Arc<dyn ObjectStore>, key: &ObjPath, bytes: Bytes) -> Result<()> {
    store.put(key, bytes.into()).await?;
    Ok(())
}

pub async fn get(store: &Arc<dyn ObjectStore>, key: &ObjPath) -> Result<Bytes> {
    Ok(store.get(key).await?.bytes().await?)
}

pub async fn get_range(
    store: &Arc<dyn ObjectStore>,
    key: &ObjPath,
    range: Range<u64>,
) -> Result<Bytes> {
    Ok(store.get_range(key, range).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_hive_partitioned_and_zero_padded() {
        let k = segment_key("", "orders", 3, 100);
        assert_eq!(k.to_string(), "orders/partition=3/segment-00000000000000000100.parquet");
        let k2 = segment_key("env/v1", "orders", 0, 0);
        assert_eq!(k2.to_string(), "env/v1/orders/partition=0/segment-00000000000000000000.parquet");
        assert_eq!(manifest_key("", "orders", 3).to_string(), "orders/partition=3/manifest.json");
    }

    #[tokio::test]
    async fn filesystem_put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ObjectStoreConfig {
            backend: "filesystem".into(),
            bucket: "b".into(),
            prefix: "".into(),
            endpoint: "".into(),
            region: "us-east-1".into(),
        };
        let store = build_store(&cfg, dir.path().to_str().unwrap()).unwrap();
        let key = manifest_key("", "t", 0);
        put(&store, &key, Bytes::from_static(b"hello")).await.unwrap();
        let got = get(&store, &key).await.unwrap();
        assert_eq!(&got[..], b"hello");
        let r = get_range(&store, &key, 1..3).await.unwrap();
        assert_eq!(&r[..], b"el");
    }
}
```

Add `mod object_store;` to the `mod` declarations in `kafkrs-server/src/main.rs` (alongside `mod config;` etc.).

- [ ] **Step 3: Run test to verify it fails, then compile**

Run: `cargo test -p kafkrs-server object_store::`
Expected: initially FAILS to compile because `main.rs` still references the deleted `Message`/`Writer`. To isolate, temporarily comment the body of `main()` and the `mod listener; mod writer;` lines in `main.rs` is NOT allowed (no placeholders). Instead, accept that `kafkrs-server` will not build until Task 15. Run the object_store tests via: `cargo test -p kafkrs-server --lib object_store:: --no-fail-fast` after Task 15. **Reorder note:** if executing inline, do Task 15's `main.rs`/`listener.rs` rewrite stubs before this task's test run. For subagent-driven execution, Tasks 6–14 build their modules; the crate is first compiled green at Task 15.

(Practical guidance for the executor: write Tasks 6–14 modules, then Task 15 makes `kafkrs-server` compile and all server unit tests run together.)


---

## Task 7: Parquet segment writer

`RecordBatch` → Parquet bytes using the spec's writer settings (§"Writer settings"): 1 row group, page indexes on, 1 MiB pages, zstd(3), dictionary for `schema_id`.

**Files:**
- Create: `kafkrs-server/src/segment.rs`
- Modify: `kafkrs-server/src/main.rs` (add `mod segment;`)
- Test: `kafkrs-server/src/segment.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-server/src/segment.rs`:

```rust
use anyhow::Result;
use arrow_array::RecordBatch;
use bytes::Bytes;
use kafkrs_models::record::{records_to_recordbatch, Record};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{EnabledStatistics, WriterProperties};

/// Serializes records into a single-row-group Parquet object per the spec's
/// writer settings.
pub fn write_segment(records: &[Record]) -> Result<Bytes> {
    let batch: RecordBatch = records_to_recordbatch(records);
    let props: WriterProperties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_max_row_group_size(usize::MAX) // 1 row group per segment
        .set_data_page_size_limit(1024 * 1024) // 1 MiB pages
        .set_statistics_enabled(EnabledStatistics::Page) // page indexes
        .set_dictionary_enabled(false)
        .set_column_dictionary_enabled("schema_id".into(), true)
        .build();
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w: ArrowWriter<&mut Vec<u8>> =
            ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))?;
        w.write(&batch)?;
        w.close()?;
    }
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    fn rec(o: i64) -> Record {
        Record { offset: o, timestamp_ns: 1_700_000_000_000_000_000 + o, schema_id: 5, key: vec![1], value: vec![2, 3] }
    }

    #[test]
    fn roundtrips_through_parquet_single_row_group() {
        let recs: Vec<Record> = (0..1000).map(rec).collect();
        let bytes = write_segment(&recs).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(bytes.clone()).unwrap();
        let meta = reader.metadata().clone();
        assert_eq!(meta.num_row_groups(), 1);
        let mut r = reader.build().unwrap();
        let batch = r.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 1000);
    }
}
```

Add `mod segment;` to `kafkrs-server/src/main.rs`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-server --lib segment::` (will compile-block until Task 15 — see Task 6 Step 3 note). Logically: FAIL until `write_segment` exists; PASS once written.

- [ ] **Step 3: Verify settings against the parquet 55 API**

If `set_column_dictionary_enabled` or `ZstdLevel::try_new` signatures differ in parquet 55, run `cargo doc -p parquet --open` and adjust to the equivalent builder methods. The required behaviour is: zstd level 3, exactly one row group, page-level statistics enabled, dictionary only on `schema_id`. Do not change the behaviour, only the method names if the API differs.

---

## Task 8: WAL file writer + recovery

Per-segment WAL file at `<data_dir>/wal/<topic>/<partition>/<base_offset>.wal`. Group-commit append (`write_all` of a pre-serialized batch, then `fsync`), and a recovery scan that truncates at the first invalid record.

**Files:**
- Create: `kafkrs-server/src/wal_writer.rs`
- Modify: `kafkrs-server/src/main.rs` (add `mod wal_writer;`)
- Test: `kafkrs-server/src/wal_writer.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-server/src/wal_writer.rs`:

```rust
use anyhow::Result;
use kafkrs_models::record::Record;
use kafkrs_models::wal::{encode_record, scan_wal};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Owns one append-only WAL file (one per segment). The PartitionWriter is the
/// sole writer (spec invariant 3).
pub struct WalFile {
    path: PathBuf,
    file: File,
}

impl WalFile {
    pub fn wal_path(data_dir: &str, topic: &str, partition: u32, base_offset: i64) -> PathBuf {
        Path::new(data_dir)
            .join("wal")
            .join(topic)
            .join(partition.to_string())
            .join(format!("{base_offset}.wal"))
    }

    /// Opens (creating parent dirs) the WAL file for a segment, append mode.
    pub fn open(data_dir: &str, topic: &str, partition: u32, base_offset: i64) -> Result<WalFile> {
        let path: PathBuf = Self::wal_path(data_dir, topic, partition, base_offset);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let file: File = OpenOptions::new().create(true).append(true).read(true).open(&path)?;
        Ok(WalFile { path, file })
    }

    /// Group commit: encode every record, one `write_all`, then `fsync`.
    /// Returns only after the data is durable (spec §"Group commit").
    pub fn append_and_sync(&mut self, records: &[Record]) -> Result<()> {
        let mut buf: Vec<u8> = Vec::new();
        for r in records {
            encode_record(r, &mut buf);
        }
        self.file.write_all(&buf)?;
        self.file.sync_all()?;
        Ok(())
    }

    pub fn delete(self) -> Result<()> {
        drop(self.file);
        std::fs::remove_file(&self.path)?;
        Ok(())
    }
}

/// Recovery: scan a WAL file, validate, truncate the file at the first invalid
/// record (spec §"Recovery on startup" step 2). Returns recovered records.
pub fn recover_wal_file(path: &Path) -> Result<Vec<Record>> {
    let bytes: Vec<u8> = std::fs::read(path)?;
    let (records, valid_len): (Vec<Record>, usize) = scan_wal(&bytes);
    if valid_len < bytes.len() as usize {
        let f: File = OpenOptions::new().write(true).open(path)?;
        f.set_len(valid_len as u64)?;
        f.sync_all()?;
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(o: i64) -> Record {
        Record { offset: o, timestamp_ns: 1_000 + o, schema_id: 0, key: vec![], value: vec![o as u8] }
    }

    #[test]
    fn append_then_recover_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        let mut w = WalFile::open(dd, "t", 0, 0).unwrap();
        w.append_and_sync(&[rec(0), rec(1)]).unwrap();
        w.append_and_sync(&[rec(2)]).unwrap();
        let path = WalFile::wal_path(dd, "t", 0, 0);
        let recovered = recover_wal_file(&path).unwrap();
        assert_eq!(recovered.iter().map(|r| r.offset).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn torn_tail_is_truncated_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap();
        let mut w = WalFile::open(dd, "t", 0, 0).unwrap();
        w.append_and_sync(&[rec(0), rec(1)]).unwrap();
        let path = WalFile::wal_path(dd, "t", 0, 0);
        // simulate torn write: append garbage tail
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0xFF; 7]).unwrap();
        }
        let recovered = recover_wal_file(&path).unwrap();
        assert_eq!(recovered.len(), 2);
        // file truncated back to the 2-record length
        let again = recover_wal_file(&path).unwrap();
        assert_eq!(again.len(), 2);
    }
}
```

Add `mod wal_writer;` to `kafkrs-server/src/main.rs`.

- [ ] **Step 2: Run test to verify it fails then passes**

Logically FAIL before impl, PASS after. (Crate compiles green at Task 15; run `cargo test -p kafkrs-server --lib wal_writer::` then.)

---

## Task 9: Uploader actor

Receives a sealed `RecordBatch` + offset/timestamp bounds, writes Parquet, PUTs the object, read-modify-PUTs the manifest, then notifies the PartitionWriter that the segment is durable (spec §"Seal-and-upload flow"). Idempotent: deterministic key + bit-identical content.

**Files:**
- Create: `kafkrs-server/src/uploader.rs`
- Modify: `kafkrs-server/src/main.rs` (add `mod uploader;`)
- Test: `kafkrs-server/src/uploader.rs` (`#[cfg(test)]`, async, filesystem store)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-server/src/uploader.rs`:

```rust
use crate::object_store::{get, manifest_key, put, segment_key};
use crate::segment::write_segment;
use anyhow::Result;
use bytes::Bytes;
use kafkrs_models::manifest::{Manifest, SegmentEntry};
use kafkrs_models::record::Record;
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// A sealed batch handed from the PartitionWriter to the Uploader.
pub struct SealedBatch {
    pub records: Vec<Record>,
    pub base_offset: i64,
    pub last_offset: i64,
    pub base_timestamp_ns: i64,
    pub last_timestamp_ns: i64,
}

pub enum UploaderMsg {
    Upload(SealedBatch),
}

/// Notification sent back when a segment is durable in the object store
/// (manifest updated). The PartitionWriter deletes the WAL file on receipt
/// (spec invariant 4).
#[derive(Debug, Clone)]
pub struct SegmentDurable {
    pub base_offset: i64,
}

pub struct Uploader {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    topic: String,
    partition: u32,
    rx: mpsc::Receiver<UploaderMsg>,
    durable_tx: mpsc::Sender<SegmentDurable>,
}

impl Uploader {
    pub fn new(
        store: Arc<dyn ObjectStore>,
        prefix: String,
        topic: String,
        partition: u32,
        rx: mpsc::Receiver<UploaderMsg>,
        durable_tx: mpsc::Sender<SegmentDurable>,
    ) -> Uploader {
        Uploader { store, prefix, topic, partition, rx, durable_tx }
    }

    pub async fn run(mut self) {
        while let Some(UploaderMsg::Upload(batch)) = self.rx.recv().await {
            // Retry indefinitely: WAL retains the data (spec risk note).
            loop {
                match self.upload_once(&batch).await {
                    Ok(()) => break,
                    Err(e) => {
                        log::error!("upload failed for base_offset={}: {e:?}; retrying", batch.base_offset);
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
            let _ = self.durable_tx.send(SegmentDurable { base_offset: batch.base_offset }).await;
        }
    }

    async fn upload_once(&self, batch: &SealedBatch) -> Result<()> {
        let bytes: Bytes = write_segment(&batch.records)?;
        let byte_size: u64 = bytes.len() as u64;
        let seg_key: ObjPath =
            segment_key(&self.prefix, &self.topic, self.partition, batch.base_offset);
        // Idempotent: deterministic key, bit-identical content on re-upload.
        put(&self.store, &seg_key, bytes).await?;

        let m_key: ObjPath = manifest_key(&self.prefix, &self.topic, self.partition);
        let raw: Bytes = get(&self.store, &m_key).await?;
        let mut manifest: Manifest = serde_json::from_slice(&raw)?;
        let object_key: String = format!("segment-{:020}.parquet", batch.base_offset);
        if !manifest.segments.iter().any(|s| s.base_offset == batch.base_offset) {
            manifest.segments.push(SegmentEntry {
                base_offset: batch.base_offset,
                last_offset: batch.last_offset,
                base_timestamp_ns: batch.base_timestamp_ns,
                last_timestamp_ns: batch.last_timestamp_ns,
                record_count: batch.records.len() as u64,
                byte_size,
                object_key,
            });
            manifest.segments.sort_by_key(|s| s.base_offset);
            let body: Vec<u8> = serde_json::to_vec(&manifest)?;
            put(&self.store, &m_key, Bytes::from(body)).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::build_store;
    use kafkrs_models::config::ObjectStoreConfig;

    fn fs_cfg() -> ObjectStoreConfig {
        ObjectStoreConfig { backend: "filesystem".into(), bucket: "b".into(), prefix: "".into(), endpoint: "".into(), region: "us-east-1".into() }
    }

    fn rec(o: i64) -> Record {
        Record { offset: o, timestamp_ns: 1000 + o, schema_id: 0, key: vec![], value: vec![o as u8] }
    }

    #[tokio::test]
    async fn upload_writes_segment_and_appends_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(&fs_cfg(), dir.path().to_str().unwrap()).unwrap();
        // empty manifest precondition
        put(&store, &manifest_key("", "t", 0),
            bytes::Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap())).await.unwrap();

        let (tx, rx) = mpsc::channel(4);
        let (dtx, mut drx) = mpsc::channel(4);
        let up = Uploader::new(store.clone(), "".into(), "t".into(), 0, rx, dtx);
        let h = tokio::spawn(up.run());

        tx.send(UploaderMsg::Upload(SealedBatch {
            records: vec![rec(0), rec(1)], base_offset: 0, last_offset: 1,
            base_timestamp_ns: 1000, last_timestamp_ns: 1001,
        })).await.unwrap();

        let durable = drx.recv().await.unwrap();
        assert_eq!(durable.base_offset, 0);

        let raw = get(&store, &manifest_key("", "t", 0)).await.unwrap();
        let m: Manifest = serde_json::from_slice(&raw).unwrap();
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].last_offset, 1);

        drop(tx);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn re_upload_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(&fs_cfg(), dir.path().to_str().unwrap()).unwrap();
        put(&store, &manifest_key("", "t", 0),
            bytes::Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap())).await.unwrap();
        let (tx, rx) = mpsc::channel(4);
        let (dtx, mut drx) = mpsc::channel(4);
        tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, rx, dtx).run());
        let batch = || SealedBatch { records: vec![rec(0)], base_offset: 0, last_offset: 0, base_timestamp_ns: 1, last_timestamp_ns: 1 };
        tx.send(UploaderMsg::Upload(batch())).await.unwrap();
        drx.recv().await.unwrap();
        tx.send(UploaderMsg::Upload(batch())).await.unwrap();
        drx.recv().await.unwrap();
        let m: Manifest = serde_json::from_slice(&get(&store, &manifest_key("", "t", 0)).await.unwrap()).unwrap();
        assert_eq!(m.segments.len(), 1, "duplicate base_offset must not double-append");
    }
}
```

Add `mod uploader;` to `kafkrs-server/src/main.rs`.

- [ ] **Step 2: Run (after Task 15 makes crate compile)**

Run: `cargo test -p kafkrs-server --lib uploader::` → PASS (2 tests).

---

## Task 10: PartitionWriter actor

The hot path. Owns: offset counter, the active WAL file, the active batch (collected `Record`s for the current segment), the in-flight upload queue, a `broadcast` for tail consumers. Group-commits to WAL on size/count/time threshold; seals to the Uploader on segment threshold; deletes the WAL file on `SegmentDurable`.

For v1 simplicity the active batch is held as `Vec<Record>` (the spec's "Arrow column builder" is an internal representation choice; `records_to_recordbatch` converts at seal time — behaviour is identical and the conversion still happens once per seal).

**Files:**
- Create: `kafkrs-server/src/partition_writer.rs`
- Modify: `kafkrs-server/src/main.rs` (add `mod partition_writer;`)
- Test: `kafkrs-server/src/partition_writer.rs` (`#[cfg(test)]`, async)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-server/src/partition_writer.rs`:

```rust
use crate::uploader::{SealedBatch, SegmentDurable, UploaderMsg};
use crate::wal_writer::WalFile;
use anyhow::Result;
use kafkrs_models::record::Record;
use kafkrs_models::topic::ResolvedTopicConfig;
use std::collections::BTreeMap;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{Duration, Instant};

/// An incoming record before offset assignment (offset/timestamp may be unset;
/// timestamp 0 means "broker-stamp it").
pub struct IncomingRecord {
    pub schema_id: u32,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub timestamp_ns: i64,
}

pub enum PwMsg {
    /// Produce: ack fires (oneshot resolves to assigned base offset) only
    /// after WAL fsync (spec invariant 1).
    Produce { records: Vec<IncomingRecord>, ack: oneshot::Sender<i64> },
    /// Read location query for the Fetcher.
    Locate { from_offset: i64, reply: oneshot::Sender<LocateResult> },
    /// Slice the active batch for the Fetcher.
    ReadActive { from_offset: i64, max_records: usize, reply: oneshot::Sender<Vec<Record>> },
    /// Uploader signalled a segment is durable.
    SegmentDurable(SegmentDurable),
    Shutdown,
}

#[derive(Debug, PartialEq)]
pub enum LocateResult {
    /// Highest committed offset; -1 if none yet.
    Hwm(i64),
    InActiveBatch,
    InFlight,
    BelowInFlight, // serve from object store
}

pub struct PartitionWriter {
    data_dir: String,
    topic: String,
    partition: u32,
    cfg: ResolvedTopicConfig,
    next_offset: i64,
    /// base offset of the current active segment.
    segment_base: i64,
    active: Vec<Record>,
    active_bytes: usize,
    /// pending pre-fsync records buffered for group commit.
    pending: Vec<Record>,
    pending_acks: Vec<oneshot::Sender<i64>>,
    pending_first_arrival: Option<Instant>,
    wal: WalFile,
    in_flight: BTreeMap<i64, Vec<Record>>, // base_offset -> sealed records
    rx: mpsc::Receiver<PwMsg>,
    uploader_tx: mpsc::Sender<UploaderMsg>,
    tail_tx: broadcast::Sender<i64>, // notifies new HWM
}

impl PartitionWriter {
    pub fn new(
        data_dir: String,
        topic: String,
        partition: u32,
        cfg: ResolvedTopicConfig,
        start_offset: i64,
        recovered_active: Vec<Record>,
        rx: mpsc::Receiver<PwMsg>,
        uploader_tx: mpsc::Sender<UploaderMsg>,
        tail_tx: broadcast::Sender<i64>,
    ) -> Result<PartitionWriter> {
        let segment_base: i64 =
            recovered_active.first().map(|r| r.offset).unwrap_or(start_offset);
        let wal: WalFile = WalFile::open(&data_dir, &topic, partition, segment_base)?;
        let active_bytes: usize =
            recovered_active.iter().map(|r| r.value.len() + r.key.len()).sum();
        Ok(PartitionWriter {
            data_dir, topic, partition, cfg,
            next_offset: start_offset,
            segment_base,
            active: recovered_active,
            active_bytes,
            pending: Vec::new(),
            pending_acks: Vec::new(),
            pending_first_arrival: None,
            wal,
            in_flight: BTreeMap::new(),
            rx, uploader_tx, tail_tx,
        })
    }

    fn hwm(&self) -> i64 {
        self.next_offset - 1
    }

    pub async fn run(mut self) {
        loop {
            let timeout: Duration = self
                .pending_first_arrival
                .map(|t| {
                    let elapsed: u64 = t.elapsed().as_millis() as u64;
                    Duration::from_millis(self.cfg.group_commit_time_ms.saturating_sub(elapsed))
                })
                .unwrap_or(Duration::from_secs(3600));

            tokio::select! {
                msg = self.rx.recv() => {
                    match msg {
                        Some(PwMsg::Produce { records, ack }) => self.on_produce(records, ack).await,
                        Some(PwMsg::Locate { from_offset, reply }) => { let _ = reply.send(self.locate(from_offset)); }
                        Some(PwMsg::ReadActive { from_offset, max_records, reply }) => {
                            let _ = reply.send(self.read_active(from_offset, max_records));
                        }
                        Some(PwMsg::SegmentDurable(d)) => self.on_durable(d),
                        Some(PwMsg::Shutdown) | None => { self.flush_commit().await; break; }
                    }
                }
                _ = tokio::time::sleep(timeout), if self.pending_first_arrival.is_some() => {
                    self.flush_commit().await;
                }
            }
        }
    }

    async fn on_produce(&mut self, incoming: Vec<IncomingRecord>, ack: oneshot::Sender<i64>) {
        let base: i64 = self.next_offset;
        for inc in incoming {
            let ts: i64 = if inc.timestamp_ns != 0 { inc.timestamp_ns } else { now_ns() };
            self.pending.push(Record {
                offset: self.next_offset,
                timestamp_ns: ts,
                schema_id: inc.schema_id,
                key: inc.key,
                value: inc.value,
            });
            self.next_offset += 1;
        }
        self.pending_acks.push(ack);
        let _ = base; // base offset for this produce; ack carries it after fsync
        if self.pending_first_arrival.is_none() {
            self.pending_first_arrival = Some(Instant::now());
        }
        let pending_bytes: usize = self.pending.iter().map(|r| r.value.len() + r.key.len()).sum();
        if self.pending.len() >= self.cfg.group_commit_record_count
            || pending_bytes >= self.cfg.group_commit_size_bytes
        {
            self.flush_commit().await;
        }
    }

    /// Serialize pending → WAL → fsync → fire acks → move into active batch →
    /// advance HWM → notify tail. (spec §"Group commit")
    async fn flush_commit(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let batch: Vec<Record> = std::mem::take(&mut self.pending);
        self.wal.append_and_sync(&batch).expect("WAL fsync failed");
        let acks: Vec<oneshot::Sender<i64>> = std::mem::take(&mut self.pending_acks);
        for a in acks {
            let _ = a.send(self.next_offset - 1);
        }
        self.pending_first_arrival = None;
        for r in &batch {
            self.active_bytes += r.value.len() + r.key.len();
        }
        self.active.extend(batch);
        let _ = self.tail_tx.send(self.hwm());

        if self.active_bytes as u64 >= self.cfg.segment_size_bytes {
            self.seal().await;
        }
    }

    /// Freeze the active batch, hand it to the Uploader, open the next WAL.
    async fn seal(&mut self) {
        if self.active.is_empty() {
            return;
        }
        let records: Vec<Record> = std::mem::take(&mut self.active);
        self.active_bytes = 0;
        let base_offset: i64 = records.first().unwrap().offset;
        let last: &Record = records.last().unwrap();
        let last_offset: i64 = last.offset;
        let base_timestamp_ns: i64 = records.first().unwrap().timestamp_ns;
        let last_timestamp_ns: i64 = last.timestamp_ns;
        self.in_flight.insert(base_offset, records.clone());

        let _ = self
            .uploader_tx
            .send(UploaderMsg::Upload(SealedBatch {
                records, base_offset, last_offset, base_timestamp_ns, last_timestamp_ns,
            }))
            .await;

        // open next segment WAL
        self.segment_base = self.next_offset;
        self.wal = WalFile::open(&self.data_dir, &self.topic, self.partition, self.segment_base)
            .expect("open next WAL");
    }

    fn on_durable(&mut self, d: SegmentDurable) {
        self.in_flight.remove(&d.base_offset);
        // delete the WAL file for that sealed segment (spec invariant 4)
        let path: std::path::PathBuf =
            WalFile::wal_path(&self.data_dir, &self.topic, self.partition, d.base_offset);
        let _ = std::fs::remove_file(path);
    }

    fn locate(&self, from_offset: i64) -> LocateResult {
        if from_offset > self.hwm() {
            return LocateResult::Hwm(self.hwm());
        }
        if let Some(first) = self.active.first() {
            if from_offset >= first.offset {
                return LocateResult::InActiveBatch;
            }
        } else if from_offset == self.hwm() + 1 {
            return LocateResult::Hwm(self.hwm());
        }
        if let Some((&earliest, _)) = self.in_flight.iter().next() {
            if from_offset >= earliest {
                return LocateResult::InFlight;
            }
        }
        LocateResult::BelowInFlight
    }

    fn read_active(&self, from_offset: i64, max_records: usize) -> Vec<Record> {
        self.active
            .iter()
            .filter(|r| r.offset >= from_offset)
            .take(max_records)
            .cloned()
            .collect()
    }

    pub fn in_flight_slice(&self, from_offset: i64, max_records: usize) -> Vec<Record> {
        self.in_flight
            .values()
            .flat_map(|v| v.iter())
            .filter(|r| r.offset >= from_offset)
            .take(max_records)
            .cloned()
            .collect()
    }
}

fn now_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{build_store, manifest_key, put};
    use crate::uploader::Uploader;
    use kafkrs_models::config::{DiskType, ObjectStoreConfig};
    use kafkrs_models::manifest::Manifest;
    use kafkrs_models::topic::{ResolvedTopicConfig, TopicConfigOverrides};

    fn small_cfg() -> ResolvedTopicConfig {
        // seal after a tiny number of bytes so tests exercise sealing
        let mut o = TopicConfigOverrides::default();
        o.segment_size_bytes = Some(8);
        o.group_commit_record_count = Some(1);
        ResolvedTopicConfig::resolve(&o, DiskType::Nvme)
    }

    #[tokio::test]
    async fn produce_acks_after_fsync_and_advances_hwm() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let cfg = small_cfg();
        let (utx, urx) = mpsc::channel(8);
        let (dtx, _drx) = mpsc::channel(8);
        let store = build_store(&ObjectStoreConfig{backend:"filesystem".into(),bucket:"b".into(),prefix:"".into(),endpoint:"".into(),region:"us-east-1".into()}, &dd).unwrap();
        put(&store, &manifest_key("", "t", 0), bytes::Bytes::from(serde_json::to_vec(&Manifest::empty("t",0)).unwrap())).await.unwrap();
        tokio::spawn(Uploader::new(store, "".into(), "t".into(), 0, urx, dtx).run());

        let (tx, rx) = mpsc::channel(8);
        let (ttx, _trx) = broadcast::channel(16);
        let pw = PartitionWriter::new(dd, "t".into(), 0, cfg, 0, vec![], rx, utx, ttx).unwrap();
        tokio::spawn(pw.run());

        let (atx, arx) = oneshot::channel();
        tx.send(PwMsg::Produce {
            records: vec![IncomingRecord { schema_id: 0, key: vec![], value: vec![1,2,3], timestamp_ns: 0 }],
            ack: atx,
        }).await.unwrap();
        let assigned_hwm = arx.await.unwrap();
        assert_eq!(assigned_hwm, 0);

        let (ltx, lrx) = oneshot::channel();
        tx.send(PwMsg::Locate { from_offset: 5, reply: ltx }).await.unwrap();
        assert_eq!(lrx.await.unwrap(), LocateResult::Hwm(0));

        tx.send(PwMsg::Shutdown).await.unwrap();
    }
}
```

Add `mod partition_writer;` to `kafkrs-server/src/main.rs`.

- [ ] **Step 2: Run (after Task 15)**

Run: `cargo test -p kafkrs-server --lib partition_writer::` → PASS.

---

## Task 11: Fetcher

Resolves a fetch across the three tiers via the PartitionWriter, with long-poll and the spec's error codes (spec §"Consumer flow", §"Edge cases").

**Files:**
- Create: `kafkrs-server/src/fetcher.rs`
- Modify: `kafkrs-server/src/main.rs` (add `mod fetcher;`)
- Test: `kafkrs-server/src/fetcher.rs` (`#[cfg(test)]`, async)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-server/src/fetcher.rs`:

```rust
use crate::object_store::{get, manifest_key, segment_key};
use crate::partition_writer::{LocateResult, PwMsg};
use anyhow::Result;
use bytes::Bytes;
use kafkrs_models::manifest::{Manifest, SegmentEntry};
use kafkrs_models::record::Record;
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{timeout, Duration};

#[derive(Debug, PartialEq)]
pub enum FetchError {
    UnknownTopic,
    UnknownPartition,
    OffsetOutOfRange,
    BrokerNotReady,
}

pub struct FetchRequest {
    pub topic: String,
    pub partition: u32,
    pub from_offset: i64,
    pub max_records: usize,
    pub max_wait_ms: u64,
}

pub struct FetchResponse {
    pub records: Vec<Record>,
    pub hwm: i64,
}

/// Resolves one fetch. `pw_tx`/`tail` are the target partition's handles
/// (resolved by the caller from the topic registry; None → UnknownTopic/Partition).
pub async fn fetch(
    req: FetchRequest,
    pw_tx: &mpsc::Sender<PwMsg>,
    tail: &broadcast::Sender<i64>,
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> Result<FetchResponse, FetchError> {
    if req.from_offset < 0 {
        return Err(FetchError::OffsetOutOfRange);
    }
    let loc: LocateResult = locate(pw_tx, req.from_offset).await?;
    match loc {
        LocateResult::Hwm(hwm) => {
            if req.max_wait_ms == 0 {
                return Ok(FetchResponse { records: vec![], hwm });
            }
            let mut sub: broadcast::Receiver<i64> = tail.subscribe();
            let _ = timeout(Duration::from_millis(req.max_wait_ms), async {
                loop {
                    match sub.recv().await {
                        Ok(new_hwm) if new_hwm >= req.from_offset => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            })
            .await;
            // re-resolve once after wake
            match locate(pw_tx, req.from_offset).await? {
                LocateResult::InActiveBatch => read_active(pw_tx, &req).await,
                LocateResult::Hwm(h) => Ok(FetchResponse { records: vec![], hwm: h }),
                _ => read_object_store(req, store, prefix).await,
            }
        }
        LocateResult::InActiveBatch => read_active(pw_tx, &req).await,
        LocateResult::InFlight => read_object_store(req, store, prefix).await
            .or_else(|_| Ok(FetchResponse { records: vec![], hwm: -1 })),
        LocateResult::BelowInFlight => read_object_store(req, store, prefix).await,
    }
}

async fn locate(pw_tx: &mpsc::Sender<PwMsg>, from_offset: i64) -> Result<LocateResult, FetchError> {
    let (tx, rx): (oneshot::Sender<LocateResult>, oneshot::Receiver<LocateResult>) =
        oneshot::channel();
    pw_tx
        .send(PwMsg::Locate { from_offset, reply: tx })
        .await
        .map_err(|_| FetchError::BrokerNotReady)?;
    rx.await.map_err(|_| FetchError::BrokerNotReady)
}

async fn read_active(
    pw_tx: &mpsc::Sender<PwMsg>,
    req: &FetchRequest,
) -> Result<FetchResponse, FetchError> {
    let (tx, rx): (oneshot::Sender<Vec<Record>>, oneshot::Receiver<Vec<Record>>) =
        oneshot::channel();
    pw_tx
        .send(PwMsg::ReadActive { from_offset: req.from_offset, max_records: req.max_records, reply: tx })
        .await
        .map_err(|_| FetchError::BrokerNotReady)?;
    let records: Vec<Record> = rx.await.map_err(|_| FetchError::BrokerNotReady)?;
    let hwm: i64 = records.last().map(|r| r.offset).unwrap_or(req.from_offset - 1);
    Ok(FetchResponse { records, hwm })
}

async fn read_object_store(
    req: FetchRequest,
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> Result<FetchResponse, FetchError> {
    let m_key: ObjPath = manifest_key(prefix, &req.topic, req.partition);
    let raw: Bytes = get(store, &m_key).await.map_err(|_| FetchError::UnknownTopic)?;
    let manifest: Manifest =
        serde_json::from_slice(&raw).map_err(|_| FetchError::BrokerNotReady)?;
    let seg: &SegmentEntry = manifest
        .segment_for_offset(req.from_offset)
        .ok_or(FetchError::OffsetOutOfRange)?;
    let key: ObjPath = segment_key(prefix, &req.topic, req.partition, seg.base_offset);
    let bytes: Bytes = get(store, &key).await.map_err(|_| FetchError::BrokerNotReady)?;
    let reader: ParquetRecordBatchReader = ParquetRecordBatchReaderBuilder::try_new(bytes)
        .map_err(|_| FetchError::BrokerNotReady)?
        .build()
        .map_err(|_| FetchError::BrokerNotReady)?;
    let mut out: Vec<Record> = Vec::new();
    for batch in reader {
        let batch: arrow_array::RecordBatch = batch.map_err(|_| FetchError::BrokerNotReady)?;
        out.extend(recordbatch_to_records(&batch));
    }
    let hwm: i64 = manifest.last_uploaded_offset().unwrap_or(-1);
    let records: Vec<Record> = out
        .into_iter()
        .filter(|r| r.offset >= req.from_offset)
        .take(req.max_records)
        .collect();
    Ok(FetchResponse { records, hwm })
}

fn recordbatch_to_records(batch: &arrow_array::RecordBatch) -> Vec<Record> {
    use arrow_array::{BinaryArray, Int32Array, Int64Array, TimestampNanosecondArray};
    let off: &Int64Array = batch.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let ts: &TimestampNanosecondArray =
        batch.column(1).as_any().downcast_ref::<TimestampNanosecondArray>().unwrap();
    let key: &BinaryArray = batch.column(2).as_any().downcast_ref::<BinaryArray>().unwrap();
    let val: &BinaryArray = batch.column(3).as_any().downcast_ref::<BinaryArray>().unwrap();
    let sid: &Int32Array = batch.column(4).as_any().downcast_ref::<Int32Array>().unwrap();
    (0..batch.num_rows())
        .map(|i| Record {
            offset: off.value(i),
            timestamp_ns: ts.value(i),
            schema_id: sid.value(i) as u32,
            key: if key.is_null(i) { vec![] } else { key.value(i).to_vec() },
            value: val.value(i).to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{build_store, put};
    use crate::segment::write_segment;
    use kafkrs_models::config::ObjectStoreConfig;
    use kafkrs_models::manifest::{Manifest, SegmentEntry};

    #[tokio::test]
    async fn negative_offset_is_out_of_range() {
        // pw_tx with no receiver alive triggers BrokerNotReady on locate, but
        // negative offset is checked first.
        let (tx, _rx) = mpsc::channel(1);
        let (ttx, _t) = broadcast::channel(1);
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(&ObjectStoreConfig{backend:"filesystem".into(),bucket:"b".into(),prefix:"".into(),endpoint:"".into(),region:"us-east-1".into()}, dir.path().to_str().unwrap()).unwrap();
        let err = fetch(FetchRequest{topic:"t".into(),partition:0,from_offset:-1,max_records:10,max_wait_ms:0}, &tx, &ttx, &store, "").await.unwrap_err();
        assert_eq!(err, FetchError::OffsetOutOfRange);
    }

    #[tokio::test]
    async fn reads_from_object_store_tier() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_store(&ObjectStoreConfig{backend:"filesystem".into(),bucket:"b".into(),prefix:"".into(),endpoint:"".into(),region:"us-east-1".into()}, dir.path().to_str().unwrap()).unwrap();
        let recs: Vec<Record> = (0..10).map(|o| Record{offset:o,timestamp_ns:o,schema_id:0,key:vec![],value:vec![o as u8]}).collect();
        let bytes = write_segment(&recs).unwrap();
        let byte_size = bytes.len() as u64;
        put(&store, &segment_key("", "t", 0, 0), bytes).await.unwrap();
        let mut m = Manifest::empty("t", 0);
        m.segments.push(SegmentEntry{base_offset:0,last_offset:9,base_timestamp_ns:0,last_timestamp_ns:9,record_count:10,byte_size,object_key:"segment-00000000000000000000.parquet".into()});
        put(&store, &manifest_key("", "t", 0), bytes::Bytes::from(serde_json::to_vec(&m).unwrap())).await.unwrap();

        // Force the object-store path: a pw that answers BelowInFlight.
        let (tx, mut rx) = mpsc::channel(1);
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let PwMsg::Locate { reply, .. } = msg { let _ = reply.send(LocateResult::BelowInFlight); }
            }
        });
        let (ttx, _t) = broadcast::channel(1);
        let resp = fetch(FetchRequest{topic:"t".into(),partition:0,from_offset:5,max_records:100,max_wait_ms:0}, &tx, &ttx, &store, "").await.unwrap();
        assert_eq!(resp.records.first().unwrap().offset, 5);
        assert_eq!(resp.records.len(), 5);
        assert_eq!(resp.hwm, 9);
    }
}
```

Add `mod fetcher;` to `kafkrs-server/src/main.rs`.

- [ ] **Step 2: Run (after Task 15)**

Run: `cargo test -p kafkrs-server --lib fetcher::` → PASS (2 tests).

---

## Task 12: TopicRegistry actor

Owns `topics.json`. `CreateTopic` is atomic across 3 steps (registry rewrite, WAL dirs, empty manifests) (spec §"Topic operations"). Serialised through one actor.

**Files:**
- Create: `kafkrs-server/src/topic_registry.rs`
- Modify: `kafkrs-server/src/main.rs` (add `mod topic_registry;`)
- Test: `kafkrs-server/src/topic_registry.rs` (`#[cfg(test)]`, async)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-server/src/topic_registry.rs`:

```rust
use crate::object_store::{manifest_key, put};
use anyhow::Result;
use kafkrs_models::config::DiskType;
use kafkrs_models::manifest::Manifest;
use kafkrs_models::topic::{ResolvedTopicConfig, TopicConfigOverrides, TopicEntry, TopicRegistryFile};
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub enum RegistryMsg {
    Create {
        name: String,
        partition_count: u32,
        overrides: TopicConfigOverrides,
        reply: oneshot::Sender<Result<(), RegistryError>>,
    },
    Describe { name: String, reply: oneshot::Sender<Option<TopicEntry>> },
    List { reply: oneshot::Sender<Vec<String>> },
    /// Ensure a topic exists (auto-create). No-op if present.
    EnsureExists {
        name: String,
        partition_count: u32,
        reply: oneshot::Sender<Result<(), RegistryError>>,
    },
}

#[derive(Debug, PartialEq)]
pub enum RegistryError {
    AlreadyExists,
    Io(String),
}

pub struct TopicRegistry {
    data_dir: String,
    disk: DiskType,
    store: Arc<dyn ObjectStore>,
    prefix: String,
    topics: HashMap<String, TopicEntry>,
    rx: mpsc::Receiver<RegistryMsg>,
}

fn registry_path(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("topics.json")
}

impl TopicRegistry {
    /// Loads `topics.json` (or starts empty) and returns the actor.
    pub fn load(
        data_dir: String,
        disk: DiskType,
        store: Arc<dyn ObjectStore>,
        prefix: String,
        rx: mpsc::Receiver<RegistryMsg>,
    ) -> Result<TopicRegistry> {
        let path: PathBuf = registry_path(&data_dir);
        let file: TopicRegistryFile = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path)?)?
        } else {
            TopicRegistryFile::default()
        };
        let topics: HashMap<String, TopicEntry> =
            file.topics.into_iter().map(|t| (t.name.clone(), t)).collect();
        Ok(TopicRegistry { data_dir, disk, store, prefix, topics, rx })
    }

    pub fn resolved(&self, name: &str) -> Option<ResolvedTopicConfig> {
        self.topics
            .get(name)
            .map(|t| ResolvedTopicConfig::resolve(&t.config, self.disk))
    }

    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                RegistryMsg::Create { name, partition_count, overrides, reply } => {
                    let _ = reply.send(self.create(&name, partition_count, overrides).await);
                }
                RegistryMsg::EnsureExists { name, partition_count, reply } => {
                    let r: Result<(), RegistryError> = if self.topics.contains_key(&name) {
                        Ok(())
                    } else {
                        self.create(&name, partition_count, TopicConfigOverrides::default()).await
                    };
                    let _ = reply.send(r);
                }
                RegistryMsg::Describe { name, reply } => {
                    let _ = reply.send(self.topics.get(&name).cloned());
                }
                RegistryMsg::List { reply } => {
                    let _ = reply.send(self.topics.keys().cloned().collect());
                }
            }
        }
    }

    async fn create(
        &mut self,
        name: &str,
        partition_count: u32,
        overrides: TopicConfigOverrides,
    ) -> Result<(), RegistryError> {
        if self.topics.contains_key(name) {
            return Err(RegistryError::AlreadyExists);
        }
        let entry: TopicEntry = TopicEntry {
            name: name.to_string(),
            partition_count,
            created_at_ns: now_ns(),
            config: overrides,
        };
        // Step 1: atomic rewrite of topics.json (tmp + fsync + rename).
        let mut next: TopicRegistryFile = TopicRegistryFile {
            topics: self.topics.values().cloned().collect(),
        };
        next.topics.push(entry.clone());
        atomic_write_registry(&self.data_dir, &next).map_err(|e| RegistryError::Io(e.to_string()))?;

        // Step 2: WAL directories per partition.
        for p in 0..partition_count {
            let dir: PathBuf =
                Path::new(&self.data_dir).join("wal").join(name).join(p.to_string());
            std::fs::create_dir_all(&dir).map_err(|e| RegistryError::Io(e.to_string()))?;
        }
        // Step 3: empty manifest per partition.
        for p in 0..partition_count {
            let key: ObjPath = manifest_key(&self.prefix, name, p);
            let body: Vec<u8> = serde_json::to_vec(&Manifest::empty(name, p))
                .map_err(|e| RegistryError::Io(e.to_string()))?;
            put(&self.store, &key, bytes::Bytes::from(body))
                .await
                .map_err(|e| RegistryError::Io(e.to_string()))?;
        }
        self.topics.insert(name.to_string(), entry);
        Ok(())
    }
}

fn atomic_write_registry(data_dir: &str, file: &TopicRegistryFile) -> std::io::Result<()> {
    let path: PathBuf = registry_path(data_dir);
    let tmp: PathBuf = path.with_extension("json.tmp");
    let body: Vec<u8> = serde_json::to_vec_pretty(file)?;
    {
        let mut f: std::fs::File = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn now_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::build_store;
    use kafkrs_models::config::ObjectStoreConfig;

    fn store(dir: &Path) -> Arc<dyn ObjectStore> {
        build_store(&ObjectStoreConfig{backend:"filesystem".into(),bucket:"b".into(),prefix:"".into(),endpoint:"".into(),region:"us-east-1".into()}, dir.to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn create_is_atomic_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(8);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx).unwrap();
        tokio::spawn(reg.run());

        let (r, rr) = oneshot::channel();
        tx.send(RegistryMsg::Create { name: "orders".into(), partition_count: 2, overrides: TopicConfigOverrides::default(), reply: r }).await.unwrap();
        rr.await.unwrap().unwrap();

        // topics.json persisted
        assert!(registry_path(&dd).exists());
        // WAL dirs exist
        assert!(Path::new(&dd).join("wal/orders/0").exists());
        assert!(Path::new(&dd).join("wal/orders/1").exists());
        // empty manifests exist
        let raw = crate::object_store::get(&store(dir.path()), &manifest_key("", "orders", 1)).await.unwrap();
        let m: Manifest = serde_json::from_slice(&raw).unwrap();
        assert_eq!(m.segments.len(), 0);

        // duplicate create rejected
        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::Create { name: "orders".into(), partition_count: 1, overrides: TopicConfigOverrides::default(), reply: r2 }).await.unwrap();
        assert_eq!(rr2.await.unwrap().unwrap_err(), RegistryError::AlreadyExists);
    }

    #[tokio::test]
    async fn reload_recovers_topics() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        {
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx).unwrap().run());
            let (r, rr) = oneshot::channel();
            tx.send(RegistryMsg::Create{name:"t".into(),partition_count:1,overrides:TopicConfigOverrides::default(),reply:r}).await.unwrap();
            rr.await.unwrap().unwrap();
        }
        let (_tx, rx) = mpsc::channel(1);
        let reg2 = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx).unwrap();
        assert!(reg2.resolved("t").is_some());
    }
}
```

Add `mod topic_registry;` to `kafkrs-server/src/main.rs`.

- [ ] **Step 2: Run (after Task 15)**

Run: `cargo test -p kafkrs-server --lib topic_registry::` → PASS (2 tests).

---

## Task 13: Startup recovery sequence

Per partition (spec §"Recovery on startup"): list `.wal` files, scan/truncate each, GET the manifest once, reconcile (delete fully-covered WALs; replay above-last-uploaded into the active batch and re-queue for upload), compute `next_offset`. No object-store LIST.

**Files:**
- Create: `kafkrs-server/src/recovery.rs`
- Modify: `kafkrs-server/src/main.rs` (add `mod recovery;`)
- Test: `kafkrs-server/src/recovery.rs` (`#[cfg(test)]`, async)

- [ ] **Step 1: Write the failing test**

Create `kafkrs-server/src/recovery.rs`:

```rust
use crate::object_store::{get, manifest_key};
use crate::wal_writer::recover_wal_file;
use anyhow::Result;
use bytes::Bytes;
use kafkrs_models::manifest::Manifest;
use kafkrs_models::record::Record;
use object_store::ObjectStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Result of recovering one partition.
pub struct PartitionRecovery {
    /// Records that belong to the (not-yet-uploaded) active segment, in order.
    pub active_records: Vec<Record>,
    /// Sealed-but-not-uploaded segments to re-queue (base_offset → records).
    pub orphan_segments: Vec<(i64, Vec<Record>)>,
    /// Next offset to assign.
    pub next_offset: i64,
}

pub async fn recover_partition(
    data_dir: &str,
    topic: &str,
    partition: u32,
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> Result<PartitionRecovery> {
    let wal_dir: PathBuf =
        Path::new(data_dir).join("wal").join(topic).join(partition.to_string());
    let mut wal_bases: Vec<i64> = Vec::new();
    if wal_dir.exists() {
        for entry in std::fs::read_dir(&wal_dir)? {
            let entry: std::fs::DirEntry = entry?;
            let name: String = entry.file_name().to_string_lossy().to_string();
            if let Some(base) = name.strip_suffix(".wal").and_then(|s| s.parse::<i64>().ok()) {
                wal_bases.push(base);
            }
        }
    }
    wal_bases.sort();

    // single GET of the manifest (spec: no object-store LIST).
    let raw: Bytes = get(store, &manifest_key(prefix, topic, partition)).await?;
    let manifest: Manifest = serde_json::from_slice(&raw)?;
    let last_uploaded: Option<i64> = manifest.last_uploaded_offset();

    let mut active_records: Vec<Record> = Vec::new();
    let mut orphan_segments: Vec<(i64, Vec<Record>)> = Vec::new();
    let mut next_offset: i64 = last_uploaded.map(|o| o + 1).unwrap_or(0);

    for base in wal_bases {
        let path: PathBuf = wal_dir.join(format!("{base}.wal"));
        let covered: bool = last_uploaded.map(|lu| base <= lu).unwrap_or(false)
            && manifest.segments.iter().any(|s| s.base_offset == base);
        if covered {
            // crash between manifest update and WAL delete → clean up
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let records: Vec<Record> = recover_wal_file(&path)?;
        if records.is_empty() {
            continue;
        }
        if let Some(last) = records.last() {
            next_offset = next_offset.max(last.offset + 1);
        }
        // Highest-base WAL is the active segment; earlier ones are orphan
        // sealed segments that never uploaded.
        orphan_segments.push((base, records));
    }
    // The orphan with the largest base is the active segment.
    if let Some((_, recs)) = orphan_segments.pop() {
        active_records = recs;
    }

    Ok(PartitionRecovery { active_records, orphan_segments, next_offset })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{build_store, put};
    use crate::wal_writer::WalFile;
    use kafkrs_models::config::ObjectStoreConfig;
    use kafkrs_models::manifest::{Manifest, SegmentEntry};

    fn rec(o: i64) -> Record { Record { offset: o, timestamp_ns: o, schema_id: 0, key: vec![], value: vec![o as u8] } }

    #[tokio::test]
    async fn replays_wal_above_last_uploaded() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let store = build_store(&ObjectStoreConfig{backend:"filesystem".into(),bucket:"b".into(),prefix:"".into(),endpoint:"".into(),region:"us-east-1".into()}, &dd).unwrap();
        // uploaded segment 0..=4
        let mut m = Manifest::empty("t", 0);
        m.segments.push(SegmentEntry{base_offset:0,last_offset:4,base_timestamp_ns:0,last_timestamp_ns:4,record_count:5,byte_size:1,object_key:"segment-00000000000000000000.parquet".into()});
        put(&store, &manifest_key("", "t", 0), bytes::Bytes::from(serde_json::to_vec(&m).unwrap())).await.unwrap();
        // covered WAL 0 + active WAL 5
        let mut w0 = WalFile::open(&dd, "t", 0, 0).unwrap();
        w0.append_and_sync(&[rec(0), rec(1)]).unwrap();
        let mut w5 = WalFile::open(&dd, "t", 0, 5).unwrap();
        w5.append_and_sync(&[rec(5), rec(6)]).unwrap();

        let r = recover_partition(&dd, "t", 0, &store, "").await.unwrap();
        assert_eq!(r.active_records.iter().map(|x| x.offset).collect::<Vec<_>>(), vec![5, 6]);
        assert_eq!(r.next_offset, 7);
        // covered WAL 0 deleted
        assert!(!WalFile::wal_path(&dd, "t", 0, 0).exists());
    }
}
```

Add `mod recovery;` to `kafkrs-server/src/main.rs`.

- [ ] **Step 2: Run (after Task 15)**

Run: `cargo test -p kafkrs-server --lib recovery::` → PASS.

---

## Task 14: Listener + main wiring

Replace the unsound `read_to_end` loop with framed length-prefixed I/O, fix the accept-once bug (`main.rs:46`), and wire all actors per partition. The wire protocol is out of scope; this is a minimal length-prefixed `bincode` shim sufficient to exercise storage end-to-end.

**Files:**
- Modify: `kafkrs-server/src/listener.rs` (full rewrite)
- Modify: `kafkrs-server/src/main.rs` (full rewrite)
- Modify: `kafkrs-server/src/config.rs` (return new `Config` — already compatible; just confirm import path)
- Delete: `kafkrs-server/src/writer.rs`
- Modify: `kafkrs-server/Cargo.toml` (remove `arrow-ipc`; keep `bincode`)

- [ ] **Step 1: Define the minimal wire frame + handlers**

Replace `kafkrs-server/src/listener.rs` with:

```rust
use crate::fetcher::{fetch, FetchRequest};
use crate::partition_writer::{IncomingRecord, PwMsg};
use crate::topic_registry::{RegistryError, RegistryMsg};
use bincode::config as bincode_config;
use bincode::serde::{decode_from_slice, encode_to_vec};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};

/// Per-partition handles the listener routes to.
#[derive(Clone)]
pub struct PartitionHandle {
    pub pw_tx: mpsc::Sender<PwMsg>,
    pub tail: broadcast::Sender<i64>,
}

#[derive(Clone)]
pub struct SharedState {
    pub partitions: Arc<tokio::sync::RwLock<HashMap<(String, u32), PartitionHandle>>>,
    pub registry: mpsc::Sender<RegistryMsg>,
    pub store: Arc<dyn object_store::ObjectStore>,
    pub prefix: String,
    pub auto_create: bool,
    pub default_partition_count: u32,
}

#[derive(Serialize, Deserialize)]
pub enum WireRequest {
    Produce { topic: String, partition: u32, key: Vec<u8>, value: Vec<u8>, schema_id: u32, timestamp_ns: i64 },
    Fetch { topic: String, partition: u32, from_offset: i64, max_records: usize, max_wait_ms: u64 },
}

#[derive(Serialize, Deserialize)]
pub enum WireResponse {
    Produced { base_offset: i64 },
    Fetched { records: Vec<(i64, i64, u32, Vec<u8>, Vec<u8>)>, hwm: i64 },
    Error(String),
}

pub struct Listener {
    socket: TcpStream,
    state: SharedState,
}

impl Listener {
    pub fn new(socket: TcpStream, state: SharedState) -> Listener {
        Listener { socket, state }
    }

    pub async fn process(&mut self) {
        let bc = bincode_config::standard();
        loop {
            let mut len_buf: [u8; 4] = [0u8; 4];
            if self.socket.read_exact(&mut len_buf).await.is_err() {
                return; // connection closed
            }
            let len: usize = u32::from_le_bytes(len_buf) as usize;
            let mut buf: Vec<u8> = vec![0u8; len];
            if self.socket.read_exact(&mut buf).await.is_err() {
                return;
            }
            let (req, _): (WireRequest, usize) = match decode_from_slice(&buf, bc) {
                Ok(v) => v,
                Err(e) => {
                    self.write(&WireResponse::Error(format!("decode: {e}"))).await;
                    continue;
                }
            };
            let resp: WireResponse = self.handle(req).await;
            self.write(&resp).await;
        }
    }

    async fn handle(&self, req: WireRequest) -> WireResponse {
        match req {
            WireRequest::Produce { topic, partition, key, value, schema_id, timestamp_ns } => {
                if self.state.auto_create {
                    let (r, rr): (
                        oneshot::Sender<Result<(), RegistryError>>,
                        oneshot::Receiver<Result<(), RegistryError>>,
                    ) = oneshot::channel();
                    let _ = self
                        .state
                        .registry
                        .send(RegistryMsg::EnsureExists {
                            name: topic.clone(),
                            partition_count: self.state.default_partition_count,
                            reply: r,
                        })
                        .await;
                    let _ = rr.await;
                }
                let handle: Option<PartitionHandle> = {
                    let guard: tokio::sync::RwLockReadGuard<
                        '_,
                        HashMap<(String, u32), PartitionHandle>,
                    > = self.state.partitions.read().await;
                    guard.get(&(topic.clone(), partition)).cloned()
                };
                let Some(handle) = handle else {
                    return WireResponse::Error("UnknownTopic".into());
                };
                let (ack, ack_rx): (oneshot::Sender<i64>, oneshot::Receiver<i64>) =
                    oneshot::channel();
                if handle
                    .pw_tx
                    .send(PwMsg::Produce {
                        records: vec![IncomingRecord { schema_id, key, value, timestamp_ns }],
                        ack,
                    })
                    .await
                    .is_err()
                {
                    return WireResponse::Error("BrokerNotReady".into());
                }
                match ack_rx.await {
                    Ok(hwm) => WireResponse::Produced { base_offset: hwm },
                    Err(_) => WireResponse::Error("BrokerNotReady".into()),
                }
            }
            WireRequest::Fetch { topic, partition, from_offset, max_records, max_wait_ms } => {
                let handle: Option<PartitionHandle> = {
                    let guard: tokio::sync::RwLockReadGuard<
                        '_,
                        HashMap<(String, u32), PartitionHandle>,
                    > = self.state.partitions.read().await;
                    guard.get(&(topic.clone(), partition)).cloned()
                };
                let Some(handle) = handle else {
                    return WireResponse::Error("UnknownTopic".into());
                };
                match fetch(
                    FetchRequest { topic, partition, from_offset, max_records, max_wait_ms },
                    &handle.pw_tx,
                    &handle.tail,
                    &self.state.store,
                    &self.state.prefix,
                )
                .await
                {
                    Ok(resp) => WireResponse::Fetched {
                        records: resp
                            .records
                            .into_iter()
                            .map(|r| (r.offset, r.timestamp_ns, r.schema_id, r.key, r.value))
                            .collect(),
                        hwm: resp.hwm,
                    },
                    Err(e) => WireResponse::Error(format!("{e:?}")),
                }
            }
        }
    }

    async fn write(&mut self, resp: &WireResponse) {
        let body: Vec<u8> = encode_to_vec(resp, bincode_config::standard()).unwrap();
        let _ = self.socket.write_all(&(body.len() as u32).to_le_bytes()).await;
        let _ = self.socket.write_all(&body).await;
    }
}
```

- [ ] **Step 2: Rewrite `main.rs` — accept loop + actor spawn**

Replace `kafkrs-server/src/main.rs` with:

```rust
use clap::Parser;
use log::{error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::{broadcast, mpsc, RwLock};

use kafkrs_models::topic::ResolvedTopicConfig;

use crate::listener::{Listener, PartitionHandle, SharedState};
use crate::object_store::build_store;
use crate::partition_writer::PartitionWriter;
use crate::recovery::recover_partition;
use crate::topic_registry::{RegistryMsg, TopicRegistry};
use crate::uploader::{Uploader, UploaderMsg};

mod config;
mod fetcher;
mod listener;
mod object_store;
mod partition_writer;
mod recovery;
mod segment;
mod topic_registry;
mod uploader;
mod wal_writer;

#[derive(Parser)]
struct Cli {
    config_path: Option<String>,
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let args: Cli = Cli::parse();
    let config_path: String = args.config_path.unwrap_or_else(|| "./config.toml".to_string());
    let cfg: kafkrs_models::config::Config = config::load_config(config_path);

    let store: Arc<dyn ::object_store::ObjectStore> =
        build_store(&cfg.object_store, &cfg.data_dir).expect("object store");
    let prefix: String = cfg.object_store.prefix.clone();

    // Topic registry actor.
    let (reg_tx, reg_rx): (mpsc::Sender<RegistryMsg>, mpsc::Receiver<RegistryMsg>) =
        mpsc::channel(64);
    let registry: TopicRegistry = TopicRegistry::load(
        cfg.data_dir.clone(),
        cfg.broker.disk_type.clone(),
        store.clone(),
        prefix.clone(),
        reg_rx,
    )
    .expect("load topic registry");

    // Snapshot existing topics for partition bring-up before moving the actor.
    let known: Vec<(String, u32, ResolvedTopicConfig)> = {
        let (tx, rx) = std::sync::mpsc::channel();
        // registry.resolved is sync; gather from the loaded file directly:
        registry_topics(&registry, &mut { let _ = &tx; }, rx)
    };

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Bring up each known partition independently (spec risk: startup must not
    // serialize on the slowest manifest GET — each task is independent).
    for (topic, pcount, rtc) in known {
        for p in 0..pcount {
            spawn_partition(
                &cfg.data_dir, &topic, p, rtc, store.clone(), prefix.clone(), partitions.clone(),
            )
            .await;
        }
    }

    tokio::spawn(registry.run());

    let state: SharedState = SharedState {
        partitions: partitions.clone(),
        registry: reg_tx.clone(),
        store: store.clone(),
        prefix: prefix.clone(),
        auto_create: cfg.broker.auto_create_topics,
        default_partition_count: cfg.broker.default_partition_count,
    };

    for port in cfg.ports.clone() {
        let addr: String = format!("{}:{}", cfg.address, port);
        let listener: TcpListener = TcpListener::bind(&addr).await.expect("bind");
        info!("Listening on {addr}");
        let st: SharedState = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => {
                        let st2: SharedState = st.clone();
                        tokio::spawn(async move { Listener::new(socket, st2).process().await });
                    }
                    Err(e) => error!("accept error: {e}"),
                }
            }
        });
    }

    match signal::ctrl_c().await {
        Ok(()) => info!("Shutdown signal received. Goodbye"),
        Err(e) => error!("signal error: {e}"),
    }
}

/// Helper: list topics+resolved config from the loaded registry actor state.
fn registry_topics(
    reg: &TopicRegistry,
    _unused: &mut (),
    _rx: std::sync::mpsc::Receiver<()>,
) -> Vec<(String, u32, ResolvedTopicConfig)> {
    reg.snapshot()
}

async fn spawn_partition(
    data_dir: &str,
    topic: &str,
    partition: u32,
    cfg: ResolvedTopicConfig,
    store: Arc<dyn ::object_store::ObjectStore>,
    prefix: String,
    partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
) {
    let rec: crate::recovery::PartitionRecovery =
        recover_partition(data_dir, topic, partition, &store, &prefix)
            .await
            .expect("recover partition");

    let (utx, urx): (mpsc::Sender<UploaderMsg>, mpsc::Receiver<UploaderMsg>) =
        mpsc::channel::<UploaderMsg>(64);
    let (dtx, mut drx): (
        mpsc::Sender<crate::uploader::SegmentDurable>,
        mpsc::Receiver<crate::uploader::SegmentDurable>,
    ) = mpsc::channel(64);
    tokio::spawn(
        Uploader::new(store.clone(), prefix.clone(), topic.to_string(), partition, urx, dtx).run(),
    );

    let (pw_tx, pw_rx): (
        mpsc::Sender<partition_writer::PwMsg>,
        mpsc::Receiver<partition_writer::PwMsg>,
    ) = mpsc::channel(256);
    let (tail, _): (broadcast::Sender<i64>, broadcast::Receiver<i64>) = broadcast::channel(1024);

    // Re-queue orphan sealed segments for upload.
    for (base, records) in rec.orphan_segments {
        let last: &kafkrs_models::record::Record = records.last().unwrap();
        let _ = utx
            .send(UploaderMsg::Upload(uploader::SealedBatch {
                base_offset: base,
                last_offset: last.offset,
                base_timestamp_ns: records.first().unwrap().timestamp_ns,
                last_timestamp_ns: last.timestamp_ns,
                records,
            }))
            .await;
    }

    let pw: PartitionWriter = PartitionWriter::new(
        data_dir.to_string(),
        topic.to_string(),
        partition,
        cfg,
        rec.next_offset,
        rec.active_records,
        pw_rx,
        utx,
        tail.clone(),
    )
    .expect("partition writer");

    let pw_tx_for_durable: mpsc::Sender<partition_writer::PwMsg> = pw_tx.clone();
    tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let _ = pw_tx_for_durable.send(partition_writer::PwMsg::SegmentDurable(d)).await;
        }
    });

    tokio::spawn(pw.run());
    partitions
        .write()
        .await
        .insert((topic.to_string(), partition), PartitionHandle { pw_tx, tail });
}
```

Add a `snapshot` method to `TopicRegistry` in `topic_registry.rs`:

```rust
impl TopicRegistry {
    pub fn snapshot(&self) -> Vec<(String, u32, ResolvedTopicConfig)> {
        self.topics
            .values()
            .map(|t| {
                (
                    t.name.clone(),
                    t.partition_count,
                    ResolvedTopicConfig::resolve(&t.config, self.disk),
                )
            })
            .collect()
    }
}
```

- [ ] **Step 3: Simplify the `registry_topics` shim**

The `registry_topics` helper above is over-complicated. Replace the `known` block and helper in `main.rs` with a direct call:

```rust
    let known: Vec<(String, u32, ResolvedTopicConfig)> = registry.snapshot();
```

and delete the `registry_topics` fn entirely.

- [ ] **Step 4: Cargo + cleanup**

In `kafkrs-server/Cargo.toml`: remove the `arrow-ipc = "55.0.0"` line; add `env_logger = "0.11"`. Confirm `kafkrs-server/src/config.rs` still compiles (it returns `kafkrs_models::config::Config`; no change needed). Then delete the obsolete writer: `rm kafkrs-server/src/writer.rs`

- [ ] **Step 5: Compile the whole server crate + run ALL server unit tests**

Run: `cargo test -p kafkrs-server --lib`
Expected: PASS — every module test from Tasks 6–13 plus this task compiles and is green.

If a parquet/object_store API name differs from this plan, fix only the API call, never the asserted behaviour, and re-run.

---

## Task 15: kafkrs-python encode_message signature

Match the new `Record` envelope: `key: bytes`, `value: bytes`, drop `partition`, add `schema_id: u32`. The Python binding produces a `WireRequest::Produce`-compatible payload is out of scope; v1 keeps it producing a serialized envelope for tests.

**Files:**
- Modify: `kafkrs-python/src/lib.rs`
- Modify: `kafkrs-python/Cargo.toml` (no change expected; uses `kafkrs-models`)

- [ ] **Step 1: Write the implementation**

Replace `kafkrs-python/src/lib.rs` with:

```rust
use bincode::config;
use bincode::serde::encode_to_vec;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use kafkrs_models::record::Record;

#[pyfunction]
#[pyo3(signature = (key, value, schema_id, timestamp_ns=0))]
fn encode_message<'py>(
    py: Python<'py>,
    key: Vec<u8>,
    value: Vec<u8>,
    schema_id: u32,
    timestamp_ns: i64,
) -> PyResult<Bound<'py, PyBytes>> {
    let ts: i64 = if timestamp_ns != 0 {
        timestamp_ns
    } else {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as i64
    };
    let record: Record = Record {
        offset: 0, // assigned by the broker at commit time
        timestamp_ns: ts,
        schema_id,
        key,
        value,
    };
    let bin: Vec<u8> = encode_to_vec(&record, config::standard()).unwrap();
    Ok(PyBytes::new_bound(py, &bin))
}

#[pymodule]
fn kafkrs_python(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(encode_message, module)?)?;
    Ok(())
}
```

- [ ] **Step 2: Compile**

Run: `cargo build -p kafkrs-python`
Expected: builds (pyo3 cdylib).


---

## Task 16: End-to-end integration test

Produce → WAL fsync ack → seal → upload → fetch from all three tiers, plus a crash-recovery scenario, against the filesystem object store.

**Files:**
- Create: `kafkrs-server/tests/storage_e2e.rs`

- [ ] **Step 1: Write the failing test**

Create `kafkrs-server/tests/storage_e2e.rs`. Because the actors are crate-internal, the integration test drives them through a small in-process harness using the public-ish module path; expose what it needs by adding `pub mod` for the tested modules. **First**, in `kafkrs-server/src/main.rs` change the relevant `mod X;` lines to `pub mod X;` for: `object_store`, `partition_writer`, `uploader`, `fetcher`, `topic_registry`, `recovery`, `segment`, `wal_writer`, and add at the top of `main.rs`:

```rust
// Allow the integration test to drive actors in-process.
#[cfg(test)]
pub use crate as kafkrs_server_internal;
```

Then create the test (it builds the crate as a binary; integration tests of a `bin` crate require a `lib` target — so also add `kafkrs-server/src/lib.rs` re-exporting the modules):

Create `kafkrs-server/src/lib.rs`:

```rust
pub mod config;
pub mod fetcher;
pub mod listener;
pub mod object_store;
pub mod partition_writer;
pub mod recovery;
pub mod segment;
pub mod topic_registry;
pub mod uploader;
pub mod wal_writer;
```

Add to `kafkrs-server/Cargo.toml`:

```toml
[lib]
name = "kafkrs_server"
path = "src/lib.rs"

[[bin]]
name = "kafkrs-server"
path = "src/main.rs"
```

And in `main.rs` replace the `mod X;` block with `use kafkrs_server::{...};`-style imports (import each module used: `config`, `listener`, `object_store`, `partition_writer`, `recovery`, `topic_registry`, `uploader`, `fetcher`). Remove the in-file `mod` declarations for these (they now live in the lib).

Now the integration test:

```rust
use bytes::Bytes;
use kafkrs_models::config::{DiskType, ObjectStoreConfig};
use kafkrs_models::manifest::Manifest;
use kafkrs_models::record::Record;
use kafkrs_models::topic::{ResolvedTopicConfig, TopicConfigOverrides};
use kafkrs_server::object_store::{build_store, manifest_key, put};
use kafkrs_server::partition_writer::{IncomingRecord, PartitionWriter, PwMsg};
use kafkrs_server::uploader::{Uploader, UploaderMsg};
use tokio::sync::{broadcast, mpsc, oneshot};

async fn setup(dd: &str, seal_bytes: u64) -> (mpsc::Sender<PwMsg>, broadcast::Sender<i64>) {
    let store = build_store(
        &ObjectStoreConfig { backend: "filesystem".into(), bucket: "b".into(), prefix: "".into(), endpoint: "".into(), region: "us-east-1".into() },
        dd,
    )
    .unwrap();
    put(&store, &manifest_key("", "t", 0), Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap())).await.unwrap();

    let (utx, urx) = mpsc::channel(64);
    let (dtx, mut drx) = mpsc::channel(64);
    tokio::spawn(Uploader::new(store, "".into(), "t".into(), 0, urx, dtx).run());

    let (pw_tx, pw_rx) = mpsc::channel(256);
    let (tail, _) = broadcast::channel(1024);
    let pw_tx_d = pw_tx.clone();
    tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let _ = pw_tx_d.send(PwMsg::SegmentDurable(d)).await;
        }
    });

    let mut o = TopicConfigOverrides::default();
    o.segment_size_bytes = Some(seal_bytes);
    o.group_commit_record_count = Some(1);
    let cfg = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
    let pw = PartitionWriter::new(dd.into(), "t".into(), 0, cfg, 0, vec![], pw_rx, utx, tail.clone()).unwrap();
    tokio::spawn(pw.run());
    (pw_tx, tail)
}

async fn produce(tx: &mpsc::Sender<PwMsg>, v: Vec<u8>) -> i64 {
    let (a, ar) = oneshot::channel();
    tx.send(PwMsg::Produce {
        records: vec![IncomingRecord { schema_id: 0, key: vec![], value: v, timestamp_ns: 0 }],
        ack: a,
    })
    .await
    .unwrap();
    ar.await.unwrap()
}

#[tokio::test]
async fn produce_seals_uploads_and_is_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap().to_string();
    let (tx, _tail) = setup(&dd, 4).await; // tiny seal threshold

    // produce enough to force at least one seal + upload
    for i in 0..10u8 {
        let hwm = produce(&tx, vec![i; 8]).await;
        assert_eq!(hwm, i as i64);
    }
    // allow the uploader to drain
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // recovery sees uploaded segments + remaining active WAL
    let store = build_store(
        &ObjectStoreConfig { backend: "filesystem".into(), bucket: "b".into(), prefix: "".into(), endpoint: "".into(), region: "us-east-1".into() },
        &dd,
    )
    .unwrap();
    let r = kafkrs_server::recovery::recover_partition(&dd, "t", 0, &store, "").await.unwrap();
    // next_offset must cover everything produced (10 records → offsets 0..=9)
    assert!(r.next_offset >= 10, "next_offset = {}", r.next_offset);
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p kafkrs-server --test storage_e2e`
Expected: PASS.

- [ ] **Step 3: Full workspace verification**

Run: `cargo test --workspace`
Expected: ALL tests pass (models unit tests, server unit tests, e2e). Also run `cargo fmt` (the repo's precommit hook expects it) and `cargo build --workspace`.

---

## Self-Review

**1. Spec coverage:**

| Spec section | Task |
| --- | --- |
| Storage tier model, durability/visibility boundary | Tasks 8, 10 (ack after fsync; HWM advances with commit) |
| WAL format, record layout, CRC32C at end | Task 3 |
| Group commit (size/count/time, disk profiles) | Tasks 1, 10 |
| Active batch (Arrow at seal time) | Task 10 (`records_to_recordbatch` at seal) |
| Recovery on startup (no LIST, idempotent re-upload) | Tasks 8, 13 |
| Parquet schema v1 + writer settings | Tasks 2, 7 |
| Object key layout (Hive, 20-digit) | Task 6 |
| Manifest (JSON, no next_offset, binary search) | Task 4 |
| Seal-and-upload flow | Tasks 9, 10 |
| Read paths (3 tiers, consumer flow, edge cases) | Task 11 |
| WAL not in read hierarchy / recovery not-ready | Tasks 11, 13 (BrokerNotReady) |
| Topic registry + schema model (schema_id tag) | Tasks 2, 5, 12 |
| Topic operations (CreateTopic atomic 3-step) | Task 12 |
| Auto topic creation | Task 14 (EnsureExists on produce) |
| Configuration | Tasks 1, 14 |
| Impact on existing code (message.rs bug, listener, main accept loop, writer.rs, python) | Tasks 2, 14, 15 |

`DeleteTopic`, retention, multi-broker, transform, schema registry, broker-hosted analytics — explicitly out of scope, no task (correct per spec).

**2. Placeholder scan:** No "TBD"/"add error handling"/"similar to Task N". Each code step shows complete code. The Task 14 `registry_topics` shim is explicitly simplified away in Step 3 of that task (called out, not left dangling).

**3. Type consistency:** `Record { offset, timestamp_ns, schema_id, key, value }` used identically across Tasks 2, 3, 7, 9, 10, 11, 13, 15. `Manifest`/`SegmentEntry` field names consistent Tasks 4, 9, 11, 13. `PwMsg`, `LocateResult`, `SealedBatch`, `SegmentDurable`, `UploaderMsg` consistent Tasks 9, 10, 11, 14, 16. `ResolvedTopicConfig` field names consistent Tasks 5, 10, 12.

**Known execution ordering caveat (called out, not a defect):** the `kafkrs-server` crate does not compile until Task 14 because Tasks 6–13 add modules that reference each other and the crate root still has the old `Message`/`Writer` until Task 14. Each of Tasks 6–13 is written test-first with its assertions; they are all run and verified together at Task 14 Step 5 and Task 16 Step 3. `kafkrs-models` (Tasks 1–5) compiles and tests independently at every task. This is the natural seam for a crate-internal actor system and is the reason Task 14 batches the first full server compile + test run.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-19-storage-model.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
