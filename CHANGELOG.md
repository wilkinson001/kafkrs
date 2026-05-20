# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the workspace's crates follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The three crates (`kafkrs-models`, `kafkrs-server`, `kafkrs-python`) are versioned in lockstep.

## [0.2.0] — 2026-05-20

Storage subsystem rewrite. The single-file Arrow IPC writer is replaced by a per-partition WAL + Parquet-on-object-store model with offset-resumable reads, three-tier read resolution, and an asynchronous uploader. See `docs/superpowers/specs/2026-05-18-storage-model-design.md` for the design rationale and `docs/superpowers/plans/2026-05-19-storage-model.md` for the implementation plan.

**This is a breaking release across all three crates.** No migration path from 0.1.0 data on disk; the WAL format, segment format, and wire envelope are all new.

### `kafkrs-models` 0.2.0

#### Added
- `record::Record` — WAL/wire envelope (`offset`, `timestamp_ns`, `schema_id`, `key: Vec<u8>`, `value: Vec<u8>`).
- `record::parquet_arrow_schema()` and `record::records_to_recordbatch()` — v1 envelope-only Parquet schema (`offset`, `timestamp_ns`, `key`, `value`, `schema_id`) with load-bearing column order; empty key materialises as a null.
- `wal` module — length-prefixed, CRC32C-validated binary codec (`encode_record`, `decode_record`, `scan_wal`) with `WalDecodeError::{Incomplete, CrcMismatch, Malformed}`. CRC trailer placement enables one-pass torn-tail recovery.
- `manifest` module — `Manifest` / `SegmentEntry` JSON types, binary-search `Manifest::segment_for_offset()`, `Manifest::last_uploaded_offset()`. `next_offset` is deliberately not stored.
- `topic` module — `TopicConfigOverrides`, `TopicEntry`, `TopicRegistryFile`, and `ResolvedTopicConfig::resolve()` for merging per-topic overrides over broker defaults. Public `DEFAULT_SEGMENT_SIZE_BYTES` / `DEFAULT_SEGMENT_SEAL_TIME_MS` / `DEFAULT_MAX_KEY_SIZE_BYTES` / `DEFAULT_MAX_VALUE_SIZE_BYTES` constants.
- `config::BrokerConfig`, `config::ObjectStoreConfig`, `config::DiskType` (`Nvme` / `Ssd` / `Rotational`), and `config::GroupCommitProfile` with per-disk defaults (5 ms / 64 KiB / 256 records for NVMe; 15 ms / 256 KiB / 1024 for SSD; 50 ms / 1 MiB / 4096 for rotational).
- Dependencies: `crc32c`, `serde_json`; dev-dependency `toml`.

#### Changed
- **Breaking:** `Config` schema overhaul. `Config` now requires `data_dir`, `[broker]`, and `[object_store]` sections; `logfile` is gone. `ports` (plural) replaces `port`.

#### Removed
- **Breaking:** `message` module and the `Message` struct. Replaced by `record::Record`. The `partition` field on records is gone (partition is request-envelope metadata, not per-record), `schema: Option<String>` is replaced by `schema_id: u32`, and `key` is now `Vec<u8>` instead of `String`.
- **Breaking:** `Config.logfile` field.

#### Fixed
- Arrow schema field for the timestamp column was mis-named `"partition"` in `arrow_schema()`. The new `parquet_arrow_schema()` names it `"timestamp_ns"`.

### `kafkrs-server` 0.2.0

