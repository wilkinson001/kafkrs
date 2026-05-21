# Changelog — `kafkrs-models`

All notable changes to this crate are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The three crates in this workspace (`kafkrs-models`, `kafkrs-server`, `kafkrs-python`) are versioned in lockstep.

## [0.3.0] — 2026-05-21

Wire protocol v1 lands. See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md` for the design.

### Added
- `kafkrs-models/proto/wire/v1.proto` — canonical protobuf schema for the kafkrs wire protocol. Defines `Command` (top-level envelope), `Connect`/`Connected`, `Ping`/`Pong`, `Produce`/`ProduceResp`, `Fetch`/`FetchResp`, `CreateTopic`/`DescribeTopic`/`ListTopics`, `Error` + `ErrorCode`. Field-number ranges `40–49` and `50–59` reserved for future streaming-consumer and admin RPCs.
- `kafkrs-models::wire::v1` module — `prost`-generated Rust bindings, produced at compile time by `build.rs`.
- Dependencies: `prost`, `prost-build`, `protoc-bin-vendored` (build-time, hermetic — no system protoc required).

## [0.2.0] — 2026-05-20

Storage subsystem rewrite. The single-file Arrow IPC writer is replaced by a per-partition WAL + Parquet-on-object-store model with offset-resumable reads, three-tier read resolution, and an asynchronous uploader. See `docs/superpowers/specs/2026-05-18-storage-model-design.md` for the design rationale and `docs/superpowers/plans/2026-05-19-storage-model.md` for the implementation plan.

**This is a breaking release across all three crates.** No migration path from 0.1.0 data on disk; the WAL format, segment format, and wire envelope are all new.

### Added
- `record::Record` — WAL/wire envelope (`offset`, `timestamp_ns`, `schema_id`, `key: Vec<u8>`, `value: Vec<u8>`).
- `record::parquet_arrow_schema()` and `record::records_to_recordbatch()` — v1 envelope-only Parquet schema (`offset`, `timestamp_ns`, `key`, `value`, `schema_id`) with load-bearing column order; empty key materialises as a null.
- `wal` module — length-prefixed, CRC32C-validated binary codec (`encode_record`, `decode_record`, `scan_wal`) with `WalDecodeError::{Incomplete, CrcMismatch, Malformed}`. CRC trailer placement enables one-pass torn-tail recovery.
- `manifest` module — `Manifest` / `SegmentEntry` JSON types, binary-search `Manifest::segment_for_offset()`, `Manifest::last_uploaded_offset()`. `next_offset` is deliberately not stored.
- `topic` module — `TopicConfigOverrides`, `TopicEntry`, `TopicRegistryFile`, and `ResolvedTopicConfig::resolve()` for merging per-topic overrides over broker defaults. Public `DEFAULT_SEGMENT_SIZE_BYTES` / `DEFAULT_SEGMENT_SEAL_TIME_MS` / `DEFAULT_MAX_KEY_SIZE_BYTES` / `DEFAULT_MAX_VALUE_SIZE_BYTES` constants.
- `config::BrokerConfig`, `config::ObjectStoreConfig`, `config::DiskType` (`Nvme` / `Ssd` / `Rotational`), and `config::GroupCommitProfile` with per-disk defaults (5 ms / 64 KiB / 256 records for NVMe; 15 ms / 256 KiB / 1024 for SSD; 50 ms / 1 MiB / 4096 for rotational).
- Dependencies: `crc32c`, `serde_json`; dev-dependency `toml`.

### Changed
- **Breaking:** `Config` schema overhaul. `Config` now requires `data_dir`, `[broker]`, and `[object_store]` sections; `logfile` is gone. `ports` (plural) replaces `port`.

### Removed
- **Breaking:** `message` module and the `Message` struct. Replaced by `record::Record`. The `partition` field on records is gone (partition is request-envelope metadata, not per-record), `schema: Option<String>` is replaced by `schema_id: u32`, and `key` is now `Vec<u8>` instead of `String`.
- **Breaking:** `Config.logfile` field.

### Fixed
- Arrow schema field for the timestamp column was mis-named `"partition"` in `arrow_schema()`. The new `parquet_arrow_schema()` names it `"timestamp_ns"`.

## [0.1.0]

Initial prototype: shared `Message` / `Config` types backing a single-broker TCP listener that wrote bincode-decoded messages to one Arrow IPC file. No persistence guarantees on shutdown, no offset model, no partitioning, no object-store tier. Replaced wholesale by 0.2.0.
