# Storage Model Design — kafkrs v1

**Status:** Draft for review
**Date:** 2026-05-18
**Scope:** Storage subsystem (WAL, segment format, read paths, topic registry). Wire protocol, transform layer, and multi-broker replication are out of scope and have their own future brainstorms.

## Motivation

The current `kafkrs` broker writes a single Arrow IPC file per process. The file uses `arrow_ipc::writer::FileWriter`, which produces a sealed file only readable after the writer calls `finish()`. The codebase already anticipates the need for multiple topics with multiple partitions and a per-partition writer (per the README), and the in-flight buffer drops partial batches on shutdown today.

This spec defines a storage model that:

- supports offset-based resumable reads from any position in the log (the primary access pattern),
- backs durable storage with an S3-compatible object store while serving the latency-sensitive hot path from local disk,
- leaves a clean seam for a future in-broker analytical query surface and a future transform layer,
- avoids the failure modes of object-store-backed systems that LIST on the hot path or on startup.

## Design choices, with rationale

### Storage tier model

Two tiers, with a strict relationship between them:

- **Local hot tier:** a per-partition write-ahead log (WAL) on local disk plus in-memory Arrow column builders representing the active batch and any sealed-but-not-yet-uploaded batches.
- **Object store cold tier:** Parquet segments, one per sealed batch, written to an S3-compatible bucket. A per-partition manifest object indexes the segments.

