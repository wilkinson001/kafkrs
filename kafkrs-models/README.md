# kafkrs-models

Shared types used by `kafkrs-server` and by any future Rust client. The generated wire-protocol bindings live here too. See the [root README](../README.md) for the project overview.

## What's in here

- `proto/wire/v1.proto` — canonical wire schema. The `Command` envelope, every RPC request/response message, the `ErrorCode` enum, and the reserved field-number ranges for v1.5+ RPCs.
- `src/record.rs` — `Record` envelope (offset, timestamp_ns, schema_id, key, value); same shape on the WAL and on the wire.
- `src/wal.rs` — length-prefixed, CRC32C-validated WAL codec (`encode_record`, `decode_record`, `scan_wal`).
- `src/manifest.rs` — per-partition segment index (`Manifest`, `SegmentEntry`). JSON-serialised, lives in the object store, binary-searched on read.
- `src/topic.rs` — `TopicEntry`, `TopicConfigOverrides`, `ResolvedTopicConfig`, per-disk default profiles.
- `src/config.rs` — broker + object-store config (`Config`, `BrokerConfig`, `ObjectStoreConfig`, `DiskType`).
- `src/wire.rs` — re-exports the `prost`-generated `wire::v1::*` types.

## How the wire bindings are built

`build.rs` invokes `prost-build` at compile time against `proto/wire/v1.proto`. The generated code lands at `target/<profile>/build/kafkrs-models-*/out/kafkrs.wire.v1.rs` and is included into `src/wire.rs` via `include!`.

`protoc-bin-vendored` is a build-dependency, so no system `protoc` is required.

The Python protobuf bindings used by `kafkrs-python` are generated from this same `.proto` but live in that crate (checked-in at `kafkrs-python/kafkrs/wire/v1_pb2.py`); they aren't produced by this crate's build script.

## Tests

```bash
cargo test -p kafkrs-models
```

This covers:

- Unit tests for the WAL codec, manifest segment lookup, topic config resolution, and config TOML parsing.
- `tests/wire_compile.rs` — smoke test that the generated wire types are reachable and that `ErrorCode` enum values match the spec.

## Linting the proto schema

From the workspace root:

```bash
buf lint
```

Configured by `buf.yaml`. Run before pushing any change to `proto/wire/v1.proto` — the proto's evolution rules (additive-only within a major version, reserved field numbers must never be reused) are documented in the wire-protocol spec and enforced in spirit by this linter.

## Versioning rules

Within a `protocol_version`, only additive changes are allowed: new optional fields, new oneof variants, new enum values. Anything else (renumbering, type changes, removed RPCs, frame-format changes) requires a coordinated version bump. The full rules and what they imply for CI are in the wire-protocol spec.

## Design docs

- [`docs/superpowers/specs/2026-05-18-storage-model-design.md`](../docs/superpowers/specs/2026-05-18-storage-model-design.md) — WAL format, manifest schema, durability invariants.
- [`docs/superpowers/specs/2026-05-20-wire-protocol-design.md`](../docs/superpowers/specs/2026-05-20-wire-protocol-design.md) — wire schema and the versioning rules referenced above.
