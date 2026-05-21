# kafkrs-server

The broker — both the binary and the library it's built on. See the [root README](../README.md) for the project overview, configuration reference, and an end-to-end quickstart.

## What's in here

Modules in `src/`:

- `wire/` — TCP wire protocol. `frame` (length-prefixed framing + protobuf `Command` codec), `connection` (per-connection three-task state machine: reader / dispatcher / writer), `dispatch` (per-RPC handlers), `errors` (broker-error → wire `ErrorCode` mapping).
- `partition_writer` — per-`(topic, partition)` actor that owns the WAL file, the in-memory active batch, and the offset counter. Producer ack is gated on WAL `fsync` here.
- `uploader` — per-partition actor that turns sealed batches into Parquet objects and updates the partition manifest in the object store.
- `fetcher` — three-tier read resolution (active batch → in-flight upload queue → object store) with long-poll on the partition's tail broadcast.
- `topic_registry` — `./data/topics.json` actor; serves `CreateTopic` / `DescribeTopic` / `ListTopics` and `EnsureExists` (auto-create-on-produce).
- `recovery` — startup reconciliation: WAL replay + manifest cross-check, no object-store LIST.
- `wal_writer` / `segment` / `object_store` — storage primitives.
- `startup::spawn_partition` — partition actor bring-up; used both at boot (in `main.rs`) and by the auto-create path in `wire::dispatch::handle_produce`.

The crate is a `[lib]` + `[[bin]]`, so integration tests in `tests/` can drive the actors directly.

## Building and running

From the workspace root:

```bash
cargo run --bin kafkrs-server -- config.toml
```

The broker reads its config from the supplied TOML file (see the root README for the full schema), logs `Listening on <addr>` per port, and populates `./data` with `topics.json` and per-partition WAL directories.

## Tests

```bash
cargo test -p kafkrs-server
```

This covers:

- **Unit tests** — `cargo test -p kafkrs-server --lib` covers the wire codec, error mapping, and the storage primitives.
- **`tests/storage_e2e.rs`** — drives the partition-actor stack directly (no TCP). Smokes the produce → fsync ack → seal → upload → recovery loop.
- **`tests/wire_e2e.rs`** — drives the broker over a real TCP socket using the actual wire format. Covers the Connect handshake, produce + fetch round-trip with payload bytes, version rejection, and the pre-Connect rejection path.

## Files of interest

- `src/main.rs` — startup: config load → object store + topic registry + per-partition actor bring-up → `wire::accept_loop` per configured port.
- `src/wire/connection.rs` — the per-connection state machine and three-task model. The most spec-shaped file.
- `src/partition_writer.rs` — the durability boundary. Group-commit logic, WAL fsync, in-memory active batch, in-flight upload queue.

## Design docs

- [`docs/superpowers/specs/2026-05-18-storage-model-design.md`](../docs/superpowers/specs/2026-05-18-storage-model-design.md) — WAL format, segment layout, object-store keys, per-partition actor model, durability and visibility invariants.
- [`docs/superpowers/specs/2026-05-20-wire-protocol-design.md`](../docs/superpowers/specs/2026-05-20-wire-protocol-design.md) — frame format, protobuf schema, Connect handshake, three-task connection model, versioning rules.