**Durability boundary:** producer ack happens after WAL `fsync`. Object store upload is asynchronous. A broker crash between WAL fsync and successful upload does not lose data — the WAL retains the records and the upload retries on the next startup. A broker crash between record arrival and WAL fsync drops the record (producer hasn't been acked, may retry).

**Visibility boundary:** records become visible to consumers at the same instant they become durable to producers — i.e., after WAL fsync. There are no "uncommitted reads."

### Why not Arrow IPC on disk

`arrow_ipc::FileWriter` produces a sealed file with a footer index. The footer only exists after `finish()`, so an active segment is not seekable while it's being written. `arrow_ipc::StreamWriter` supports continuous append but has no footer index. Neither fits an append-only log with random-access reads.

Parquet was specifically designed for partial reads over a network: per-column statistics, page-level indexes, dictionary encoding, bloom filters, predicate pushdown via row-group metadata. It's also the lingua franca of the analytical-engine ecosystem (DuckDB, DataFusion, Polars, Trino, Spark, Iceberg), which is what makes the "queryable log" story tractable without us building a query engine.

Arrow remains the in-memory representation throughout the broker — the active batch is an Arrow column builder, and the row-to-columnar conversion happens once at WAL-fsync time. The original "minimise serialisation overhead" intuition is preserved at the compute boundaries where it matters; only the disk format changes.

### Hot path stays local; analytical reads are direct-to-bucket

Hot-path producer acks fsync to local WAL and so run at single-digit milliseconds. Tail consumers are served from in-memory column builders via a `tokio::sync::broadcast` notification from the Partition Writer — zero disk reads.

Analytical readers (DuckDB, DataFusion, Polars) read Parquet objects directly from the bucket. The broker does not serve analytical queries in v1. Freshness of analytical reads is bounded by the segment seal time (default 60 s; configurable down to ~5 s at the cost of more, smaller Parquet files). A broker-hosted query surface (Arrow Flight SQL + DataFusion federation across in-memory and object-store data) is the v2 plan.

## Architecture

```
                              ┌────────────────────────────────────────────┐
                              │              Broker process                │
                              │                                            │
producer ──TCP──▶  Listener ──▶  Partition Writer ──▶  WAL ──▶  Uploader  │──▶ Object store
                  (per conn)    (per partition;        │       (per         │   (Parquet
                                 active batch,         │        partition)  │    segments
                                 group commit)         │                    │   + manifest)
                                                       │                    │
consumer ──TCP──▶  Fetcher  ◀──┴────────── reads ──────┴────────────────────┘
                  (per conn)    (in-memory → in-flight queue → object store)

control plane ──▶  Topic registry (per-topic config, schema_id allowance)
```

### Actors

- **Listener.** Terminates TCP connections, parses framed requests, dispatches. Replaces the current single-shot `Listener` in `kafkrs-server/src/listener.rs`. Wire-protocol details are out of scope for this spec.
- **Partition Writer.** One per `(topic, partition)`. Owns the WAL file, the in-memory active batch (Arrow column builders), the in-flight upload queue, and the partition's offset counter. The only writer to its WAL file; the only authority on offsets for its partition.
- **Uploader.** One per partition. Receives sealed batches from the Partition Writer, writes Parquet objects, updates the partition manifest, signals back when the upload is durable so the corresponding WAL file can be deleted.
- **Fetcher.** Handles consumer fetches. Resolves offsets against the three-tier read hierarchy (in-memory active batch → in-flight upload queue → object store). Maintains a cached copy of the manifest, invalidated by Uploader notifications.
- **Topic Registry.** Holds per-topic config (partition count, segment thresholds, group-commit knobs). Backed by `./data/topics.json`, loaded on startup, rewritten atomically on changes.

Cross-actor communication uses `tokio::sync::mpsc` channels, matching the pattern already established in `kafkrs-server/src/main.rs`.

### Invariants

- Producer ack is gated on WAL fsync, never on object store upload.
- Consumer visibility advances at the same step as producer ack (no uncommitted reads).
- The Partition Writer is the sole writer to its WAL file and sole authority on offsets.
- The Uploader deletes a WAL file only after the corresponding manifest update is durable in the object store, so a crash never loses acked records.
- The broker does not LIST the object store on the hot path or on startup; the manifest is the index, and crash-recovery orphans are resolved via idempotent re-upload.

## WAL format

WAL files are per-partition, per-segment:

```
./data/wal/<topic>/<partition>/<base_offset>.wal
```

Each WAL file accumulates records for exactly one segment. When the segment seals, a new WAL file opens for the next. After successful upload and manifest update, the WAL file is deleted. No in-file truncation.

### Record layout

Length-prefixed, CRC-validated, little-endian:

| field | size | notes |
| --- | --- | --- |
| `length` | 4 B | bytes from `offset` through `value`, excluding CRC |
| `offset` | 8 B | partition-local, monotonic |
| `timestamp_ns` | 8 B | producer-supplied or broker-stamped if absent |
| `schema_id` | 4 B | 0 = opaque; tag only in v1, see Schema model |
| `key_len` | 2 B | u16, max 64 KiB; 0 = no key (null and empty collapsed in v1) |
| `value_len` | 4 B | u32, max 4 GiB |
| `key` | `key_len` B | opaque bytes, drives partition routing |
| `value` | `value_len` B | payload |
| `crc32c` | 4 B | CRC32C (Castagnoli) over `offset` through `value` |

Fixed header overhead: 30 B per record plus the 4 B CRC trailer (34 B).

### Justification of the size choices

- **`length` 4 B (u32).** Frames the record; gives ~4 GiB headroom. Smaller widths constrain message size; larger is wasteful.
- **`offset` 8 B.** Partitions can run for years; 4 B would be reachable on a high-throughput partition over its lifetime.
- **`timestamp_ns` 8 B.** Nanoseconds since Unix epoch; matches `i64`.
- **`schema_id` 4 B.** Tag space large enough for any registry future; dictionary-encodes well in Parquet given typical low cardinality.
- **`key_len` 2 B (u16, max 64 KiB).** Kafka keys are practically tiny (UUIDs, snowflake IDs, composite identifiers); 2 B makes "keys are not large" a property of the format. A separate broker-level `max_key_size_bytes` (default 1024) acts as a soft policy ceiling.
- **`value_len` 4 B.** Matches the outer `length` cap.

### Why the CRC is at the end

Disk writes happen page-at-a-time (~4 KiB on Linux). A torn write at crash time most often loses the *trailing* bytes of a record. Placing the CRC adjacent to the last bytes ensures:

1. Torn tails trivially fail validation.
2. Recovery is a one-pass scan: validate, repeat; first failure is the truncation point.
3. The writer computes CRC incrementally as it streams the body, then emits — no need to buffer the full record to compute CRC up-front.

This matches Postgres WAL, RocksDB WAL, Kafka log segments, and the LevelDB/Pebble log format.

### CRC32C, not CRC32

Modern CPUs have a hardware instruction for CRC32C (Castagnoli polynomial); it's the storage-layer integrity standard (ZFS, ext4 metadata, RocksDB, Kafka).

### Group commit

Producer-ack latency is determined by the group-commit window. Per-partition:

- A `PendingBatch` accumulates incoming records in memory (pre-fsync, invisible to consumers).
- A commit fires when **either** the batch reaches a size threshold **or** a time threshold elapses since the first record arrived.
- On commit: serialise all pending records, single `writev` to the WAL, `fsync`, then move records into the active batch and fire all pending producer acks. The high watermark advances; tail consumers subscribed via broadcast are notified.

Defaults depend on disk type (see Config):

| disk type | `time_ms` | `size_bytes` | `record_count` |
| --- | --- | --- | --- |
| `nvme` | 5 | 64 KiB | 256 |
| `ssd` | 15 | 256 KiB | 1024 |
| `rotational` | 50 | 1 MiB | 4096 |

Rotational defaults are larger because fsync on a 7200 RPM platter is ~5–10 ms; a 5 ms group window would mostly be waiting on the sync. The larger window amortises slow fsyncs at the cost of higher tail latency. Throughput remains high; latency does not.

All three knobs are also per-topic overridable.

### Active batch (in-memory shape)

Once records pass WAL fsync, they are appended to an Arrow column builder in memory — one per envelope field. The row-to-columnar conversion happens once, at WAL-fsync time. The active batch is what tail consumers slice from, and what gets finalised into a `RecordBatch` and handed to the Uploader at seal time.

### Recovery on startup

Per partition:

1. List `.wal` files in the partition directory (filesystem listing, not object store).
2. For each WAL file, scan records, validate CRC and length, truncate at first invalid record.
3. Cross-reference against the partition manifest fetched from the object store (single GET):
   - WAL files whose base offset is fully covered by an uploaded segment → delete (cleanup from crash between manifest update and WAL delete).
   - WAL files above the last uploaded offset → keep, replay into the active batch, queue for upload.
4. Next offset = (last valid WAL record offset) + 1.

No object-store LIST is performed. Orphan Parquet objects from a crash between segment PUT and manifest update are resolved by **idempotent re-upload**: the object key is deterministic in `base_offset`, so the re-upload either overwrites a successful previous upload with bit-identical content (no-op) or completes the failed PUT for the first time. After re-upload + manifest update, the state is consistent.

## Segment format on the object store

### Parquet schema (v1, envelope-only)

| column | parquet type | notes |
| --- | --- | --- |
| `offset` | `INT64 REQUIRED` | sorted; enables predicate pushdown for offset ranges |
| `timestamp_ns` | `TIMESTAMP(NANOS, UTC) REQUIRED` | enables time-range pruning |
| `key` | `BINARY OPTIONAL` | nullable for "no key" records |
| `value` | `BINARY REQUIRED` | opaque payload bytes |
| `schema_id` | `INT32 REQUIRED` | dictionary-encoded; low cardinality |

This is the **v1 format**. v2 will extend it additively: when a topic declares a payload schema, the Uploader decodes the payload at write time and emits additional Parquet columns per payload field. Old readers see only envelope columns; new readers see envelope plus payload. The v1 format does not change.

### Writer settings

| setting | value | rationale |
| --- | --- | --- |
| Row groups per segment | 1 | simplest writer; no mid-write decisions about row group boundaries |
| Page indexes | enabled | cheap to write; enables fine-grained pruning for analytical queries even with one row group |
| Page size | 1 MiB | Parquet default |
| Compression | zstd (level 3) | strictly better than snappy; standard for new Parquet writers |
| Column encoding | dictionary for `schema_id`; plain for `offset`, `timestamp_ns`, `key`, `value` | |

If profiling later shows analytical query latency is dominated by GET volume, the natural next step is multiple row groups per segment (e.g., 16 MiB each), which requires the writer to flush partial state mid-segment.

### Object key layout (Hive-style partitioning)

```
s3://<bucket>/<prefix>/<topic>/partition=<n>/segment-<base_offset:020>.parquet
s3://<bucket>/<prefix>/<topic>/partition=<n>/manifest.json
```

Two specific choices:

- **`partition=N` directory naming.** Hive-style partitioning; DuckDB, DataFusion, and Spark automatically surface `partition` as a virtual column for queries.
- **20-digit zero-padded `base_offset`.** Lexicographic listings are offset-ordered. Matches Kafka's segment naming.

### Manifest

One manifest per `(topic, partition)`. JSON for v1.

```json
{
  "topic": "orders",
  "partition": 3,
  "format_version": 1,
  "segments": [
    {
      "base_offset": 0,
      "last_offset": 99999,
      "base_timestamp_ns": 1700000000000000000,
      "last_timestamp_ns": 1700000060000000000,
      "record_count": 100000,
      "byte_size": 12345678,
      "object_key": "segment-00000000000000000000.parquet"
    }
  ]
}
```

The manifest is the fast index for `offset → segment` and `timestamp → segment` resolution. Readers binary-search the segments list. **`next_offset` is deliberately not in the manifest** — it is a broker-local concept; placing it in the manifest would falsely imply the manifest is the source of truth for "what offset comes next."

In a single-broker world, the Uploader for a given partition is the only writer of the manifest, so atomicity is per-S3-PUT (which is atomic per object). Multi-broker contention is a known v2 problem with candidate paths flagged in **Out of scope**.

### Seal-and-upload flow

When the active batch hits the seal threshold (size or time):

1. Partition Writer freezes its column builders, finalises a `RecordBatch`, hands it to the Uploader with `(base_offset, last_offset, base_timestamp_ns, last_timestamp_ns)`. A fresh column builder starts for the next batch.
2. Uploader writes Parquet to a temp byte buffer using the writer settings above.
3. Uploader PUTs the Parquet object to `s3://.../partition=N/segment-<offset>.parquet`.
4. Uploader fetches the current manifest, appends the new segment entry, PUTs the updated manifest.
5. Uploader notifies the Partition Writer: "segment with `base_offset = X` is durable."
6. Partition Writer deletes the corresponding `<X>.wal` file.

Each step is idempotent across crashes.

## Read paths

The Fetcher resolves consumer reads against three tiers in order.

### Tier 1 — In-memory active batch (Partition Writer)

Reads issued as messages to the Partition Writer; it slices column builders and returns a `RecordBatch`. Tail-subscribed consumers receive new records via `tokio::sync::broadcast` after each group commit. Zero disk reads.

### Tier 2 — In-flight upload queue

Sealed batches awaiting upload sit as immutable `RecordBatch`es in RAM, indexed by base offset. Lookup is a small in-memory range check. Tier exists for at most a few seconds in normal operation.

### Tier 3 — Object store

For offsets older than the earliest in-flight batch:

1. Fetcher consults its cached manifest (refreshed via Uploader notifications).
2. Binary-search the segments list to find the segment containing the target offset.
3. Range-GET the Parquet from S3, filter by `offset >= from_offset`, return the slice.

### Consumer flow

```
Consumer sends: Fetch { topic, partition, from_offset, max_records, max_wait_ms }

Fetcher:
  1. resolve (topic, partition) → Partition Writer handle.
  2. ask Partition Writer for HWM and the location of from_offset:
       - if from_offset > HWM:
           if max_wait_ms > 0: subscribe to broadcast, await advancement or timeout
           else: return empty response with HWM
       - if in active batch: slice from column builders
       - if in in-flight queue: return queued RecordBatch slice
       - else: serve from object store (manifest → segment → Parquet)
  3. respond with records and current HWM.
```

### Why the WAL is not in the read hierarchy

The WAL is redundant with the in-memory tiers in steady state — every WAL record is also represented in either the active batch or the in-flight queue. The WAL exists only to rebuild in-memory state after a crash, and during recovery the partition is marked "not ready" and fetches wait. This is standard behaviour for systems of this shape and is much simpler than serving from WAL during recovery.

### Edge cases

- `from_offset > HWM` → long-poll subscribe; on timeout return empty with HWM.
- `from_offset < earliest available offset` → `OffsetOutOfRange`. v1 keeps everything, so this is only triggered by negative offsets.
- Topic does not exist → `UnknownTopic`. Auto-create-on-fetch is not supported (see Topic operations).
- Topic exists but partition id is out of range → `UnknownPartition`.
- Recovery in progress for that partition → `BrokerNotReady` (or wait, depending on protocol decision).

### Analytical query path (separate)

Analytical queries against the bucket use DuckDB, DataFusion, Polars, etc., reading Parquet directly. The broker does not participate. The Hive-style partition naming makes `partition` a virtual column for free. Unsealed data is invisible to analytical queries; freshness is bounded by `segment_seal_time_ms`. v2 plans Arrow Flight SQL + DataFusion federation across in-memory and object-store data; see **Out of scope**.

## Topic registry and schema model

### Topic schema

| field | notes |
| --- | --- |
| `name` | string, unique |
| `partition_count` | u32, immutable in v1 |
| `created_at_ns` | i64 |
| `config` | overrides on broker-level defaults |

Per-topic overridable broker defaults:

```
segment_size_bytes             default 128 MiB
segment_seal_time_ms           default 60_000
max_key_size_bytes             default 1024
max_value_size_bytes           default 1 MiB
group_commit_time_ms           default per disk_type profile
group_commit_size_bytes        default per disk_type profile
group_commit_record_count      default per disk_type profile
```

### Registry persistence

`./data/topics.json`, loaded on startup, held in memory, rewritten atomically on changes (write `topics.json.tmp`, fsync, rename). Multi-broker coordination of the registry is a v2 problem.

### Schema model (v1)

The `schema_id` field on each WAL record is a producer-supplied integer with deliberately thin broker semantics:

- `0` is reserved for "no schema / opaque bytes."
- Any other value is a producer-meaningful tag. The broker stores it, makes it queryable as a column in the Parquet segment, and otherwise treats it as opaque.
- No schema *definitions* are stored in v1. The broker does not know what any specific `schema_id` means.

This is enough to let producers tag records and consumers filter (`WHERE schema_id = 5`) without committing to a schema registry implementation. The model extends additively in v2 to a real schema registry that drives schema-aware Parquet (payload fields as native Parquet columns).

### Topic operations

| operation | scope |
| --- | --- |
| `CreateTopic(name, partition_count, config_overrides)` | v1 |
| `DescribeTopic(name)` | v1 |
| `ListTopics()` | v1 |
| `DeleteTopic(name)` | **v1.5+** — raises non-trivial atomicity questions about WAL + S3 cleanup. |

`CreateTopic` rejects if the topic already exists. On success it:

1. Adds the topic to `topics.json` and rewrites the file atomically.
2. Creates the WAL directories: `./data/wal/<topic>/<partition>/` for each partition `0..partition_count`.
3. Writes an empty manifest to `s3://.../<topic>/partition=<n>/manifest.json` for each partition. Empty manifests have `segments: []`.

The empty-manifest precondition simplifies the Uploader's update logic: it can always assume a manifest exists for the partition it's writing to. The CreateTopic operation must be atomic across these three steps; a crash mid-way must leave the topic recoverable on next startup (either fully created or fully absent — detected by the presence of the entry in `topics.json` after fsync).

Control-plane operations are serialised through the Topic Registry actor and do not contend with produce/fetch.

### Auto topic creation

When `broker.auto_create_topics = true` (default `false`), a produce to a non-existent topic synthesises a `CreateTopic` call before the produce is handled, using:

- `partition_count = broker.default_partition_count` (default `1`).
- No config overrides; topic uses broker-level defaults for everything.

Semantics:

- **Produce only.** Fetch requests against an unknown topic return `UnknownTopic`, even with auto-create enabled. Auto-create-on-fetch is a documented Kafka misfeature we deliberately avoid.
- **First-produce latency.** Auto-create performs `partition_count` S3 PUTs for empty manifests, costing roughly 50–200 ms each. The first producer to a new topic sees this as a one-time slow start; subsequent produces are normal.
- **Concurrency.** Concurrent produces to the same new topic serialise through the Topic Registry actor: the first creates, the rest observe the topic exists and proceed.
- **Logging.** The broker emits a warning log on every auto-creation with the topic name and the producer identity (when the wire protocol carries one). This is the primary operational defence against typo-induced topic sprawl.
- **Default off.** Matches Kafka 3.x+ default. Auto-create-on is a documented footgun (typo-creates-topic, no `DeleteTopic` in v1 to clean up).

## Configuration

`config.toml` grows as follows:

```toml
address = "127.0.0.1"
ports = [5432]              # plural; fixes the existing port-vs-ports inconsistency
data_dir = "./data"

[broker]
disk_type = "nvme"          # nvme | ssd | rotational; sets group-commit defaults
auto_create_topics = false  # produce to unknown topic auto-creates; default off
default_partition_count = 1 # for auto-created topics

[object_store]
backend = "s3"              # s3 | filesystem (for local testing)
bucket = "kafkrs-data"
prefix = ""
endpoint = ""               # for non-AWS S3-compatible stores (MinIO, R2, etc.)
region = "us-east-1"
# credentials sourced from env / IAM
```

Per-topic overrides live in the topic registry, not `config.toml`.

## Impact on existing code

| location | change |
| --- | --- |
| `kafkrs-models/src/message.rs:25` | Standalone bug: Arrow `Field` for timestamp is mis-named `"partition"`. Fix to `"timestamp"`. |
| `kafkrs-models/src/message.rs` | `Message` becomes a WAL/wire record: `key: Vec<u8>` (was `String`); drop `partition: Option<String>` (partition is request-envelope metadata, not per-record); replace `schema: Option<String>` with `schema_id: u32`. |
| `kafkrs-models/src/message.rs` | `arrow_schema()` and `messages_to_recordbatch()` move into the segment-writer codepath; the Parquet schema (envelope columns) replaces the Arrow IPC schema. |
| `kafkrs-server/src/listener.rs` | Replaced. The current `read_to_end`-per-message loop is unsound; replaced by framed length-prefixed I/O. Wire protocol details are a separate brainstorm. |
| `kafkrs-server/src/main.rs:46` | Port loop calls `accept()` once and never re-accepts. Replaced by a proper accept-loop per listener. |
| `kafkrs-server/src/writer.rs` | The single global `Writer` becomes per-partition `PartitionWriter` actors. The Arrow IPC `FileWriter` is replaced by the WAL writer + per-partition `Uploader`. |
| `kafkrs-server/src/writer.rs` | Current shutdown path calls `arrow_writer.finish()` but never flushes the partial in-memory buffer. Fixed by the new "WAL is the durability boundary" model — WAL records are durable as soon as fsynced, so shutdown drops only pre-fsync pending records (which the producer has not been acked for). |
| `config.toml` | `port = 5432` (singular) does not match `Config { ports: Vec<u16> }`. New config also adds `data_dir`, `[broker]`, and `[object_store]`. |
| `kafkrs-python/src/lib.rs` | `encode_message` signature changes: `key` becomes `bytes` not `str`, `value` stays `bytes`, drops `partition`, adds `schema_id: u32`. |

These are consequences of the design, not extra work — listed so the implementation plan is honest about the surface area.

## Out of scope (v2 and later)

Explicit deferrals so the spec stays focused:

- **Wire protocol.** This spec assumes a framed produce/fetch protocol exists. Designing the protocol is a separate brainstorm. The storage actors are robust to most reasonable protocol shapes.
- **Transform layer.** Architecture has a clean seam between Listener/Partition Writer and Partition Writer/Fetcher. Design is a separate brainstorm.
- **Multi-broker / replication.** Single broker for v1. Known v2 design problems:
  - **Manifest contention.** Candidate paths: manifest-as-append-log (each upload writes its own small entry; readers merge — Iceberg-style), conditional PUTs (`If-Match` etag — now supported on S3, GCS, R2), or partition leadership (only one broker writes the manifest per partition).
  - **Topic registry coordination.** Candidate paths: single canonical file in the object store with conditional PUT, or a separate consensus layer.
  - **Partition routing.** A producer connecting to broker X may need to be routed to broker Y for partition P. Affinity vs. forwarding is a design choice.
- **Schema registry.** Schema definitions, validation, evolution rules. The v1 `schema_id`-as-tag model extends additively.
- **Schema-aware Parquet** (payload fields as native columns). Depends on the schema registry.
- **Broker-hosted analytical query surface.** Planned design: Arrow Flight SQL + DataFusion federating across in-memory active batch and object-store segments. Fits naturally with multi-broker (Flight SQL is connection-oriented, so multi-broker just adds query routing). Snapshot-endpoint shortcuts (X1 from the brainstorm) are explicitly rejected — they commit to an awkward UX and don't make the v2 plan cheaper to build.
- **Retention, segment deletion, log compaction.** v1 keeps everything.
- **Consumer groups and offset commit storage.** Consumers track their own offsets in v1.
- **Authentication / authorisation.** Broker is trusted.
- **`DeleteTopic`.** Atomicity of WAL + S3 cleanup deserves its own treatment.
- **Object store complete-loss behaviour.** v1 retries indefinitely with the WAL retaining data; v2 will design explicit "degraded" behaviour for prolonged outages.
- **WAL file preallocation, `fdatasync` / `sync_file_range` per disk type.** Worth a v1.5 knob; not on the critical path.
- **Manifest binary/compacted format.** JSON manifest is fine at v1 scales. When manifests grow to many MB or segment counts grow large, the manifest moves to a binary format and/or older segments get consolidated into a single entry.

## Open questions / risks the implementation should monitor

- **Object store latency variability.** S3 PUTs can occasionally take seconds. Uploader needs sensible timeout + exponential backoff. On persistent failure, the WAL retains the data and the upload retries indefinitely.
- **Group-commit defaults assume the configured `disk_type` matches reality.** Misconfiguration (e.g., NVMe profile on a rotating disk) produces poor throughput. Document clearly; consider an optional startup self-test in v1.5.
- **Memory footprint at high partition counts.** Each partition keeps an active batch (column builders) + an in-flight upload queue + a cached manifest. Fine at v1 scales (tens to low hundreds of partitions). High thousands would need attention.
- **Manifest GET latency on startup.** One GET per partition. At hundreds of partitions, total startup time is bounded by the slowest GET. Should not be on the critical path of producer ingest start; design the startup sequence so partitions can come online independently.

## Invariants (summary for implementers)

These are the load-bearing properties the implementation must preserve:

1. Producer ack is gated only on WAL fsync.
2. Consumer visibility advances at the same step as producer ack — no uncommitted reads.
3. The Partition Writer is the sole writer to its WAL file and sole authority on its offset counter.
4. The Uploader deletes a WAL file only after the corresponding manifest update is durable in the object store.
5. The broker does not LIST the object store on the hot path or on startup. The manifest is the index; orphan reconciliation is via idempotent re-upload.
6. Object keys for segments are deterministic functions of `base_offset` to enable invariant 5.
7. Group-commit window defaults match the configured `disk_type`.
