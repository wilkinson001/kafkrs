# Changelog — `kafkrs-python`

All notable changes to this crate are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The three crates in this workspace (`kafkrs-models`, `kafkrs-server`, `kafkrs-python`) are versioned in lockstep.

## [0.3.2] — 2026-05-27

Version bump only — kafkrs-python has no code changes. Stays in lockstep with the broker's 0.3.2 release.

## [0.3.1] — 2026-05-24

Tracks the server 0.3.1 release. See `docs/superpowers/specs/2026-05-24-tier1-fixes-design.md`.

### Changed
- Regenerated `kafkrs/wire/v1_pb2.py` to include the new `max_fetch_wait_ms` field on `TopicConfigOverrides` (proto field 8). Users can now set `overrides.max_fetch_wait_ms = N` when calling `Client.create_topic(...)`.

### Added
- `tests/test_client.py::test_create_topic_then_produce` — end-to-end test for the explicit `CreateTopic` → `Produce` path now that the server bug is fixed.

## [0.3.0] — 2026-05-21

Crate restructured from a PyO3 extension to a pure-Python package. Wire protocol v1 client. See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md`.

### Added
- `kafkrs.Client` — async TCP client speaking wire protocol v1. Methods: `connect`, `close`, `produce`, `fetch`, `create_topic`, `list_topics`. Supports `async with` via `__aenter__`/`__aexit__`. Raises `kafkrs.client.WireError(code, message)` on broker-side errors.
- `kafkrs.wire.v1_pb2` — checked-in protobuf bindings generated from `kafkrs-models/proto/wire/v1.proto`. Users do not need `protoc` installed.
- `tests/test_client.py` — end-to-end test that spawns a real `kafkrs-server` binary and exercises Connect → Produce → Fetch.

### Changed
- **Breaking:** entire surface. The PyO3 `encode_message` function is gone, replaced by a real async client. Python users now `pip install kafkrs` (no Rust toolchain needed) and write `async with kafkrs.Client(host, port) as c: await c.produce(...)`.
- Build backend: `maturin` → `hatchling`.
- `protobuf` dependency pinned to `>=7.0` (matches protoc-generated runtime requirement).

### Removed
- All Rust code (`src/lib.rs`, `build.rs`, `Cargo.toml`).
- Dependencies on `pyo3`, `bincode`, and `kafkrs-models` (Rust). The only runtime dependency is `protobuf`.

## [0.2.0] — 2026-05-20

Storage subsystem rewrite. The single-file Arrow IPC writer is replaced by a per-partition WAL + Parquet-on-object-store model with offset-resumable reads, three-tier read resolution, and an asynchronous uploader. See `docs/superpowers/specs/2026-05-18-storage-model-design.md` for the design rationale and `docs/superpowers/plans/2026-05-19-storage-model.md` for the implementation plan.

**This is a breaking release across all three crates.** No migration path from 0.1.0 data on disk; the WAL format, segment format, and wire envelope are all new.

### Changed
- **Breaking:** `encode_message(key, value, schema, partition)` is now `encode_message(key, value, schema_id, timestamp_ns=0)`.
  - `key`: `str` → `bytes` (`Vec<u8>`).
  - `value`: already `bytes`; serialisation unchanged.
  - `schema: Option<str>` → `schema_id: u32` (producer-assigned tag; `0` means "no schema / opaque").
  - `partition` parameter removed (partition routing is request envelope metadata, not record-level).
  - New `timestamp_ns: i64 = 0` parameter; `0` means "broker-stamps it on arrival".
- Bincode encoding switched from `config::legacy()` to `config::standard()` to match the new server-side framing.
- Now imports `kafkrs_models::record::Record` (was `kafkrs_models::message::Message`).

## [0.1.0]

Initial prototype: Python bindings exposing `encode_message(key, value, schema, partition)` that bincode-encoded a `kafkrs_models::message::Message` for the single-broker TCP listener. Replaced wholesale by 0.2.0.
