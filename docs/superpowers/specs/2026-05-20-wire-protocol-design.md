# Wire Protocol Design — kafkrs v1

**Status:** Draft for review
**Date:** 2026-05-20
**Scope:** TCP wire protocol between clients and the kafkrs broker. Storage subsystem, transform layer, multi-broker replication, and schema registry are out of scope and covered by their own designs.

## Motivation

The 0.2.0 storage rewrite (see `2026-05-18-storage-model-design.md`) intentionally deferred the wire protocol. As a placeholder, `kafkrs-server` framed messages as a 4-byte LE length + bincode-encoded `WireRequest` enum, and `kafkrs-python` was left bincode-encoding raw `Record` envelopes. The result:

- `kafkrs-python` is wire-incompatible with the 0.2.0 broker — it ships a `Record` where the server expects a `WireRequest::Produce { topic, partition, ... }` envelope.
- Bincode is not a cross-language specification. Any non-Rust client would need to reverse-engineer `serde-bincode`'s output for a specific Rust version.
- `kafkrs-python` directly imports `kafkrs_models::record::Record`, locking the two crates to lockstep releases and silent wire breaks (the 0.2.0 changelog records `config::legacy()` → `config::standard()` as if it were a non-event; it was a wire break).
- Every future RPC (consumer groups, retention admin, auth, multi-broker negotiation) would accrete onto an ad-hoc protocol with no versioning, no correlation IDs, and a stringly-typed error variant (`WireResponse::Error(String)` with magic strings like `"UnknownTopic"`).

This spec defines a wire protocol that:

- carries a language-neutral contract via a checked-in `.proto` schema,
- decouples client crates from `kafkrs-models` — the schema is the only shared artifact,
- supports additive evolution (new RPCs, new fields) without version bumps,
- defines a structured error taxonomy in place of magic strings,
- preserves the throughput characteristics of length-prefixed binary framing on the produce/fetch hot path.

## Design choices, with rationale

### Custom TCP framing + protobuf control envelope (Pulsar-style)

Three transports were on the table:

- **gRPC** (HTTP/2 + protobuf + tonic). Gives streaming RPCs, deadlines, cancellation, multiplexing, and a mature observability ecosystem for free. Pays in dependency weight, HTTP/2 framing overhead, and a memory copy per `bytes` field in `prost`-generated decode.
- **Kafka-style hand-rolled binary**. Maximum control, smallest dependency footprint. Reinvents protobuf's evolution rules by hand; only justified by Kafka-wire compatibility, which is not a kafkrs goal.
- **Pulsar-style: custom TCP framing carrying a protobuf control envelope, with raw payload bytes appended after the protobuf.** Chosen.

The Pulsar pattern keeps the per-record hot path zero-copy (payload bytes are sliced from the receive buffer, never decoded), uses protobuf for the control plane where its evolution rules and codegen pay off, and stays close to the existing length-prefixed framing already in `kafkrs-server`. The Pulsar precedent matters specifically because they faced the same tradeoff — wanting protobuf's IDL benefits without HTTP/2 framing overhead on the data path — and shipped it at scale.

### Protobuf for the RPC envelope, Avro reserved for record payloads

Two nested schema layers, owned by different parties:

- **Protobuf** is *kafkrs's* schema. It defines the `Command` envelope (`Produce`, `Fetch`, `CreateTopic`, …). Changed by us when we add RPCs. One generated artifact per client language.
- **Avro** is the *user's* schema for the bytes inside `value`. v1 treats every payload as opaque (`schema_id = 0`) or as a producer-meaningful tag (`schema_id > 0`). When the schema registry lands (separate brainstorm), `schema_id` will resolve to an Avro schema; the broker will decode payloads at segment-upload time and emit them as native Parquet columns (the "schema-aware Parquet" path deferred by the storage spec). **This change will not touch the wire protocol** — it's a storage-layer addition.

The split mirrors Confluent's Kafka stack (custom RPC envelope + Avro payloads + Schema Registry), with protobuf in place of their hand-rolled envelope so we inherit `prost`'s mature Rust tooling.

