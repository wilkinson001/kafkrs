# Kafkrs

A Rust implementation of a Kafka-like streaming platform. Single-broker today, with a per-partition actor model, a WAL + Parquet-on-object-store storage tier, and a versioned protobuf wire protocol.

## Status

Current release is **0.3.0** across all three crates (versioned in lockstep). Single-broker only — the wire protocol reserves field-number ranges for v1.5+ streaming-consumer and admin RPCs. The on-disk format and the wire format are both stable within a major version per the design specs; cross-version migration is not supported.

## Components

### `kafkrs-server`

The broker binary plus the library it's built on. It accepts TCP connections on the configured ports, terminates the wire protocol, and routes Produce/Fetch traffic to per-partition actors. Internal modules:

- `wire` — frame codec, per-connection three-task state machine (reader / dispatcher / writer), and RPC dispatch for the protobuf `Command` envelope.
- `partition_writer` — per-`(topic, partition)` actor owning the WAL file, the in-memory active batch (Arrow column builders), and the offset counter.
- `uploader` — per-partition actor that turns sealed batches into Parquet objects and updates the partition manifest.
- `fetcher` — three-tier read resolution (active batch → in-flight upload queue → object store) with long-poll on the partition's tail broadcast.
- `topic_registry` — `./data/topics.json` actor; handles `CreateTopic` / `DescribeTopic` / `ListTopics` and `EnsureExists` (auto-create-on-produce).
- `recovery` — WAL replay on startup and reconciliation against the per-partition manifest.
- `wal_writer` / `segment` / `object_store` — storage primitives (WAL file framing, Parquet segment writer, backend-agnostic object store).
- `startup` — partition actor bring-up; used both at boot and by the auto-create path.

Producer ack is gated on WAL `fsync` only; object-store upload is asynchronous. A crash between WAL fsync and manifest update never loses acked records — startup recovery replays the WAL and re-queues sealed-but-not-uploaded segments. Consumer visibility advances at the same instant as producer ack.

### `kafkrs-models`

Shared types used by `kafkrs-server` and by any future Rust client. Generated wire-protocol bindings live here too.

- `record` — the WAL / wire envelope (`offset`, `timestamp_ns`, `schema_id`, `key`, `value`).
- `config` / `topic` — broker and per-topic configuration (`BrokerConfig`, `ObjectStoreConfig`, `DiskType`, `TopicConfigOverrides`, `ResolvedTopicConfig`).
- `manifest` — per-partition segment index (`Manifest`, `SegmentEntry`); JSON-serialised, lives in the object store.
- `wal` — length-prefixed, CRC32C-validated WAL codec (`encode_record`, `decode_record`, `scan_wal`).
- `wire` — `prost`-generated Rust types compiled from `proto/wire/v1.proto` at build time. The `.proto` file is the canonical cross-language contract.

### `kafkrs-python`

A pure-Python async client. Pip-installable, no Rust toolchain needed at install or runtime — the only runtime dependency is `protobuf`. The generated protobuf bindings (`kafkrs/wire/v1_pb2.py`) are checked in, so `protoc` is also not required.

Public API:

- `kafkrs.Client(host, port, client_id="kafkrs-python")` — async single-connection TCP client.
- Methods: `connect` / `close` / `produce` / `fetch` / `create_topic` / `list_topics`, plus `__aenter__` / `__aexit__` for `async with`.
- `fetch` returns a list of `FetchedRecord` dataclasses (`offset`, `timestamp_ns`, `schema_id`, `key`, `value`).
- Broker-side errors raise `kafkrs.client.WireError(code, message)`.

## Getting started

### Configuration

The broker reads its config from a TOML file. The repo ships a working default at the root:

```toml
address = "127.0.0.1"
ports = [5432]
data_dir = "./data"

[broker]
disk_type = "nvme"           # nvme | ssd | rotational
auto_create_topics = false
default_partition_count = 1

[object_store]
backend = "filesystem"       # filesystem | s3
bucket = "kafkrs-data"
prefix = ""
endpoint = ""                # set for non-AWS S3-compatible stores (MinIO, R2, etc.)
region = "us-east-1"
```

`disk_type` tunes the group-commit defaults (window, batch size, record count) per the storage spec. `auto_create_topics` is off by default; flip it to `true` if you want produces to unknown topics to auto-create them with `default_partition_count` partitions.

### Running the broker

From the repo root:

```bash
cargo run --bin kafkrs-server -- config.toml
```

The broker logs a `Listening on <addr>` line per configured port and populates `./data` with `topics.json` and per-partition WAL directories as topics get created.

### Producing and fetching from Python

Install the client into a virtualenv:

```bash
cd kafkrs-python
python3 -m venv .venv && .venv/bin/pip install -e .
```

Then with the broker running:

```python
import asyncio
from kafkrs import Client

async def main():
    async with Client("127.0.0.1", 5432) as c:
        # If auto_create_topics is false, create the topic first:
        # await c.create_topic("demo", partition_count=1)
        base, last = await c.produce("demo", 0, [(b"key", b"value")])
        print(f"produced offsets {base}..{last}")

        records, hwm = await c.fetch("demo", 0, from_offset=0, max_wait_ms=500)
        for r in records:
            print(r.offset, r.key, r.value)

asyncio.run(main())
```

## Design docs

- [`docs/superpowers/specs/2026-05-18-storage-model-design.md`](docs/superpowers/specs/2026-05-18-storage-model-design.md) — storage subsystem (WAL format, Parquet segment layout, object-store keys, per-partition actor model, durability and visibility invariants).
- [`docs/superpowers/specs/2026-05-20-wire-protocol-design.md`](docs/superpowers/specs/2026-05-20-wire-protocol-design.md) — wire protocol (frame format, protobuf schema, Connect handshake, three-task connection model, versioning rules).
- [`docs/superpowers/plans/`](docs/superpowers/plans/) — execution plans for each release.

## Repository layout

```
kafkrs/
├── kafkrs-models/       # shared types + generated wire bindings (prost)
│   └── proto/wire/v1.proto
├── kafkrs-server/       # broker binary + library; per-partition actors
├── kafkrs-python/       # pure-Python async client
├── config.toml          # broker configuration
├── buf.yaml             # proto lint config
└── docs/superpowers/
    ├── specs/           # design docs
    └── plans/           # execution plans
```