#### Added
- `object_store` module — backend-agnostic store construction (`filesystem` for local testing, `s3` for AWS/MinIO/R2/etc.) plus Hive-partitioned, 20-digit zero-padded key helpers (`segment_key`, `manifest_key`) and async `put` / `get` / `get_range`.
- `segment` module — `write_segment()` produces a single-row-group Parquet object with zstd(3), 1 MiB pages, page-level statistics, and dictionary encoding on `schema_id`.
- `wal_writer` module — per-segment `WalFile` (`open` / `append_and_sync` / `delete`) and `recover_wal_file()` that scans, validates, and truncates a WAL file at the first invalid record. `append_and_sync` is the durability boundary: producer acks happen only after `fsync` returns.
- `uploader` module — `Uploader` actor: Parquet write → object PUT → idempotent manifest read-modify-PUT → `SegmentDurable` notification. Failed uploads retry indefinitely (the WAL retains the data). Deterministic keys + sorted segment list make re-uploads bit-identical no-ops.
- `partition_writer` module — `PartitionWriter` actor: per-`(topic, partition)` owner of offsets, the active WAL file, the in-memory active batch (`Vec<Record>`, converted to Arrow at seal time), the pre-fsync `pending` buffer, the in-flight upload queue, and a `tokio::sync::broadcast` for tail consumers. Group commit fires on size, record count, or time threshold; segment seal fires on byte threshold; the WAL file for a sealed segment is deleted only after the Uploader reports it durable.
- `fetcher` module — three-tier read resolution (active batch → in-flight queue → object store) via `LocateResult`, long-poll on `from_offset > HWM` against the tail broadcast, and explicit error variants (`UnknownTopic`, `UnknownPartition`, `OffsetOutOfRange`, `BrokerNotReady`).
- `topic_registry` module — `TopicRegistry` actor owning `topics.json`. `CreateTopic` is atomic across three steps (registry rewrite via tmp+fsync+rename, WAL directory creation, empty-manifest PUT per partition). `EnsureExists` powers broker-level auto-create-on-produce. `snapshot()` returns the resolved per-partition config for startup bring-up.
- `recovery` module — per-partition startup reconciliation: lists local `.wal` files, fetches the partition manifest once (no object-store LIST), deletes WALs fully covered by uploaded segments, replays the active WAL into memory, and re-queues sealed-but-not-uploaded orphan segments for the Uploader. `next_offset` is derived from the manifest + WAL tail.
- `listener` module — framed length-prefixed I/O (4-byte LE length + bincode body) carrying `WireRequest::{Produce, Fetch}` and `WireResponse::{Produced, Fetched, Error}`. Per-connection `Listener::process()` decodes a request, routes it to the partition's `pw_tx`/`tail`, and writes the response. Replaces the unsound `read_to_end`-per-message loop.
- `kafkrs-server` is now a `[lib]` + `[[bin]]` crate. Internal modules are exported through `kafkrs_server::*` so that integration tests in `kafkrs-server/tests/` can drive the actors directly.
- Integration test `tests/storage_e2e.rs::produce_seals_uploads_and_is_recoverable`: 10 produces with a 4-byte seal threshold exercise the full produce → fsync ack → seal → upload → recovery loop.
- Configuration: `data_dir`, `[broker]` (`disk_type`, `auto_create_topics`, `default_partition_count`), `[object_store]` (`backend`, `bucket`, `prefix`, `endpoint`, `region`); `config.toml` updated accordingly.
- Dependencies: `object_store` (with `aws` feature), `bytes`, `anyhow`, `parquet`, `crc32c`, `serde_json`, `env_logger`; dev-dependency `tempfile`. Added `"time"` to the tokio feature set.

#### Changed
- **Breaking:** Per-partition `PartitionWriter` actors replace the single global `Writer`. Each partition owns its own WAL file and offset counter; cross-actor communication is via `tokio::sync::mpsc` and `broadcast`.
- **Breaking:** Producer ack is now gated on WAL `fsync`, not on Arrow IPC buffering. Consumer visibility advances at the same instant.
- **Breaking:** On-disk format is Parquet segments in an S3-compatible store (or local filesystem for testing), indexed by a per-partition JSON manifest, instead of one sealed Arrow IPC file per process.
- `config::load_config` is now `pub` (was `pub(crate)`) so the integration test target can call it from outside the bin.

#### Removed
- **Breaking:** `writer.rs` and the `Writer` struct. The Arrow IPC `FileWriter` is gone; segments are Parquet, the WAL is the durability boundary, and the broken shutdown path that called `arrow_writer.finish()` without flushing the in-memory buffer no longer exists.
- **Breaking:** `arrow-ipc` dependency.

#### Fixed
- Accept-loop bug in `main.rs` that called `TcpListener::accept()` exactly once per port and never re-accepted. The new accept loop runs `loop { listener.accept().await }` and spawns a `Listener::process()` task per connection.
- Pre-fsync records are no longer silently dropped on shutdown: they were never acked to the producer, so producers know to retry. Anything that *was* acked is durable on disk and recovered on startup.

### `kafkrs-python` 0.2.0

#### Changed
- **Breaking:** `encode_message(key, value, schema, partition)` is now `encode_message(key, value, schema_id, timestamp_ns=0)`.
  - `key`: `str` → `bytes` (`Vec<u8>`).
  - `value`: already `bytes`; serialisation unchanged.
  - `schema: Option<str>` → `schema_id: u32` (producer-assigned tag; `0` means "no schema / opaque").
  - `partition` parameter removed (partition routing is request envelope metadata, not record-level).
  - New `timestamp_ns: i64 = 0` parameter; `0` means "broker-stamps it on arrival".
- Bincode encoding switched from `config::legacy()` to `config::standard()` to match the new server-side framing.
- Now imports `kafkrs_models::record::Record` (was `kafkrs_models::message::Message`).

## [0.1.0]

Initial prototype: a single-broker TCP listener wrote bincode-decoded `Message`s to one Arrow IPC file via `arrow_ipc::FileWriter`. No persistence guarantees on shutdown, no offset model, no partitioning, no object-store tier. Replaced wholesale by 0.2.0.
