# Changelog — `kafkrs-server`

All notable changes to this crate are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The three crates in this workspace (`kafkrs-models`, `kafkrs-server`, `kafkrs-python`) are versioned in lockstep.

## [0.6.1] — 2026-09-22

Additive release: broker admin endpoints (`/health` liveness + `/ready` readiness) served on the existing `ports.metrics` admin port.

### Added
- `GET /health` — always returns `200 OK` + `ok` while the broker process is running. Suitable for Kubernetes liveness probes and load-balancer health checks.
- `GET /ready` — returns `503 Service Unavailable` during startup and flips to `200 OK` once wire listeners are bound (`main.rs` calls `metrics::set_ready(true)` at that point). Suitable for Kubernetes readiness probes.
- `pub fn metrics::set_ready(bool)` for callers that want to gate readiness on additional conditions.

### Changed
- The admin HTTP endpoint is now hand-rolled inside `metrics::init` instead of relying on `PrometheusBuilder::with_http_listener`. `install_recorder()` returns a `PrometheusHandle` whose `.render()` produces the exposition text on demand; the same listener serves `/metrics`, `/health`, and `/ready`.

## [0.6.0] — 2026-09-22

DeleteTopic: mark-and-sweep semantics with fast client response, restart-safe pending state, and topic UUIDs baked into object-store prefixes so `Delete + Create` under the same name is race-free. See `docs/superpowers/specs/2026-09-22-delete-topic-design.md`.

**Behaviour change:** `DeleteTopic` is a new admin RPC. By default (`delete_data = true`), a background sweep task cleans up WAL files and object-store data after the client's fast response. `delete_data = false` gives "detach" semantics — actors torn down, storage left alone.

**Breaking on-disk format:** every object-store key gains `v=<uuid>/` after the topic name. `topics.json` schema changes. Operators upgrading from 0.5.0 must delete `data_dir` and the object-store bucket before starting 0.6.0. See kafkrs-models 0.6.0 for details.

### Added
- `deletion` module: `sweep_deletion(record, store, prefix, data_dir)` async fn that walks a snapshot manifest, deletes each segment key, deletes the manifest, and does a one-shot LIST-and-sweep of the topic UUID prefix to catch orphans.
- `pending_deletes` module: durable state file (`data/pending_deletes.json`) with atomic `append`/`remove`/`load_all`. Broker restart replays every unfinished entry.
- `PwMsg::Shutdown { ack: oneshot::Sender<()> }` variant for graceful `PartitionWriter` shutdown.
- `RegistryMsg::Delete { name, delete_data, reply }` variant + `RegistryError::UnknownTopic`.
- `handle_delete_topic` wire dispatch handler orchestrating registry removal + actor shutdown + WAL removal + snapshot + sweep spawn.
- Startup replay of unfinished sweeps in `main.rs`.
- 5 new metric constants under `kafkrs.delete.*`: `pending_topics`, `segments_removed`, `bytes_removed`, `duration_ms`, `errors`. Broker metric count 32 → 37.
- 4 new e2e tests covering: partition removal + subsequent-produce rejection, `delete_data=false` detach semantics, `Delete + Create` uses fresh UUID prefix, pending-delete replay on startup.

### Changed
- `TopicRegistry::snapshot()` returns `Vec<(String, String, u32, ResolvedTopicConfig)>` (name, uuid, partition_count, config). Was `Vec<(String, u32, ResolvedTopicConfig)>`.
- `PartitionHandle` gains `uuid: String`.
- `PartitionWriter::new` and `Uploader::new` gain a `topic_uuid: String` parameter (positioned right after `topic: String`).
- `spawn_partition` gains a `topic_uuid: String` parameter.
- `segment_key` and `manifest_key` gain a `topic_uuid: &str` parameter.

### Not implemented
- Global rate-limiting of concurrent sweeps (deferred; N pending deletes = N tasks, each self-throttled).
- Retry-with-backoff within a single process for failed sweeps (deferred; a failed sweep is deferred until next broker restart).
- Sweep progress query API (metrics cover observability).
- Consumer group offset cleanup (consumer groups don't exist yet).
- Cascade rules and dry-run mode.

## [0.5.0] — 2026-09-21

Broker metrics: a Prometheus scrape endpoint exposing 32 metrics across produce, fetch, uploader, retention, partition state, runtime, and wire subsystems. Semantic conventions follow OpenTelemetry `messaging.*` where applicable; call sites are exporter-agnostic via the `metrics` crate façade so future OTLP push is a config swap. Metric names and label keys are defined as `pub const` items in `kafkrs-server::metrics` so future renames are one-edit changes. See `docs/superpowers/specs/2026-09-21-metrics-design.md`.

**Behaviour change:** none by default. Metrics are off unless operators set `ports.metrics` in `config.toml`. No new port is bound on upgrade unless explicitly enabled.

### Added
- `metrics` module with `init(&PortsConfig, bool)`, `partition_label`, `LATENCY_BUCKETS_MS`, and `describe_all`.
- 32 metrics: `messaging.kafkrs.produce.{records,bytes,latency_ms,errors}`, `messaging.kafkrs.fetch.{requests,records,bytes,latency_ms,long_poll_wait_ms,source,errors}`, `kafkrs.uploader.{segments_uploaded,bytes_uploaded,upload_latency_ms,upload_retries}`, `kafkrs.retention.{segments_evicted,bytes_evicted,passes,delete_failures,sweep_kicks_sent,sweep_kicks_dropped}`, `kafkrs.partition.{count,records_in_memory,bytes_in_memory,segments_uploaded,hwm_offset,wal_files}`, `kafkrs.runtime.{uptime_seconds,build_info}`, `kafkrs.wire.{connections_active,connections_accepted,rpc_requests}`. All names defined as `pub const` items in `kafkrs-server::metrics`.
- Per-partition labels behind `broker.metrics_high_cardinality` opt-in.
- New crate dependencies: `metrics = "0.24"`, `metrics-exporter-prometheus = "0.16"`.
- `KAFKRS_GIT_SHA` compile-time env var support for `kafkrs.runtime.build_info` label (defaults to `"unknown"` when unset).

### Changed
- **BREAKING (config)**: `Config.ports: Vec<u16>` becomes `Config.ports: PortsConfig`. See kafkrs-models 0.5.0 for details.
- `main.rs` calls `metrics::init` before spawning subsystems.
- `Uploader::retention_pass` gains a `trigger: &'static str` parameter used to label `kafkrs.retention.passes`.

### Not implemented
- Native OTLP push exporter (deferred; swap `metrics-exporter-prometheus` for `metrics-exporter-opentelemetry` when needed).
- Distributed traces (`tracing` + `tracing-opentelemetry`; separate spec).
- `/health` endpoint (trivial add on the same admin port when driver appears).
- Native/sparse Prometheus histograms.

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
- Integration test in `tests/wire_e2e.rs`: `retention_evicts_old_segments_via_sweeper`.
- Two Uploader-level tests: `upload_then_retention_evicts_expired`, `retention_kick_evicts_without_upload`.
- Six unit tests in `retention::tests` covering time-based, size-based, either-dimension, tail-never-evicted, and single-segment cases.

### Changed
- `Uploader::new` signature gains `cfg: ResolvedTopicConfig` as the 5th parameter (immediately after `partition`).
- `spawn_partition` clones `utx` before moving it into `PartitionWriter::new` so a clone can be stashed in `PartitionHandle`.

### Not implemented
- Object-store orphan reclamation on partial deletion failure (accepted v1 limitation; documented in the spec).
- Compaction (Kafka's `cleanup.policy=compact`); separate concern for a future spec.

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