### Why a Connect handshake (instead of version-per-request)

Three negotiation strategies were considered: no negotiation (rely entirely on protobuf field-number evolution), per-request version (every `Command` carries `protocol_version`), or an explicit `Connect` handshake. The handshake is chosen because:

- A broker can reject an incompatible client *before* any RPC, with a clear `ERR_UNSUPPORTED_PROTOCOL_VERSION` instead of a silent decode failure.
- It is the natural slot for future authentication (`auth_data: bytes` field reserved on `ConnectRequest` in v1).
- It is the natural slot for future feature negotiation (compression, max frame size).
- Per-request version fields are wasted bytes on every RPC.

The handshake is otherwise minimal: one `Connect` frame, one `Connected` reply, no further setup before RPCs flow.

### Long-poll Fetch in v1; flow-controlled push reserved for v1.5+

Two consumer models were considered: Kafka-style long-poll `Fetch` (the storage spec's current assumption) and Pulsar-style `Subscribe` + `Flow` + push. v1 ships long-poll because:

- The storage spec already implements long-poll via the partition's `tokio::sync::broadcast` tail and `max_wait_ms`. Reusing the existing seam is the smallest delta.
- The Pulsar transport choice does *not* force the Pulsar consumer model. Both can coexist.
- Field numbers `40–49` in `Command.body` are reserved now for `Subscribe` / `Subscribed` / `Flow` / `Message` / `Ack` / `Unsubscribe`, so adding the flow-controlled model later is an additive within-version change.

## Architecture

```
client ──TCP──▶  ┌─────────────────────────────────────────────────────────────────┐
                 │ kafkrs broker                                                   │
                 │                                                                 │
                 │  ┌───────────┐   Command   ┌─────────────┐                      │
                 │  │ Reader    │────────────▶│ Dispatcher  │                      │
                 │  │  (codec,  │   via mpsc  │  (per-conn  │                      │
                 │  │   decode) │             │   state +   │                      │
                 │  └───────────┘             │   per-RPC   │                      │
                 │                            │   spawn)    │                      │
                 │  ┌───────────┐   Command   └──────┬──────┘                      │
                 │  │ Writer    │◀────────────       │                             │
                 │  │  (encode, │   via shared       │ pw_tx / registry / store    │
                 │  │   send)   │   mpsc             ▼                             │
                 │  └───────────┘             ┌─────────────────────────────────┐  │
                 │       ▲                    │ partition_writer, fetcher,      │  │
                 │       └────────────────────│ topic_registry, uploader, ...   │  │
                 │                            │ (unchanged from 0.2.0)          │  │
                 │                            └─────────────────────────────────┘  │
                 └─────────────────────────────────────────────────────────────────┘
```

Each accepted TCP connection spawns three tokio tasks (reader, dispatcher, writer) communicating through mpsc channels. The dispatcher owns the connection state machine and spawns per-RPC tasks so that a long-polling `Fetch` does not block subsequent `Produce` requests on the same connection. The single writer task serializes all outbound frames, guaranteeing responses for different `correlation_id`s never interleave bytes on the socket. This replaces the sequential read→handle→write loop in the current `Listener::process` (`kafkrs-server/src/listener.rs:71`).

### Invariants

- The first frame on every connection MUST be a `Connect` command. Anything else returns `ERR_HANDSHAKE_REQUIRED` and closes the connection.
- Every response Command echoes the request's `correlation_id`. Clients correlate by it; broker does not infer order.
- The outer frame format (two `u32` length prefixes, big-endian) is part of the protocol and cannot change within a version.
- Within a `protocol_version`, evolution is purely additive: new optional fields, new oneof variants, new enum values. Anything else is a version bump.
- The `Payload` section is raw, never decoded by the protobuf layer. The broker slices it using the `RecordMeta` lengths in the `Command`.
- Connection-level errors (handshake failures, framing failures) close the connection. Per-request errors (`UnknownTopic`, `OffsetOutOfRange`) do not.

## Frame format

Every kafkrs message is one frame:

```
┌──────────────────┬──────────────────┬─────────────────────────┬──────────────────────┐
│  total_size (4B) │ command_size (4B)│  Command (protobuf)     │ Payload (raw bytes)  │
└──────────────────┴──────────────────┴─────────────────────────┴──────────────────────┘
                                       ◀──── command_size B ───▶ ◀── payload_size B ──▶
                   ◀────────────────── total_size bytes ─────────────────────────────────▶
```

| field | size | notes |
| --- | --- | --- |
| `total_size` | 4 B, big-endian u32 | bytes from `command_size` through end of `Payload`, **excluding** these 4 bytes |
| `command_size` | 4 B, big-endian u32 | length of the protobuf `Command` block |
| `Command` | `command_size` B | protobuf-encoded `kafkrs.wire.v1.Command` |
| `Payload` | `total_size − 4 − command_size` B | optional; raw bytes (record keys + values concatenated). Present only for `Produce`, `FetchResponse`, and the future `Message` command. Zero-length for everything else. |

**Endianness:** all multi-byte integers on the wire are big-endian (network byte order). The storage WAL stays little-endian (different domain — local disk).

**Max frame size:** 4 MiB total in v1. Frames exceeding it return `ERR_FRAME_TOO_LARGE` and the connection is closed. Tunable later via Connect feature negotiation.

### Payload section layout

For commands carrying records, the protobuf `Command` includes a `repeated RecordMeta` listing each record's `key_len`, `value_len`, `schema_id`, and `timestamp_ns`. The `Payload` section is the concatenation `key₁ ‖ value₁ ‖ key₂ ‖ value₂ ‖ …`. The reader walks the payload by stepping through the metas — no decode of key/value bytes, just slices into the receive buffer.

```
Command: ProduceRequest {
  topic: "orders", partition: 3,
  records: [
    InRecordMeta { key_len: 4,  value_len: 12, schema_id: 0, timestamp_ns: 0 },
    InRecordMeta { key_len: 0,  value_len: 8,  schema_id: 0, timestamp_ns: 0 },
  ]
}

Payload (24 bytes):
[key₁: 4B][value₁: 12B][value₂: 8B]   ← key₂ omitted (key_len = 0)
```

### Worked example: small Produce, byte-by-byte

A single 8-byte-key, 16-byte-value Produce to `("logs", 0)`:

```
00 00 00 4E   ← total_size = 78
00 00 00 32   ← command_size = 50
[50 bytes: protobuf Command containing ProduceRequest with 1 InRecordMeta]
[ 8 bytes: key]
[16 bytes: value]
```

`total_size = 78 = 4 (command_size field) + 50 (command) + 8 (key) + 16 (value)`. The receiver reads 4 bytes, learns the total, reads 78 more, slices `command_size`/command/payload, decodes the small protobuf, slices the payload by the metas.

### Why two length fields, not one

A single `total_size` is enough to consume a frame. The separate `command_size` lets the reader work in two stages: read a small fixed prefix → know the protobuf bounds → decode the small protobuf → read the `RecordMeta`s → slice the payload. Without the second length, you'd have to either inline payload lengths into the protobuf (which couples decode order) or decode the full protobuf to know where it ends.

### Streaming / partial reads

The reader uses `tokio_util::codec::LengthDelimitedCodec` configured for a u32 big-endian outer prefix. Stdlib-strength code; the only hand-written byte work is the inner `command_size` + payload slicing. The 0.2.0 listener's `read_to_end`-per-message bug is structurally impossible with this codec.

## Protobuf schema

Single `.proto` file, lives at `kafkrs-models/proto/wire/v1.proto`. Generated into Rust via `prost-build` at compile time; generated into Python via checked-in `*_pb2.py` files (so consumers don't need `protoc`).

```proto
syntax = "proto3";
package kafkrs.wire.v1;

message Command {
  // Reserved for v1.5+ streaming-consumer RPCs:
  //   Subscribe / Subscribed / Flow / Message / Ack / Unsubscribe
  reserved 40 to 49;
  // Reserved for v1.5+ admin RPCs:
  //   DeleteTopic / DeleteTopicResp / AlterConfig / AlterConfigResp / BrokerInfo
  reserved 50 to 59;

  uint64 correlation_id = 1;

  oneof body {
    // Connection lifecycle (10–19)
    ConnectRequest         connect               = 10;
    ConnectedResponse      connected             = 11;
    PingRequest            ping                  = 12;
    PongResponse           pong                  = 13;

    // Data plane (20–29)
    ProduceRequest         produce               = 20;
    ProduceResponse        produce_resp          = 21;
    FetchRequest           fetch                 = 22;
    FetchResponse          fetch_resp            = 23;

    // Control plane (30–39)
    CreateTopicRequest     create_topic          = 30;
    CreateTopicResponse    create_topic_resp     = 31;
    DescribeTopicRequest   describe_topic        = 32;
    DescribeTopicResponse  describe_topic_resp   = 33;
    ListTopicsRequest      list_topics           = 34;
    ListTopicsResponse     list_topics_resp      = 35;

    // Errors
    ErrorResponse          error                 = 99;
  }
}

// ---- Connection lifecycle ----

message ConnectRequest {
  uint32 protocol_version = 1;   // v1 = 1
  string client_id        = 2;   // free-form, for logging
  bytes  auth_data        = 3;   // reserved; v1 ignores contents
}

message ConnectedResponse {
  uint32 protocol_version = 1;   // server's chosen version
  string broker_id        = 2;
}

message PingRequest  {}
message PongResponse {}

// ---- Data plane ----

message InRecordMeta {
  uint32 key_len      = 1;   // 0 = no key
  uint32 value_len    = 2;
  uint32 schema_id    = 3;   // 0 = opaque (matches storage spec)
  int64  timestamp_ns = 4;   // 0 = broker stamps on arrival
}

message ProduceRequest {
  string topic                  = 1;
  uint32 partition              = 2;
  repeated InRecordMeta records = 3;   // payload bytes follow Command: k₁‖v₁‖k₂‖v₂‖…
}

message ProduceResponse {
  int64 base_offset = 1;   // offset assigned to records[0]
  int64 last_offset = 2;   // offset assigned to records[N-1]
  int64 hwm         = 3;   // partition HWM after this produce
}

message OutRecordMeta {
  int64  offset       = 1;
  int64  timestamp_ns = 2;
  uint32 schema_id    = 3;
  uint32 key_len      = 4;
  uint32 value_len    = 5;
}

message FetchRequest {
  string topic        = 1;
  uint32 partition    = 2;
  int64  from_offset  = 3;
  uint32 max_records  = 4;
  uint32 max_wait_ms  = 5;   // 0 = no long-poll
}

message FetchResponse {
  repeated OutRecordMeta records = 1;   // bytes in payload section
  int64    hwm                   = 2;
}

// ---- Control plane ----

message TopicConfigOverrides {
  optional uint64 segment_size_bytes        = 1;
  optional uint64 segment_seal_time_ms      = 2;
  optional uint32 max_key_size_bytes        = 3;
  optional uint32 max_value_size_bytes      = 4;
  optional uint64 group_commit_time_ms      = 5;
  optional uint64 group_commit_size_bytes   = 6;
  optional uint32 group_commit_record_count = 7;
}

message CreateTopicRequest {
  string topic                   = 1;
  uint32 partition_count         = 2;
  TopicConfigOverrides overrides = 3;
}
message CreateTopicResponse {}   // success implicit

message DescribeTopicRequest  { string topic = 1; }
message DescribeTopicResponse {
  string topic                = 1;
  uint32 partition_count      = 2;
  int64  created_at_ns        = 3;
  TopicConfigOverrides config = 4;   // only overridden fields are present
}

message ListTopicsRequest  {}
message ListTopicsResponse { repeated string topics = 1; }

// ---- Errors ----

message ErrorResponse {
  ErrorCode code    = 1;
  string    message = 2;   // human-readable, may be empty
}

enum ErrorCode {
  ERR_UNSPECIFIED                  = 0;

  // Connection-level (1xx)
  ERR_UNSUPPORTED_PROTOCOL_VERSION = 100;
  ERR_HANDSHAKE_REQUIRED           = 101;
  ERR_ALREADY_CONNECTED            = 102;
  ERR_AUTH_FAILED                  = 103;
  ERR_MALFORMED_FRAME              = 104;
  ERR_FRAME_TOO_LARGE              = 105;
  ERR_INVALID_COMMAND              = 106;

  // Data plane (2xx)
  ERR_UNKNOWN_TOPIC                = 200;
  ERR_UNKNOWN_PARTITION            = 201;
  ERR_OFFSET_OUT_OF_RANGE          = 202;
  ERR_RECORD_TOO_LARGE             = 203;
  ERR_KEY_TOO_LARGE                = 204;
  ERR_BROKER_NOT_READY             = 205;

  // Control plane (3xx)
  ERR_TOPIC_ALREADY_EXISTS         = 300;
  ERR_INVALID_TOPIC_NAME           = 301;
  ERR_INVALID_PARTITION_COUNT      = 302;

  // Internal (9xx)
  ERR_INTERNAL                     = 900;
}
```

### Field-numbering convention

- `1–9`: reserved for top-level `Command` metadata (`correlation_id` today; future `trace_context`, `feature_flags`).
- `10–19`: connection lifecycle RPC pairs.
- `20–29`: data plane RPC pairs.
- `30–39`: control plane RPC pairs.
- `40–49`: reserved for the v1.5+ streaming-consumer RPCs.
- `50–59`: reserved for the v1.5+ admin RPCs.
- `99`: `ErrorResponse`.

### Why two `RecordMeta` types

Producer-side and broker-side fields are genuinely different: producers don't know offsets (the broker assigns them); consumers need offsets returned. Unifying via optional fields would be ambiguous (`offset = 0` could mean "broker, assign me one" or "the broker assigned offset 0"). Two types keeps each direction's contract precise.

## Connection lifecycle

### State machine

```
       ┌──────────────────┐
       │ TCP accepted     │
       └────────┬─────────┘
                │
                ▼
       ┌──────────────────────────┐         any frame ≠ Connect
       │ AWAITING_CONNECT         │────────────────────────────▶ ErrorResponse(HANDSHAKE_REQUIRED) → close
       └────────┬─────────────────┘
                │ Connect received
                ▼
       ┌──────────────────────────┐         version unsupported
       │ verify protocol_version  │────────────────────────────▶ ErrorResponse(UNSUPPORTED_PROTOCOL_VERSION) → close
       └────────┬─────────────────┘
                │ version ok
                ▼
       ┌──────────────────────────┐
       │ ConnectedResponse sent   │
       └────────┬─────────────────┘
                │
                ▼
       ┌──────────────────────────┐         second Connect          → ErrorResponse(ALREADY_CONNECTED) → close
       │ READY                    │         malformed frame         → ErrorResponse(MALFORMED_FRAME) → close
       │   (handle RPCs,          │         frame > max_frame_size  → ErrorResponse(FRAME_TOO_LARGE) → close
       │    multiplexed by        │         unknown oneof variant   → ErrorResponse(INVALID_COMMAND) + connection survives
       │    correlation_id)       │         peer EOF                → close
       └──────────────────────────┘
```

Connection-level errors close the connection because they indicate the peer doesn't speak the protocol. `ERR_INVALID_COMMAND` is per-request — a future-version client may send an RPC the broker doesn't know, and we want it to recover by trying a different RPC, not lose the whole connection.

**`correlation_id` on connection-level errors.** Errors raised before a request's `correlation_id` can be safely decoded (`ERR_MALFORMED_FRAME`, `ERR_FRAME_TOO_LARGE`) MUST be sent in a `Command` with `correlation_id = 0`. Errors raised after a `correlation_id` was successfully read (`ERR_HANDSHAKE_REQUIRED`, `ERR_UNSUPPORTED_PROTOCOL_VERSION`, `ERR_ALREADY_CONNECTED`, `ERR_INVALID_COMMAND`) MUST echo the request's `correlation_id`. Clients SHOULD treat `correlation_id = 0` on an `ErrorResponse` as a connection-fatal error not associated with any in-flight RPC.

### Per-connection concurrency

Each accepted connection spawns three tokio tasks:

```
       ┌─────────────────┐  Command   ┌──────────────────┐
       │ inbound reader  │───────────▶│ dispatch handler │
       │ (decode frames) │  via mpsc  │ (spawn per-RPC   │
       └─────────────────┘            │  task)           │
                                       └────────┬─────────┘
                                                │ Command (response)
                                                │ via shared mpsc
                                                ▼
                                       ┌──────────────────┐
                                       │ outbound writer  │
                                       │ (encode frames,  │
                                       │  serialize sends)│
                                       └──────────────────┘
```

- **Reader.** Reads frames using `LengthDelimitedCodec`. Decodes the protobuf `Command`. Forwards to the dispatcher. Single read at a time per connection; backpressure is the dispatcher's mpsc buffer.
- **Dispatcher.** Owns the connection state machine. In `READY`, spawns a per-RPC task for each inbound Command. Hands the spawned task an `mpsc::Sender<Command>` (the writer's input) plus the `correlation_id` to echo.
- **Writer.** Owns the socket write half. Receives `Command` responses on its mpsc, encodes one frame, writes. Serializing through one task guarantees responses never interleave bytes on the socket.

This is why `correlation_id` exists: multiple in-flight RPCs on one connection complete out of order, the client correlates by id. A client can fire a `Fetch` with `max_wait_ms=5000` and still send `Produce`s on the same connection during the wait.

### Disconnect handling

When the reader sees EOF or any unrecoverable error: it signals the dispatcher to stop accepting new RPCs. In-flight long-poll `Fetch`es detect their reply channel closing and cancel cleanly. `Produce` RPCs already submitted to `partition_writer` complete normally — the WAL fsync still happens; the producer just never sees the ack, which is fine because it'll retry. No silent record loss.

## Versioning rules

These rules MUST be enforced by CI (via `buf lint` + `buf breaking` against `main`) once a CI pipeline exists, and SHOULD be linked from `CONTRIBUTING.md` for human review in the meantime.

### Version identifier

`protocol_version` is a single `uint32`. v1 = `1`. Carried in `ConnectRequest` and echoed in `ConnectedResponse`. There is no per-RPC version field.

### Single-version brokers

Each broker build supports **exactly one** `protocol_version`. Client requests `v1`, server accepts iff it's also `v1`, otherwise rejects with `ERR_UNSUPPORTED_PROTOCOL_VERSION` and closes. Multi-version support is a forward-compatible extension; v1 doesn't need it.

### Changes that are safe within a version

These are additive and may be made without bumping `protocol_version`:

| Change | Notes |
| --- | --- |
| Add a new `optional` field to a message | Old clients ignore it. |
| Add a new RPC variant in the `Command` oneof | Older counterparties reply `ERR_INVALID_COMMAND`. Connection survives. |
| Add a new `ErrorCode` enum value | Clients MUST treat unknown error codes as transient errors. |
| Add a new value to any other enum | Clients MUST tolerate unknown values. |
| Mark a field deprecated (still serialized) | Use the `[deprecated = true]` annotation. |

### Changes that REQUIRE a version bump

These are silent or unrecoverable breaks and MUST trigger a coordinated version bump:

| Change | Why |
| --- | --- |
| Remove or renumber a field | Breaks decode for clients still sending it. |
| Change a field's type | Silent wire-format change. |
| Remove an RPC variant from the `Command` oneof | Existing clients sending it would get `ERR_INVALID_COMMAND` forever. |
| Change the semantics of an existing field | Silent semantic break. |
| Change the outer frame format (`total_size` / `command_size` prefixes) | The framing predates protobuf decode; clients cannot recover. |
| Change endianness | Same. |

A version bump is a planned breaking event with a documented migration path. Old clients see `ERR_UNSUPPORTED_PROTOCOL_VERSION` at Connect — clear, structured, not a silent corruption.

### Protobuf hygiene rules

- Every removed field number MUST be added to a `reserved` list in its message so it can never be reused.
- Field numbers in the `Command` oneof MUST never be renumbered, even when reorganizing the `.proto` file.
- `buf lint` and `buf breaking` MUST be run against `main` in CI from day one of the pipeline existing.
- The `Command.reserved 40 to 49, 50 to 59` lines are load-bearing for the v1.5 roadmap; do not remove them without first removing the deferred RPC plans.

## Impact on existing code

### Files replaced

| Location | Change |
| --- | --- |
| `kafkrs-server/src/listener.rs` | Full rewrite. Sequential read→handle→write loop replaced by reader/dispatcher/writer three-task model with `LengthDelimitedCodec` and prost-decoded `Command`. The `WireRequest` / `WireResponse` enums (`listener.rs:30-59`) are deleted; the generated `kafkrs_models::wire::v1::Command` replaces them. The magic-string error handling (`listener.rs:130`, `listener.rs:170`) is replaced by `ErrorCode`. |
| `kafkrs-python/src/lib.rs` and the PyO3 build setup | Deleted. `kafkrs-python` is restructured as a pure-Python package built on the standard `protobuf` runtime. Proves the protocol is genuinely language-neutral; removes the welding to `kafkrs-models`. |

### Files added

| Location | Purpose |
| --- | --- |
| `kafkrs-models/proto/wire/v1.proto` | The canonical protobuf schema. |
| `kafkrs-models/build.rs` | Runs `prost_build` on the `.proto` at compile time. |
| `kafkrs-models/src/wire.rs` | Re-exports the generated types under a stable path. |
| `kafkrs-python/kafkrs/wire/v1_pb2.py` | Checked-in Python protobuf bindings (so users do not need `protoc`). |
| `kafkrs-python/kafkrs/client.py` | Async Python client: connection management, Connect handshake, correlation_id multiplexing, produce/fetch surface. |
| `kafkrs-server/tests/wire_e2e.rs` | End-to-end integration test driving the broker over a real TCP socket using the actual wire format. |
| `buf.yaml`, `buf.gen.yaml` (workspace root) | Schema linting + breaking-change detection. |

### Dependencies added

- `kafkrs-models`: `prost` (runtime), `prost-build` (build), `prost-types`.
- `kafkrs-server`: `tokio-util` with the `codec` feature as a direct dependency.
- `kafkrs-python`: `protobuf` (pure-Python runtime).

### Dependencies removed

- `kafkrs-server`: `bincode` (no longer used by the wire; the on-disk WAL codec in `kafkrs-models/src/wal.rs` is hand-rolled binary, not bincode).
- `kafkrs-python`: `bincode`, `pyo3`, `kafkrs-models` — the Python crate becomes entirely Rust-free.

### What stays untouched

- `kafkrs-models::record::Record`, `kafkrs-models::wal`, `kafkrs-models::manifest` — the on-disk format is unaffected.
- `kafkrs-server::partition_writer`, `uploader`, `recovery`, `fetcher`, `topic_registry` — the actor architecture is unchanged. Only the listener fronting them is rewritten.
- `config.toml`, `BrokerConfig`, `ObjectStoreConfig` — no new config required by the wire layer in v1.

### Rollout

Single coordinated landing. There are no users to migrate; the only client is `kafkrs-python` and it is being rewritten in the same change. All three crates bump to 0.3.0 in lockstep.

## Out of scope

### Reserved for additive evolution within v1 (no version bump)

- **Streaming-consumer RPCs.** Field numbers `40–49` reserved for `Subscribe` / `Subscribed` / `Flow` / `Message` / `Ack` / `Unsubscribe`. Pulsar-style flow-controlled push. Added when long-poll `Fetch` hits observed limits.
- **Admin RPCs.** Field numbers `50–59` reserved for `DeleteTopic`, `AlterTopicConfig`, and at least one diagnostic RPC. `DeleteTopic` deferral mirrors the storage spec.
- **Compression negotiation.** A future `feature_flags` field on `ConnectRequest` plus a per-message compression marker. zstd is the likely choice (matches the segment writer).
- **Larger `max_frame_size`.** Today's 4 MiB cap is hard-coded; future versions can negotiate up via Connect.
- **`ErrorCode` additions.** Documented as additive.

### Out of scope until a separate brainstorm

- **TLS / mTLS.** Transport-layer concern; wrap `TcpStream` in `tokio_rustls` when needed. No protocol changes required.
- **Authentication.** The `auth_data: bytes` slot on `ConnectRequest` exists; v1 ignores its contents. The first auth method (SASL-PLAIN, token-based, …) gets its own design.
- **Schema registry RPCs.** `RegisterSchema`, `FetchSchema`, schema-id allocation. The wire's `schema_id` stays a producer-supplied opaque `uint32` until the registry exists.
- **Multi-broker / partition routing.** Single broker for v1. A future `Lookup` RPC and broker-to-broker traffic patterns get their own design tied to the storage spec's multi-broker deferrals.
- **Transactions, idempotent produce.** Kafka's exactly-once semantics. Not v1.
- **Kafka wire-protocol compatibility layer.** A future translator that lets Kafka clients talk to kafkrs. Lives as a parallel listener that bridges Kafka's wire to internal actors. Not part of this spec.
- **Cross-language clients beyond Python.** The `.proto` file is the contract; anyone can implement a client. The first additional client (likely Go or JS) will surface ergonomic patterns worth documenting in a separate clients design.

## Open questions / risks the implementation should monitor

- **Python protobuf decode performance.** Pure-Python `protobuf` is significantly slower than the C++ extension. If the Python client becomes a hot loop, consider `protobuf` with the C++ backend (still pip-installable) or eventually a Rust-backed extension. Not a v1 problem.
- **prost's `bytes` decode copies.** `prost`-generated structs put `bytes` fields into `Vec<u8>`. For commands with embedded `bytes` (like `auth_data`), this is one extra copy per Command. The hot-path `Produce` / `Fetch` payloads are NOT embedded in the protobuf — they live in the separate Payload section, which is sliced from the receive buffer without copies. The performance argument that motivated Pulsar-style framing is preserved.
- **No backpressure beyond the TCP socket buffer.** A misbehaving client that produces faster than the broker can WAL-fsync will fill the connection's inbound mpsc buffer; the reader task blocks; the TCP receive window closes; the client's writes block. Sufficient for v1. Explicit flow control comes with the streaming-consumer RPCs.
- **The `correlation_id` is `uint64` and assumed unique-per-connection by the client.** Brokers do not validate uniqueness. A client that reuses a `correlation_id` may receive a response intended for an earlier request. Clients SHOULD start `correlation_id` at `1` and never reuse `0` — `0` is reserved for broker-emitted connection-level errors with no associated request. Documented as a client responsibility.
- **Connection setup cost.** Connect requires one TCP roundtrip before any RPC. For very short-lived clients (a single produce, then disconnect), this is overhead. Mitigation: clients pool connections.

## Invariants (summary for implementers)

These are the load-bearing properties the implementation MUST preserve:

1. The first frame on every connection is a `Connect` command.
2. Every response Command echoes the request's `correlation_id` exactly.
3. The outer frame format is fixed within a `protocol_version`.
4. Within a `protocol_version`, all schema changes are additive (new optional fields, new oneof variants, new enum values).
5. Connection-level errors close the connection; per-request errors do not.
6. The `Payload` section is sliced, never decoded by the protobuf layer.
7. The single per-connection writer task is the sole writer to the socket — responses never interleave bytes.
8. `kafkrs-python` never imports `kafkrs-models`. The `.proto` file is the only shared artifact.
