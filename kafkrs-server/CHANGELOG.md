# Changelog — `kafkrs-server`

All notable changes to this crate are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The three crates in this workspace (`kafkrs-models`, `kafkrs-server`, `kafkrs-python`) are versioned in lockstep.

## [0.3.2] — 2026-05-27

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

## [0.3.1] — 2026-05-24

Four bug fixes from the post-0.3.0 code review. See `docs/superpowers/specs/2026-05-24-tier1-fixes-design.md`.

### Added
- `PartitionHandle` gains a `cfg: ResolvedTopicConfig` field so per-RPC handlers can read per-topic limits without a registry round-trip.
- Per-connection `AbortHandle` map keyed by `correlation_id`. In-flight per-RPC tasks are now aborted when the connection closes.
- New integration tests in `tests/wire_e2e.rs`: `create_topic_then_produce_succeeds`, `oversize_key_returns_err_key_too_large`, `oversize_value_returns_err_record_too_large`, `fetch_max_wait_ms_is_capped`, `broker_stays_responsive_after_disconnect_midpoll`.

### Fixed
- `handle_create_topic` now spawns partition workers after registry success. Previously an explicit `CreateTopic` followed by `Produce` returned `ERR_UNKNOWN_TOPIC`; the auto-create path was unaffected.
- `handle_produce` now enforces per-topic `max_key_size_bytes` and `max_value_size_bytes` against each record's declared sizes, returning `ERR_KEY_TOO_LARGE` (204) or `ERR_RECORD_TOO_LARGE` (203) for oversized records.
- `handle_fetch` now caps `max_wait_ms` at the per-topic `max_fetch_wait_ms`. The cap is silent — the client request is honored up to the limit.
- Per-RPC tasks are now aborted when their connection is torn down, eliminating the up-to-`max_wait_ms` leak of long-poll fetcher tasks.

### Changed
- `handle_produce` restructured: partition handle now resolved before payload slicing so the per-record size check can read `handle.cfg`.

## [0.3.0] — 2026-05-21

Wire protocol v1 lands. See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md` for the design.

### Added
- `kafkrs_server::wire` module — Pulsar-style framing (length-prefixed frames carrying a protobuf `Command` envelope plus a raw payload section), explicit `Connect` handshake, per-connection three-task model (reader / dispatcher / writer) with per-RPC task spawning for in-flight multiplexing, and structured `ErrorCode` taxonomy.
- `kafkrs_server::startup::spawn_partition` — extracted from `main.rs` so the auto-create path can bring up partition workers on first produce.
- `kafkrs-server/tests/wire_e2e.rs` — end-to-end TCP integration test (connect, produce, fetch, unsupported version, pre-Connect rejection).
- Dependencies: `prost`, `tokio-util` (codec feature), `tokio-stream`, `thiserror`.

### Changed
- **Breaking:** `kafkrs_server::listener` is replaced by `kafkrs_server::wire`. The bincode `WireRequest` / `WireResponse` enums are gone; clients now speak the protobuf-framed wire described in the spec.
- **Breaking:** Stringly-typed error responses (`WireResponse::Error(String)` with magic strings) are replaced by `ErrorCode` enum values.
- `SharedState` gains `data_dir` and `disk_type` fields so auto-create can spawn partition actors.
- Auto-create on produce now spawns partition workers after registering the topic (previously the topic was registered but produces returned `ERR_UNKNOWN_TOPIC`).

### Removed
- `kafkrs-server/src/listener.rs` — replaced by the `wire` module.
- `bincode` dependency — no longer used.

## [0.2.0] — 2026-05-20

Storage subsystem rewrite. The single-file Arrow IPC writer is replaced by a per-partition WAL + Parquet-on-object-store model with offset-resumable reads, three-tier read resolution, and an asynchronous uploader. See `docs/superpowers/specs/2026-05-18-storage-model-design.md` for the design rationale and `docs/superpowers/plans/2026-05-19-storage-model.md` for the implementation plan.

**This is a breaking release across all three crates.** No migration path from 0.1.0 data on disk; the WAL format, segment format, and wire envelope are all new.

### Added
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

### Changed
- **Breaking:** Per-partition `PartitionWriter` actors replace the single global `Writer`. Each partition owns its own WAL file and offset counter; cross-actor communication is via `tokio::sync::mpsc` and `broadcast`.
- **Breaking:** Producer ack is now gated on WAL `fsync`, not on Arrow IPC buffering. Consumer visibility advances at the same instant.
- **Breaking:** On-disk format is Parquet segments in an S3-compatible store (or local filesystem for testing), indexed by a per-partition JSON manifest, instead of one sealed Arrow IPC file per process.
- `config::load_config` is now `pub` (was `pub(crate)`) so the integration test target can call it from outside the bin.

### Removed
- **Breaking:** `writer.rs` and the `Writer` struct. The Arrow IPC `FileWriter` is gone; segments are Parquet, the WAL is the durability boundary, and the broken shutdown path that called `arrow_writer.finish()` without flushing the in-memory buffer no longer exists.
- **Breaking:** `arrow-ipc` dependency.

### Fixed
- Accept-loop bug in `main.rs` that called `TcpListener::accept()` exactly once per port and never re-accepted. The new accept loop runs `loop { listener.accept().await }` and spawns a `Listener::process()` task per connection.
- Pre-fsync records are no longer silently dropped on shutdown: they were never acked to the producer, so producers know to retry. Anything that *was* acked is durable on disk and recovered on startup.

## [0.1.0]

Initial prototype: a single-broker TCP listener wrote bincode-decoded `Message`s to one Arrow IPC file via `arrow_ipc::FileWriter`. No persistence guarantees on shutdown, no offset model, no partitioning, no object-store tier. Replaced wholesale by 0.2.0.
