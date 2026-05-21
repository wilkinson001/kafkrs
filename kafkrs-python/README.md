# kafkrs-python

Pure-Python async client for the kafkrs broker. See the [root README](../README.md) for the project overview and a Connect → Produce → Fetch quickstart.

No Rust toolchain is needed at install or runtime — the only runtime dependency is `protobuf`. The generated protobuf bindings (`kafkrs/wire/v1_pb2.py`) are checked in, so `protoc` isn't required either.

## Public API

- `kafkrs.Client(host, port, client_id="kafkrs-python")` — async single-connection TCP client.
- Methods: `connect` / `close` / `produce` / `fetch` / `create_topic` / `list_topics`, plus `__aenter__` / `__aexit__` so `async with Client(...) as c:` works.
- `fetch` returns a list of `kafkrs.client.FetchedRecord` dataclasses (`offset`, `timestamp_ns`, `schema_id`, `key`, `value`).
- Broker-side errors raise `kafkrs.client.WireError(code, message)`.

## Installation

For end users:

```bash
pip install -e .
```

For development (adds `pytest` + `pytest-asyncio`):

```bash
python3 -m venv .venv
.venv/bin/pip install -e ".[dev]"
```

Build backend is `hatchling`; `pyproject.toml` declares `protobuf>=7.0` as the only runtime dependency.

## Tests

```bash
.venv/bin/pytest
```

`tests/test_client.py` is an end-to-end test: it builds `target/debug/kafkrs-server` if missing, spawns it as a subprocess with a temp config, and exercises the full Connect → Produce → Fetch loop plus the unsupported-protocol-version path. Each test gets its own temp `data_dir` and its own broker process.

## Regenerating the protobuf bindings

`kafkrs/wire/v1_pb2.py` is checked in and only needs regenerating when `kafkrs-models/proto/wire/v1.proto` changes.

From the workspace root, the canonical command is:

```bash
buf generate
```

(Reads `buf.gen.yaml`; writes into `kafkrs-python/kafkrs/wire/`.)

At the time the file was first generated, the remote `buf.build/protocolbuffers/python` plugin rejected the `paths=source_relative` option in `buf.gen.yaml`, so `protoc` was used as a fallback:

```bash
protoc --python_out=kafkrs-python/kafkrs \
       --proto_path=kafkrs-models/proto \
       kafkrs-models/proto/wire/v1.proto
```

Either command should land the bindings at `kafkrs-python/kafkrs/wire/v1_pb2.py`. Commit the regenerated file alongside the proto change.

## Design docs

- [`docs/superpowers/specs/2026-05-20-wire-protocol-design.md`](../docs/superpowers/specs/2026-05-20-wire-protocol-design.md) — frame format, RPC schema, Connect handshake, error taxonomy. Everything this client implements.
- [`docs/superpowers/specs/2026-05-18-storage-model-design.md`](../docs/superpowers/specs/2026-05-18-storage-model-design.md) — the broker's storage model. Useful when reasoning about offset semantics and `max_wait_ms` behaviour.
