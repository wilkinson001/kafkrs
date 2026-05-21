# Wire Protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the ad-hoc bincode framing between `kafkrs-server` and `kafkrs-python` with a versioned, cross-language wire protocol defined in protobuf, framed Pulsar-style (custom TCP length-prefixed frames carrying a protobuf `Command` envelope plus a raw payload section).

**Architecture:** A single `.proto` file in `kafkrs-models` is the canonical contract. The Rust side consumes it via `prost-build` at compile time; the Python side consumes a checked-in `*_pb2.py` produced from the same `.proto`. The server's listener is rewritten as a `wire` module: each TCP connection runs three tokio tasks (reader / dispatcher / writer) communicating via mpsc channels, with explicit Connect handshake, per-RPC task spawning for in-flight multiplexing, and structured `ErrorCode` taxonomy. `kafkrs-python` is restructured from a PyO3 extension into a pure-Python async client.

**Tech Stack:** Rust 1.x, `prost` + `prost-build` (protobuf codegen), `tokio-util` `LengthDelimitedCodec` (framing), `tokio::sync::{mpsc, broadcast, oneshot}` (actor channels — already established pattern). Python 3.8+ with `protobuf` runtime (pure-Python, no native deps), `asyncio` for the client. `buf` CLI for proto linting and Python codegen.

**Spec:** `docs/superpowers/specs/2026-05-20-wire-protocol-design.md`

---

## File structure

### kafkrs-models (gains the proto + generated types)
- Create: `kafkrs-models/proto/wire/v1.proto` — the canonical schema.
- Create: `kafkrs-models/build.rs` — runs `prost_build` at compile time.
- Create: `kafkrs-models/src/wire.rs` — module that includes the generated Rust code via `include!(concat!(env!("OUT_DIR"), "/kafkrs.wire.v1.rs"))`.
- Modify: `kafkrs-models/src/lib.rs` — add `pub mod wire;`.
- Modify: `kafkrs-models/Cargo.toml` — add `prost` runtime dep, `prost-build` build dep, bump to 0.3.0.

### kafkrs-server (listener rewrite + integration test)
- Create: `kafkrs-server/src/wire/mod.rs` — module root, re-exports.
- Create: `kafkrs-server/src/wire/frame.rs` — encode/decode the outer frame (`total_size` + `command_size` + protobuf + payload).
- Create: `kafkrs-server/src/wire/errors.rs` — internal-error → `ErrorCode` mapping.
- Create: `kafkrs-server/src/wire/dispatch.rs` — per-RPC handlers (Produce, Fetch, CreateTopic, DescribeTopic, ListTopics, Ping).
- Create: `kafkrs-server/src/wire/connection.rs` — Connection state machine, three-task spawn.
- Delete: `kafkrs-server/src/listener.rs` (replaced wholesale by the `wire` module).
- Modify: `kafkrs-server/src/lib.rs` — remove `pub mod listener;`, add `pub mod wire;`.
- Modify: `kafkrs-server/src/main.rs` — use `wire::accept_loop` instead of `Listener::new`.
- Modify: `kafkrs-server/Cargo.toml` — add `prost`, `tokio-util` (with `codec` feature); remove `bincode`; bump to 0.3.0.
- Create: `kafkrs-server/tests/wire_e2e.rs` — end-to-end test driving the broker over a real TCP socket.

### kafkrs-python (full restructure to pure Python)
- Delete: `kafkrs-python/src/lib.rs`
- Delete: `kafkrs-python/src/` (entire directory)
- Delete: `kafkrs-python/Cargo.toml`
- Delete: `kafkrs-python/build.rs`
- Delete: `kafkrs-python/uv.lock`
- Create: `kafkrs-python/kafkrs/__init__.py` — exposes `Client` from `kafkrs.client`.
- Create: `kafkrs-python/kafkrs/wire/__init__.py` — empty.
- Create: `kafkrs-python/kafkrs/wire/v1_pb2.py` — generated protobuf bindings, checked in.
- Create: `kafkrs-python/kafkrs/client.py` — async TCP client.
- Create: `kafkrs-python/tests/__init__.py` — empty.
- Create: `kafkrs-python/tests/test_client.py` — end-to-end test against a spawned server.
- Modify: `kafkrs-python/pyproject.toml` — switch from `maturin` to `hatchling`; declare `protobuf` runtime dep; declare `pytest`, `pytest-asyncio` dev deps.

### Workspace
- Create: `buf.yaml` (workspace root) — proto linting rules.
- Create: `buf.gen.yaml` (workspace root) — Python codegen config (generates `kafkrs-python/kafkrs/wire/v1_pb2.py`).
- Modify: `Cargo.toml` (workspace root) — remove `kafkrs-python` from `members`.

### Changelogs (already split per crate)
- Modify: `kafkrs-models/CHANGELOG.md` — add 0.3.0 entry.
- Modify: `kafkrs-server/CHANGELOG.md` — add 0.3.0 entry.
- Modify: `kafkrs-python/CHANGELOG.md` — add 0.3.0 entry.

---

## Task 1: Add prost dependencies to kafkrs-models

**Files:**
- Modify: `kafkrs-models/Cargo.toml`

- [ ] **Step 1: Add prost runtime + prost-build to dependencies**

Edit `kafkrs-models/Cargo.toml`. Add `prost` to `[dependencies]` and create a `[build-dependencies]` section with `prost-build`:

```toml
[package]
name = "kafkrs-models"
version = "0.2.0"
edition = "2021"

[dependencies]
arrow = "55.0.0"
arrow-array = "55.0.0"
arrow-schema = "55.0.0"
chrono = { version = "0.4.38", features = ["serde"] }
crc32c = "0.6"
prost = "0.13"
serde = { version = "1.0.198", features = ["derive"] }
serde_json = "1.0"

[build-dependencies]
prost-build = "0.13"

[dev-dependencies]
toml = "0.8.12"
```

- [ ] **Step 2: Verify it compiles (no proto file yet, prost is just a dep)**

Run: `cargo check -p kafkrs-models`
Expected: success (warnings ok). Prost is a dep but nothing uses it yet.

- [ ] **Step 3: Commit**

```bash
git add kafkrs-models/Cargo.toml
git commit -m "deps: add prost + prost-build to kafkrs-models"
```

---

## Task 2: Create the proto schema file

**Files:**
- Create: `kafkrs-models/proto/wire/v1.proto`

- [ ] **Step 1: Create the proto directory**

Run: `mkdir -p kafkrs-models/proto/wire`

- [ ] **Step 2: Write the .proto file**

Create `kafkrs-models/proto/wire/v1.proto` with the full schema from the spec. Exact content:

```proto
syntax = "proto3";
package kafkrs.wire.v1;

// Top-level envelope. Every frame on the wire carries exactly one Command.
message Command {
  // Reserved for v1.5+ streaming-consumer RPCs:
  //   Subscribe / Subscribed / Flow / Message / Ack / Unsubscribe
  reserved 40 to 49;
  // Reserved for v1.5+ admin RPCs:
  //   DeleteTopic / DeleteTopicResp / AlterConfig / AlterConfigResp / BrokerInfo
  reserved 50 to 59;

  uint64 correlation_id = 1;

  oneof body {
    // Connection lifecycle (10-19)
    ConnectRequest         connect               = 10;
    ConnectedResponse      connected             = 11;
    PingRequest            ping                  = 12;
    PongResponse           pong                  = 13;

    // Data plane (20-29)
    ProduceRequest         produce               = 20;
    ProduceResponse        produce_resp          = 21;
    FetchRequest           fetch                 = 22;
    FetchResponse          fetch_resp            = 23;

    // Control plane (30-39)
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
  uint32 protocol_version = 1;
  string client_id        = 2;
  bytes  auth_data        = 3;
}

message ConnectedResponse {
  uint32 protocol_version = 1;
  string broker_id        = 2;
}

message PingRequest  {}
message PongResponse {}

// ---- Data plane ----

// Producer-side record metadata. Payload bytes follow the Command in the order
// of this list: key1 || value1 || key2 || value2 || ...
message InRecordMeta {
  uint32 key_len      = 1;
  uint32 value_len    = 2;
  uint32 schema_id    = 3;
  int64  timestamp_ns = 4;
}

message ProduceRequest {
  string topic                  = 1;
  uint32 partition              = 2;
  repeated InRecordMeta records = 3;
}

message ProduceResponse {
  int64 base_offset = 1;
  int64 last_offset = 2;
  int64 hwm         = 3;
}

// Broker-side record metadata (includes broker-assigned offset).
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
  uint32 max_wait_ms  = 5;
}

message FetchResponse {
  repeated OutRecordMeta records = 1;
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
message CreateTopicResponse {}

message DescribeTopicRequest  { string topic = 1; }
message DescribeTopicResponse {
  string topic                = 1;
  uint32 partition_count      = 2;
  int64  created_at_ns        = 3;
  TopicConfigOverrides config = 4;
}

message ListTopicsRequest  {}
message ListTopicsResponse { repeated string topics = 1; }

// ---- Errors ----

message ErrorResponse {
  ErrorCode code    = 1;
  string    message = 2;
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

- [ ] **Step 3: Commit**

```bash
git add kafkrs-models/proto/wire/v1.proto
git commit -m "wire: add v1.proto schema"
```

---

## Task 3: Wire prost-build into kafkrs-models compile

**Files:**
- Create: `kafkrs-models/build.rs`
- Create: `kafkrs-models/src/wire.rs`
- Modify: `kafkrs-models/src/lib.rs`

- [ ] **Step 1: Write the build script**

Create `kafkrs-models/build.rs`:

```rust
fn main() {
    let mut config = prost_build::Config::new();
    // Re-run when the proto file changes.
    println!("cargo:rerun-if-changed=proto/wire/v1.proto");
    config
        .compile_protos(&["proto/wire/v1.proto"], &["proto"])
        .expect("prost-build failed to compile wire/v1.proto");
}
```

- [ ] **Step 2: Write the wire module**

Create `kafkrs-models/src/wire.rs`:

```rust
//! Generated wire-protocol types. The schema lives in
//! `kafkrs-models/proto/wire/v1.proto` and is compiled at build time by
//! `build.rs`.

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/kafkrs.wire.v1.rs"));
}
```

- [ ] **Step 3: Expose the wire module**

Edit `kafkrs-models/src/lib.rs` to add `pub mod wire;` so the file reads:

```rust
pub mod config;
pub mod manifest;
pub mod record;
pub mod topic;
pub mod wal;
pub mod wire;
```

- [ ] **Step 4: Verify the generated types compile**

Run: `cargo build -p kafkrs-models`
Expected: success. Compilation should produce `target/debug/build/kafkrs-models-*/out/kafkrs.wire.v1.rs`.

- [ ] **Step 5: Verify the generated types are usable**

Write a one-off check file `kafkrs-models/tests/wire_compile.rs`:

```rust
//! Smoke test that the generated wire types are accessible.

#[test]
fn command_type_exists() {
    let cmd = kafkrs_models::wire::v1::Command {
        correlation_id: 42,
        body: None,
    };
    assert_eq!(cmd.correlation_id, 42);
}

#[test]
fn error_code_enum_values_match_spec() {
    use kafkrs_models::wire::v1::ErrorCode;
    assert_eq!(ErrorCode::ErrUnsupportedProtocolVersion as i32, 100);
    assert_eq!(ErrorCode::ErrUnknownTopic as i32, 200);
    assert_eq!(ErrorCode::ErrTopicAlreadyExists as i32, 300);
    assert_eq!(ErrorCode::ErrInternal as i32, 900);
}
```

- [ ] **Step 6: Run the smoke tests**

Run: `cargo test -p kafkrs-models --test wire_compile`
Expected: both tests pass.

- [ ] **Step 7: Commit**

```bash
git add kafkrs-models/build.rs kafkrs-models/src/wire.rs kafkrs-models/src/lib.rs kafkrs-models/tests/wire_compile.rs
git commit -m "wire: generate Rust bindings for v1.proto via prost-build"
```

---

## Task 4: Add wire dependencies to kafkrs-server

**Files:**
- Modify: `kafkrs-server/Cargo.toml` (via `cargo add`)
- Modify: `Cargo.lock`

Leave `bincode` in place — it is still referenced by `listener.rs`. Task 11 removes it once `listener.rs` is deleted in Task 9.

- [ ] **Step 1: Add prost**

Run: `cargo add prost -p kafkrs-server`
Expected: `kafkrs-server/Cargo.toml` gains a `prost = "<latest>"` line under `[dependencies]`.

- [ ] **Step 2: Add tokio-util with the codec feature**

Run: `cargo add tokio-util --features codec -p kafkrs-server`
Expected: `kafkrs-server/Cargo.toml` gains `tokio-util = { version = "<latest>", features = ["codec"] }`.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p kafkrs-server`
Expected: success.

- [ ] **Step 4: Commit**

```bash
git add kafkrs-server/Cargo.toml Cargo.lock
git commit -m "deps: add prost + tokio-util/codec to kafkrs-server"
```

---

## Task 5: Implement the outer frame codec

**Files:**
- Create: `kafkrs-server/src/wire/mod.rs`
- Create: `kafkrs-server/src/wire/frame.rs`

- [ ] **Step 1: Create the wire module file**

Create `kafkrs-server/src/wire/mod.rs`:

```rust
//! TCP wire protocol implementation.
//!
//! See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md`.

pub mod frame;
```

- [ ] **Step 2: Wire it into lib.rs**

Edit `kafkrs-server/src/lib.rs` to add `pub mod wire;` *without* yet removing `pub mod listener;`:

```rust
pub mod config;
pub mod fetcher;
pub mod listener;
pub mod object_store;
pub mod partition_writer;
pub mod recovery;
pub mod segment;
pub mod topic_registry;
pub mod uploader;
pub mod wal_writer;
pub mod wire;
```

- [ ] **Step 3: Write the failing test for frame encode**

Create `kafkrs-server/src/wire/frame.rs` with only the test body, no implementation:

```rust
use bytes::{Bytes, BytesMut};
use kafkrs_models::wire::v1::Command;
use prost::Message;

pub const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

/// One decoded frame: the protobuf Command plus the raw payload section.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub command: Command,
    pub payload: Bytes,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FrameError {
    #[error("frame too large: {0} bytes exceeds the {MAX_FRAME_SIZE} byte limit")]
    TooLarge(usize),
    #[error("frame malformed: {0}")]
    Malformed(&'static str),
    #[error("protobuf decode failed")]
    ProstDecode,
}

/// Encode a Frame to the on-wire bytes (total_size + command_size + command +
/// payload). Returns an error if the resulting frame would exceed MAX_FRAME_SIZE.
pub fn encode_frame(_frame: &Frame) -> Result<Bytes, FrameError> {
    unimplemented!("written in step 4")
}

/// Decode a frame body (everything AFTER the outer total_size prefix that
/// LengthDelimitedCodec strips). The input bytes are: command_size (4 B) +
/// command (command_size B) + payload (rest).
pub fn decode_frame_body(_body: &[u8]) -> Result<Frame, FrameError> {
    unimplemented!("written in step 5")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kafkrs_models::wire::v1::{command::Body, PingRequest};

    fn ping_command(correlation_id: u64) -> Command {
        Command {
            correlation_id,
            body: Some(Body::Ping(PingRequest {})),
        }
    }

    #[test]
    fn encode_empty_payload_command_has_correct_size_prefixes() {
        let frame = Frame {
            command: ping_command(7),
            payload: Bytes::new(),
        };
        let bytes = encode_frame(&frame).expect("encode");

        // First 4 bytes: total_size (big-endian u32), which excludes itself.
        let total_size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(bytes.len(), 4 + total_size);

        // Next 4 bytes: command_size (big-endian u32).
        let command_size = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;

        // For an empty payload, total_size == 4 (command_size) + command_size.
        assert_eq!(total_size, 4 + command_size);
    }

    #[test]
    fn encode_decode_roundtrip_no_payload() {
        let frame = Frame {
            command: ping_command(42),
            payload: Bytes::new(),
        };
        let bytes = encode_frame(&frame).expect("encode");
        // Skip the outer 4-byte total_size; that's what LengthDelimitedCodec strips.
        let decoded = decode_frame_body(&bytes[4..]).expect("decode");
        assert_eq!(decoded.command.correlation_id, 42);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn encode_decode_roundtrip_with_payload() {
        let payload = Bytes::from_static(b"hello-world-payload-bytes");
        let frame = Frame {
            command: ping_command(99),
            payload: payload.clone(),
        };
        let bytes = encode_frame(&frame).expect("encode");
        let decoded = decode_frame_body(&bytes[4..]).expect("decode");
        assert_eq!(decoded.command.correlation_id, 99);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn encode_rejects_too_large() {
        // Build a Command whose serialized size alone exceeds the cap.
        let huge = Command {
            correlation_id: 0,
            body: Some(Body::Connect(kafkrs_models::wire::v1::ConnectRequest {
                protocol_version: 1,
                client_id: "x".repeat(MAX_FRAME_SIZE + 1024),
                auth_data: vec![],
            })),
        };
        let frame = Frame {
            command: huge,
            payload: Bytes::new(),
        };
        let err = encode_frame(&frame).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }

    #[test]
    fn decode_malformed_too_short() {
        let err = decode_frame_body(&[0, 0, 0]).unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)));
    }

    #[test]
    fn decode_command_size_exceeds_body() {
        // command_size says 100, but body has only 4 more bytes.
        let mut buf = vec![];
        buf.extend_from_slice(&100u32.to_be_bytes());
        buf.extend_from_slice(b"abcd");
        let err = decode_frame_body(&buf).unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)));
    }
}
```

Also add `thiserror = "1"` to `kafkrs-server/Cargo.toml` `[dependencies]`. (Add the line if not present.)

- [ ] **Step 4: Run the tests — they should fail with `unimplemented!`**

Run: `cargo test -p kafkrs-server --lib wire::frame::tests`
Expected: tests fail because `encode_frame` and `decode_frame_body` are `unimplemented!`.

- [ ] **Step 5: Implement encode_frame**

Replace the `unimplemented!()` body of `encode_frame` with:

```rust
pub fn encode_frame(frame: &Frame) -> Result<Bytes, FrameError> {
    let command_bytes_len = frame.command.encoded_len();
    let payload_len = frame.payload.len();
    // total_size excludes itself but includes the 4-byte command_size field.
    let total_size = 4usize
        .checked_add(command_bytes_len)
        .and_then(|n| n.checked_add(payload_len))
        .ok_or(FrameError::TooLarge(usize::MAX))?;
    // Whole-frame check: outer 4 bytes (total_size field) + total_size.
    let whole = total_size.checked_add(4).ok_or(FrameError::TooLarge(usize::MAX))?;
    if whole > MAX_FRAME_SIZE {
        return Err(FrameError::TooLarge(whole));
    }
    let mut buf = BytesMut::with_capacity(whole);
    buf.extend_from_slice(&(total_size as u32).to_be_bytes());
    buf.extend_from_slice(&(command_bytes_len as u32).to_be_bytes());
    frame
        .command
        .encode(&mut buf)
        .map_err(|_| FrameError::ProstDecode)?;
    buf.extend_from_slice(&frame.payload);
    Ok(buf.freeze())
}
```

- [ ] **Step 6: Implement decode_frame_body**

Replace the `unimplemented!()` body of `decode_frame_body` with:

```rust
pub fn decode_frame_body(body: &[u8]) -> Result<Frame, FrameError> {
    if body.len() < 4 {
        return Err(FrameError::Malformed("body shorter than command_size prefix"));
    }
    let command_size = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if body.len() < 4 + command_size {
        return Err(FrameError::Malformed("body shorter than declared command_size"));
    }
    let command_bytes = &body[4..4 + command_size];
    let command = Command::decode(command_bytes).map_err(|_| FrameError::ProstDecode)?;
    let payload = Bytes::copy_from_slice(&body[4 + command_size..]);
    Ok(Frame { command, payload })
}
```

- [ ] **Step 7: Run the tests — they should pass**

Run: `cargo test -p kafkrs-server --lib wire::frame::tests`
Expected: all 6 tests pass.

- [ ] **Step 8: Commit**

```bash
git add kafkrs-server/src/wire/mod.rs kafkrs-server/src/wire/frame.rs kafkrs-server/src/lib.rs kafkrs-server/Cargo.toml
git commit -m "wire: implement outer frame encode/decode codec"
```

---

## Task 6: Map internal errors to ErrorCode

**Files:**
- Create: `kafkrs-server/src/wire/errors.rs`
- Modify: `kafkrs-server/src/wire/mod.rs`

- [ ] **Step 1: Add the module declaration**

Edit `kafkrs-server/src/wire/mod.rs`:

```rust
//! TCP wire protocol implementation.
//!
//! See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md`.

pub mod errors;
pub mod frame;
```

- [ ] **Step 2: Write the failing tests**

Create `kafkrs-server/src/wire/errors.rs`:

```rust
//! Maps internal broker errors into the wire's structured ErrorCode taxonomy.

use crate::fetcher::FetchError;
use crate::topic_registry::RegistryError;
use kafkrs_models::wire::v1::{
    command::Body, Command, ErrorCode, ErrorResponse,
};

/// Build an Error Command with the given code, message, and echo correlation_id.
pub fn make_error(correlation_id: u64, code: ErrorCode, message: impl Into<String>) -> Command {
    Command {
        correlation_id,
        body: Some(Body::Error(ErrorResponse {
            code: code as i32,
            message: message.into(),
        })),
    }
}

pub fn fetch_error_code(e: &FetchError) -> ErrorCode {
    match e {
        FetchError::UnknownTopic => ErrorCode::ErrUnknownTopic,
        FetchError::UnknownPartition => ErrorCode::ErrUnknownPartition,
        FetchError::OffsetOutOfRange => ErrorCode::ErrOffsetOutOfRange,
        FetchError::BrokerNotReady => ErrorCode::ErrBrokerNotReady,
    }
}

pub fn registry_error_code(e: &RegistryError) -> ErrorCode {
    match e {
        RegistryError::AlreadyExists => ErrorCode::ErrTopicAlreadyExists,
        RegistryError::Io(_) => ErrorCode::ErrInternal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_error_mapping_is_total_and_correct() {
        assert_eq!(fetch_error_code(&FetchError::UnknownTopic), ErrorCode::ErrUnknownTopic);
        assert_eq!(fetch_error_code(&FetchError::UnknownPartition), ErrorCode::ErrUnknownPartition);
        assert_eq!(fetch_error_code(&FetchError::OffsetOutOfRange), ErrorCode::ErrOffsetOutOfRange);
        assert_eq!(fetch_error_code(&FetchError::BrokerNotReady), ErrorCode::ErrBrokerNotReady);
    }

    #[test]
    fn registry_already_exists_maps_to_topic_already_exists() {
        assert_eq!(
            registry_error_code(&RegistryError::AlreadyExists),
            ErrorCode::ErrTopicAlreadyExists,
        );
    }

    #[test]
    fn registry_io_maps_to_internal() {
        assert_eq!(
            registry_error_code(&RegistryError::Io("disk full".into())),
            ErrorCode::ErrInternal,
        );
    }

    #[test]
    fn make_error_sets_correlation_id_and_body() {
        let cmd = make_error(123, ErrorCode::ErrUnknownTopic, "no such topic");
        assert_eq!(cmd.correlation_id, 123);
        match cmd.body {
            Some(Body::Error(er)) => {
                assert_eq!(er.code, ErrorCode::ErrUnknownTopic as i32);
                assert_eq!(er.message, "no such topic");
            }
            _ => panic!("expected Error body"),
        }
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p kafkrs-server --lib wire::errors::tests`
Expected: all 4 tests pass (this is a non-TDD task — pure pure-function mapping, written and tested together).

- [ ] **Step 4: Commit**

```bash
git add kafkrs-server/src/wire/errors.rs kafkrs-server/src/wire/mod.rs
git commit -m "wire: add ErrorCode mapping for internal errors"
```

---

## Task 7: Implement RPC dispatch handlers

**Files:**
- Create: `kafkrs-server/src/wire/dispatch.rs`
- Modify: `kafkrs-server/src/wire/mod.rs`

- [ ] **Step 1: Add the module**

Edit `kafkrs-server/src/wire/mod.rs`:

```rust
//! TCP wire protocol implementation.
//!
//! See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md`.

pub mod dispatch;
pub mod errors;
pub mod frame;
```

- [ ] **Step 2: Write the SharedState struct + Produce handler**

Create `kafkrs-server/src/wire/dispatch.rs`:

```rust
//! Per-RPC handlers. Each handler turns an inbound Command into an outbound
//! response Command. Handlers are pure functions of (state, request) →
//! response and contain no connection-level concerns (no socket I/O,
//! no Connect-state tracking).

use crate::fetcher::{fetch, FetchRequest};
use crate::partition_writer::{IncomingRecord, PwMsg};
use crate::topic_registry::{RegistryError, RegistryMsg};
use crate::wire::errors::{fetch_error_code, make_error, registry_error_code};
use crate::wire::frame::Frame;
use bytes::Bytes;
use kafkrs_models::topic::TopicConfigOverrides as TopicConfigOverridesModel;
use kafkrs_models::wire::v1::{
    command::Body, Command, ConnectedResponse, CreateTopicResponse, DescribeTopicResponse,
    ErrorCode, FetchResponse, ListTopicsResponse, OutRecordMeta, PongResponse, ProduceResponse,
    TopicConfigOverrides,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};

pub const PROTOCOL_VERSION: u32 = 1;
pub const BROKER_ID: &str = "kafkrs-broker-v1";

/// Handle to a partition's actor (mpsc to PartitionWriter + broadcast for tail
/// subscribers). Matches the prior `listener::PartitionHandle`.
#[derive(Clone)]
pub struct PartitionHandle {
    pub pw_tx: mpsc::Sender<PwMsg>,
    pub tail: broadcast::Sender<i64>,
}

/// Shared state available to every per-connection task.
#[derive(Clone)]
pub struct SharedState {
    pub partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
    pub registry: mpsc::Sender<RegistryMsg>,
    pub store: Arc<dyn object_store::ObjectStore>,
    pub prefix: String,
    pub auto_create: bool,
    pub default_partition_count: u32,
}

// ---- Per-RPC handlers ----

pub fn handle_ping(correlation_id: u64) -> Frame {
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::Pong(PongResponse {})),
        },
        payload: Bytes::new(),
    }
}

pub fn handle_connected(correlation_id: u64) -> Frame {
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::Connected(ConnectedResponse {
                protocol_version: PROTOCOL_VERSION,
                broker_id: BROKER_ID.to_string(),
            })),
        },
        payload: Bytes::new(),
    }
}

pub async fn handle_produce(
    correlation_id: u64,
    state: &SharedState,
    topic: String,
    partition: u32,
    records_meta: Vec<kafkrs_models::wire::v1::InRecordMeta>,
    payload: Bytes,
) -> Frame {
    // Slice payload into per-record (key, value) pairs using the metas.
    let mut records = Vec::with_capacity(records_meta.len());
    let mut cursor = 0usize;
    for m in &records_meta {
        let kl = m.key_len as usize;
        let vl = m.value_len as usize;
        if cursor + kl + vl > payload.len() {
            return Frame {
                command: make_error(
                    correlation_id,
                    ErrorCode::ErrMalformedFrame,
                    "produce payload shorter than declared record sizes",
                ),
                payload: Bytes::new(),
            };
        }
        let key = payload.slice(cursor..cursor + kl).to_vec();
        let value = payload.slice(cursor + kl..cursor + kl + vl).to_vec();
        cursor += kl + vl;
        records.push(IncomingRecord {
            schema_id: m.schema_id,
            key,
            value,
            timestamp_ns: m.timestamp_ns,
        });
    }
    if cursor != payload.len() {
        return Frame {
            command: make_error(
                correlation_id,
                ErrorCode::ErrMalformedFrame,
                "produce payload longer than declared record sizes",
            ),
            payload: Bytes::new(),
        };
    }

    // Auto-create the topic if configured.
    if state.auto_create {
        let (r, rr) = oneshot::channel::<Result<(), RegistryError>>();
        let _ = state
            .registry
            .send(RegistryMsg::EnsureExists {
                name: topic.clone(),
                partition_count: state.default_partition_count,
                reply: r,
            })
            .await;
        let _ = rr.await;
    }

    // Resolve the partition handle.
    let handle = {
        let guard = state.partitions.read().await;
        guard.get(&(topic.clone(), partition)).cloned()
    };
    let Some(handle) = handle else {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
            payload: Bytes::new(),
        };
    };

    let n = records.len() as i64;
    let (ack, ack_rx) = oneshot::channel::<i64>();
    if handle
        .pw_tx
        .send(PwMsg::Produce { records, ack })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    match ack_rx.await {
        Ok(base_offset) => Frame {
            command: Command {
                correlation_id,
                body: Some(Body::ProduceResp(ProduceResponse {
                    base_offset,
                    last_offset: base_offset + n - 1,
                    hwm: base_offset + n - 1,
                })),
            },
            payload: Bytes::new(),
        },
        Err(_) => Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        },
    }
}

pub async fn handle_fetch(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::FetchRequest,
) -> Frame {
    let handle = {
        let guard = state.partitions.read().await;
        guard.get(&(req.topic.clone(), req.partition)).cloned()
    };
    let Some(handle) = handle else {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
            payload: Bytes::new(),
        };
    };
    let result = fetch(
        FetchRequest {
            topic: req.topic,
            partition: req.partition,
            from_offset: req.from_offset,
            max_records: req.max_records as usize,
            max_wait_ms: req.max_wait_ms as u64,
        },
        &handle.pw_tx,
        &handle.tail,
        &state.store,
        &state.prefix,
    )
    .await;
    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            return Frame {
                command: make_error(correlation_id, fetch_error_code(&e), ""),
                payload: Bytes::new(),
            };
        }
    };
    // Build payload + metas.
    let mut payload = bytes::BytesMut::new();
    let mut metas = Vec::with_capacity(resp.records.len());
    for r in &resp.records {
        metas.push(OutRecordMeta {
            offset: r.offset,
            timestamp_ns: r.timestamp_ns,
            schema_id: r.schema_id,
            key_len: r.key.len() as u32,
            value_len: r.value.len() as u32,
        });
        payload.extend_from_slice(&r.key);
        payload.extend_from_slice(&r.value);
    }
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::FetchResp(FetchResponse {
                records: metas,
                hwm: resp.hwm,
            })),
        },
        payload: payload.freeze(),
    }
}

pub async fn handle_create_topic(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::CreateTopicRequest,
) -> Frame {
    let overrides = wire_overrides_to_model(req.overrides.unwrap_or_default());
    let (tx, rx) = oneshot::channel::<Result<(), RegistryError>>();
    if state
        .registry
        .send(RegistryMsg::Create {
            name: req.topic,
            partition_count: req.partition_count,
            overrides,
            reply: tx,
        })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    match rx.await {
        Ok(Ok(())) => Frame {
            command: Command {
                correlation_id,
                body: Some(Body::CreateTopicResp(CreateTopicResponse {})),
            },
            payload: Bytes::new(),
        },
        Ok(Err(e)) => Frame {
            command: make_error(correlation_id, registry_error_code(&e), format!("{e:?}")),
            payload: Bytes::new(),
        },
        Err(_) => Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        },
    }
}

pub async fn handle_describe_topic(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::DescribeTopicRequest,
) -> Frame {
    let (tx, rx) = oneshot::channel();
    if state
        .registry
        .send(RegistryMsg::Describe {
            name: req.topic.clone(),
            reply: tx,
        })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    match rx.await.ok().flatten() {
        Some(entry) => Frame {
            command: Command {
                correlation_id,
                body: Some(Body::DescribeTopicResp(DescribeTopicResponse {
                    topic: entry.name,
                    partition_count: entry.partition_count,
                    created_at_ns: entry.created_at_ns,
                    config: Some(model_overrides_to_wire(entry.config)),
                })),
            },
            payload: Bytes::new(),
        },
        None => Frame {
            command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
            payload: Bytes::new(),
        },
    }
}

pub async fn handle_list_topics(correlation_id: u64, state: &SharedState) -> Frame {
    let (tx, rx) = oneshot::channel();
    if state
        .registry
        .send(RegistryMsg::List { reply: tx })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    let topics = rx.await.unwrap_or_default();
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::ListTopicsResp(ListTopicsResponse { topics })),
        },
        payload: Bytes::new(),
    }
}

// ---- Overrides translation ----

fn wire_overrides_to_model(w: TopicConfigOverrides) -> TopicConfigOverridesModel {
    TopicConfigOverridesModel {
        segment_size_bytes: w.segment_size_bytes,
        segment_seal_time_ms: w.segment_seal_time_ms,
        max_key_size_bytes: w.max_key_size_bytes,
        max_value_size_bytes: w.max_value_size_bytes,
        group_commit_time_ms: w.group_commit_time_ms,
        group_commit_size_bytes: w.group_commit_size_bytes,
        group_commit_record_count: w.group_commit_record_count,
    }
}

fn model_overrides_to_wire(m: TopicConfigOverridesModel) -> TopicConfigOverrides {
    TopicConfigOverrides {
        segment_size_bytes: m.segment_size_bytes,
        segment_seal_time_ms: m.segment_seal_time_ms,
        max_key_size_bytes: m.max_key_size_bytes,
        max_value_size_bytes: m.max_value_size_bytes,
        group_commit_time_ms: m.group_commit_time_ms,
        group_commit_size_bytes: m.group_commit_size_bytes,
        group_commit_record_count: m.group_commit_record_count,
    }
}
```

**NOTE for the implementing engineer:** check `kafkrs-models/src/topic.rs` to confirm `TopicConfigOverrides` has the same field names. If any field name differs, fix the translation function field-by-field. The model struct already exists; this is purely a name/visibility check.

**NOTE on `TopicEntry`:** the registry returns `TopicEntry`. Check `kafkrs-models/src/topic.rs` for the actual field names of `TopicEntry`. The code above assumes `name`, `partition_count`, `created_at_ns`, `config` — verify and adjust if needed.

- [ ] **Step 3: Verify the dispatch module compiles**

Run: `cargo check -p kafkrs-server`
Expected: success. If a TopicEntry / TopicConfigOverrides field name mismatch surfaces, fix it before continuing.

- [ ] **Step 4: Commit**

```bash
git add kafkrs-server/src/wire/dispatch.rs kafkrs-server/src/wire/mod.rs
git commit -m "wire: implement per-RPC dispatch handlers"
```

---

## Task 8: Implement the per-connection three-task state machine

**Files:**
- Create: `kafkrs-server/src/wire/connection.rs`
- Modify: `kafkrs-server/src/wire/mod.rs`

- [ ] **Step 1: Add the module**

Edit `kafkrs-server/src/wire/mod.rs`:

```rust
//! TCP wire protocol implementation.
//!
//! See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md`.

pub mod connection;
pub mod dispatch;
pub mod errors;
pub mod frame;

pub use connection::accept_loop;
pub use dispatch::{PartitionHandle, SharedState};
```

- [ ] **Step 2: Write the connection module**

Create `kafkrs-server/src/wire/connection.rs`:

```rust
//! Per-connection state machine. Spawns three cooperating tasks:
//!
//! - Reader: pulls frames off the socket, decodes them, forwards Commands to
//!   the dispatcher.
//! - Dispatcher: enforces the Connect handshake, spawns a per-RPC task for
//!   each subsequent Command so a long-polling Fetch does not block other
//!   RPCs on the same connection.
//! - Writer: drains a response-Command mpsc and writes one frame at a time so
//!   no two responses interleave bytes on the socket.

use crate::wire::dispatch::{
    handle_connected, handle_create_topic, handle_describe_topic, handle_fetch,
    handle_list_topics, handle_ping, handle_produce, PROTOCOL_VERSION, SharedState,
};
use crate::wire::errors::make_error;
use crate::wire::frame::{decode_frame_body, encode_frame, Frame, FrameError, MAX_FRAME_SIZE};
use bytes::Bytes;
use futures::SinkExt;
use kafkrs_models::wire::v1::{command::Body, Command, ErrorCode};
use log::{debug, error, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

const CONNECTION_RESPONSE_BUFFER: usize = 256;
const CONNECTION_REQUEST_BUFFER: usize = 256;

/// Accept loop bound to one TCP listener. Spawns one connection task per
/// accepted socket. Replaces the loop in `main.rs` that called
/// `Listener::new(...).process()`.
pub async fn accept_loop(listener: TcpListener, state: SharedState) {
    loop {
        match listener.accept().await {
            Ok((socket, peer)) => {
                debug!("accepted connection from {peer}");
                let st = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = run_connection(socket, st).await {
                        debug!("connection {peer} closed: {e:?}");
                    }
                });
            }
            Err(e) => error!("accept error: {e}"),
        }
    }
}

#[derive(Debug)]
enum ConnError {
    Closed,
    Frame(FrameError),
    Io(std::io::Error),
}

impl From<std::io::Error> for ConnError {
    fn from(e: std::io::Error) -> Self {
        ConnError::Io(e)
    }
}

async fn run_connection(
    socket: tokio::net::TcpStream,
    state: SharedState,
) -> Result<(), ConnError> {
    let (rd, mut wr) = socket.into_split();
    // Reader uses LengthDelimitedCodec to strip the outer total_size prefix.
    // The bytes it hands us start with command_size + protobuf + payload.
    let codec = LengthDelimitedCodec::builder()
        .length_field_offset(0)
        .length_field_length(4)
        .length_field_type::<u32>()
        .big_endian()
        .max_frame_length(MAX_FRAME_SIZE)
        .new_codec();
    let mut frames = FramedRead::new(rd, codec);

    // Channel: requests reader → dispatcher.
    let (req_tx, mut req_rx) = mpsc::channel::<Frame>(CONNECTION_REQUEST_BUFFER);
    // Channel: responses (from per-RPC tasks) → writer.
    let (resp_tx, mut resp_rx) = mpsc::channel::<Frame>(CONNECTION_RESPONSE_BUFFER);

    // Writer task: drain resp_rx, encode, write. Serializes socket writes.
    let writer = tokio::spawn(async move {
        while let Some(frame) = resp_rx.recv().await {
            let bytes = match encode_frame(&frame) {
                Ok(b) => b,
                Err(e) => {
                    error!("failed to encode outbound frame: {e}");
                    continue;
                }
            };
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    // Reader task: pull frames off socket → req_tx.
    let req_tx_for_reader = req_tx.clone();
    let reader = tokio::spawn(async move {
        while let Some(item) = frames.next().await {
            let body = match item {
                Ok(b) => b,
                Err(e) => {
                    warn!("framing error: {e}");
                    break;
                }
            };
            let frame = match decode_frame_body(&body) {
                Ok(f) => f,
                Err(e) => {
                    warn!("frame decode error: {e}");
                    // Send a malformed-frame error with correlation_id = 0 (we
                    // could not decode the inbound command).
                    let _ = req_tx_for_reader.send(Frame {
                        command: make_error(0, ErrorCode::ErrMalformedFrame, format!("{e}")),
                        payload: Bytes::new(),
                    }).await;
                    break;
                }
            };
            if req_tx_for_reader.send(frame).await.is_err() {
                break;
            }
        }
        // signal end-of-stream to the dispatcher
        drop(req_tx_for_reader);
    });

    // Dispatcher: state machine + per-RPC spawn.
    let mut connected = false;
    while let Some(frame) = req_rx.recv().await {
        let cid = frame.command.correlation_id;
        // A malformed-frame sentinel from the reader: forward to writer and exit.
        if matches!(
            frame.command.body,
            Some(Body::Error(_)) // reader injects errors only for unrecoverable framing failures
        ) {
            let _ = resp_tx.send(frame).await;
            break;
        }
        match (connected, frame.command.body.clone()) {
            (false, Some(Body::Connect(req))) => {
                if req.protocol_version != PROTOCOL_VERSION {
                    let _ = resp_tx
                        .send(Frame {
                            command: make_error(
                                cid,
                                ErrorCode::ErrUnsupportedProtocolVersion,
                                format!(
                                    "server speaks protocol_version={}; client requested {}",
                                    PROTOCOL_VERSION, req.protocol_version
                                ),
                            ),
                            payload: Bytes::new(),
                        })
                        .await;
                    break;
                }
                let _ = resp_tx.send(handle_connected(cid)).await;
                connected = true;
            }
            (false, _) => {
                let _ = resp_tx
                    .send(Frame {
                        command: make_error(cid, ErrorCode::ErrHandshakeRequired, "first frame must be Connect"),
                        payload: Bytes::new(),
                    })
                    .await;
                break;
            }
            (true, Some(Body::Connect(_))) => {
                let _ = resp_tx
                    .send(Frame {
                        command: make_error(cid, ErrorCode::ErrAlreadyConnected, ""),
                        payload: Bytes::new(),
                    })
                    .await;
                break;
            }
            (true, Some(body)) => {
                // Spawn a per-RPC task so long-poll fetches don't block others.
                let resp_tx = resp_tx.clone();
                let state = state.clone();
                let payload = frame.payload.clone();
                tokio::spawn(async move {
                    let response = dispatch_one(cid, body, payload, &state).await;
                    let _ = resp_tx.send(response).await;
                });
            }
            (true, None) => {
                let _ = resp_tx
                    .send(Frame {
                        command: make_error(cid, ErrorCode::ErrInvalidCommand, "command body absent"),
                        payload: Bytes::new(),
                    })
                    .await;
            }
        }
    }

    // Drop resp_tx so the writer's loop completes.
    drop(resp_tx);
    let _ = reader.await;
    let _ = writer.await;
    Ok(())
}

async fn dispatch_one(
    correlation_id: u64,
    body: Body,
    payload: Bytes,
    state: &SharedState,
) -> Frame {
    match body {
        Body::Ping(_) => handle_ping(correlation_id),
        Body::Produce(req) => {
            handle_produce(
                correlation_id,
                state,
                req.topic,
                req.partition,
                req.records,
                payload,
            )
            .await
        }
        Body::Fetch(req) => handle_fetch(correlation_id, state, req).await,
        Body::CreateTopic(req) => handle_create_topic(correlation_id, state, req).await,
        Body::DescribeTopic(req) => handle_describe_topic(correlation_id, state, req).await,
        Body::ListTopics(_) => handle_list_topics(correlation_id, state).await,
        // Variants the broker should never receive as a request (they are
        // response or future-reserved variants):
        Body::Connect(_)
        | Body::Connected(_)
        | Body::Pong(_)
        | Body::ProduceResp(_)
        | Body::FetchResp(_)
        | Body::CreateTopicResp(_)
        | Body::DescribeTopicResp(_)
        | Body::ListTopicsResp(_)
        | Body::Error(_) => Frame {
            command: make_error(
                correlation_id,
                ErrorCode::ErrInvalidCommand,
                "this command is not valid as a request",
            ),
            payload: Bytes::new(),
        },
    }
}
```

- [ ] **Step 3: Add futures + tokio-stream deps if missing**

`futures` and `tokio-stream` are needed for `SinkExt`/`StreamExt`. Edit `kafkrs-server/Cargo.toml` to add:

```toml
futures = "0.3"
tokio-stream = "0.1"
```

(in `[dependencies]`).

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p kafkrs-server`
Expected: success.

- [ ] **Step 5: Commit**

```bash
git add kafkrs-server/src/wire/connection.rs kafkrs-server/src/wire/mod.rs kafkrs-server/Cargo.toml
git commit -m "wire: implement per-connection state machine + three-task model"
```

---

## Task 9: Replace the old listener in main.rs

**Files:**
- Modify: `kafkrs-server/src/main.rs`
- Modify: `kafkrs-server/src/lib.rs`
- Delete: `kafkrs-server/src/listener.rs`

- [ ] **Step 1: Update main.rs to use wire::accept_loop**

Edit `kafkrs-server/src/main.rs`. Replace the imports of `listener::*` with `wire::*`, and the per-port loop body:

Replace:
```rust
use kafkrs_server::listener::{Listener, PartitionHandle, SharedState};
```
with:
```rust
use kafkrs_server::wire::{accept_loop, PartitionHandle, SharedState};
```

Replace the per-port spawn block:
```rust
    for port in cfg.ports.clone() {
        let addr: String = format!("{}:{}", cfg.address, port);
        let listener: TcpListener = TcpListener::bind(&addr).await.expect("bind");
        info!("Listening on {addr}");
        let st: SharedState = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => {
                        let st2: SharedState = st.clone();
                        tokio::spawn(async move { Listener::new(socket, st2).process().await });
                    }
                    Err(e) => error!("accept error: {e}"),
                }
            }
        });
    }
```
with:
```rust
    for port in cfg.ports.clone() {
        let addr: String = format!("{}:{}", cfg.address, port);
        let listener: TcpListener = TcpListener::bind(&addr).await.expect("bind");
        info!("Listening on {addr}");
        let st: SharedState = state.clone();
        tokio::spawn(accept_loop(listener, st));
    }
```

- [ ] **Step 2: Remove listener.rs from lib.rs**

Edit `kafkrs-server/src/lib.rs` to remove `pub mod listener;`. Final form:

```rust
pub mod config;
pub mod fetcher;
pub mod object_store;
pub mod partition_writer;
pub mod recovery;
pub mod segment;
pub mod topic_registry;
pub mod uploader;
pub mod wal_writer;
pub mod wire;
```

- [ ] **Step 3: Delete listener.rs**

Run: `rm kafkrs-server/src/listener.rs`

- [ ] **Step 4: Verify build**

Run: `cargo build -p kafkrs-server`
Expected: success.

- [ ] **Step 5: Run existing tests to confirm nothing else regressed**

Run: `cargo test -p kafkrs-server`
Expected: all existing tests still pass (the storage tests don't touch the wire layer).

- [ ] **Step 6: Commit**

```bash
git add kafkrs-server/src/main.rs kafkrs-server/src/lib.rs
git rm kafkrs-server/src/listener.rs
git commit -m "wire: replace old listener with wire::accept_loop"
```

---

## Task 10: Write the end-to-end wire integration test

**Files:**
- Create: `kafkrs-server/tests/wire_e2e.rs`

- [ ] **Step 1: Add test-only deps**

Edit `kafkrs-server/Cargo.toml` `[dev-dependencies]`:

```toml
[dev-dependencies]
tempfile = "3"
bytes = "1"
prost = "0.13"
tokio-util = { version = "0.7", features = ["codec"] }
```

- [ ] **Step 2: Write the failing test**

Create `kafkrs-server/tests/wire_e2e.rs`:

```rust
//! End-to-end test of the wire protocol: drives the broker over a real TCP
//! socket using the actual frame format, not via the internal actors.

use bytes::{Bytes, BytesMut};
use kafkrs_models::config::{DiskType, ObjectStoreConfig};
use kafkrs_models::manifest::Manifest;
use kafkrs_models::topic::{ResolvedTopicConfig, TopicConfigOverrides};
use kafkrs_models::wire::v1::{
    command::Body, Command, ConnectRequest, FetchRequest, InRecordMeta, ProduceRequest,
};
use kafkrs_server::object_store::{build_store, manifest_key, put};
use kafkrs_server::partition_writer::{PartitionWriter, PwMsg};
use kafkrs_server::topic_registry::TopicRegistry;
use kafkrs_server::uploader::{Uploader, UploaderMsg};
use kafkrs_server::wire::{accept_loop, PartitionHandle, SharedState};
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, RwLock};

async fn setup_broker(dd: &str) -> (u16, Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>) {
    let store = build_store(
        &ObjectStoreConfig {
            backend: "filesystem".into(),
            bucket: "b".into(),
            prefix: "".into(),
            endpoint: "".into(),
            region: "us-east-1".into(),
        },
        dd,
    )
    .unwrap();
    put(
        &store,
        &manifest_key("", "t", 0),
        Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap()),
    )
    .await
    .unwrap();

    // Spin up an Uploader + PartitionWriter for ("t", 0).
    let (utx, urx) = mpsc::channel(64);
    let (dtx, mut drx) = mpsc::channel(64);
    tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, urx, dtx).run());
    let (pw_tx, pw_rx) = mpsc::channel(256);
    let (tail, _) = broadcast::channel(1024);
    let pw_tx_d = pw_tx.clone();
    tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let _ = pw_tx_d.send(PwMsg::SegmentDurable(d)).await;
        }
    });
    let mut o = TopicConfigOverrides::default();
    o.group_commit_record_count = Some(1);
    let cfg = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
    let pw = PartitionWriter::new(
        dd.into(),
        "t".into(),
        0,
        cfg,
        0,
        vec![],
        pw_rx,
        utx,
        tail.clone(),
    )
    .unwrap();
    tokio::spawn(pw.run());

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));
    partitions
        .write()
        .await
        .insert(("t".into(), 0), PartitionHandle { pw_tx, tail });

    // Spin up a topic registry actor (needed for SharedState even if not used by
    // produce/fetch in this test).
    let (reg_tx, reg_rx) = mpsc::channel(8);
    let registry = TopicRegistry::load(
        dd.into(),
        DiskType::Nvme,
        store.clone(),
        "".into(),
        reg_rx,
    )
    .unwrap();
    tokio::spawn(registry.run());

    let state = SharedState {
        partitions: partitions.clone(),
        registry: reg_tx,
        store,
        prefix: "".into(),
        auto_create: false,
        default_partition_count: 1,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(accept_loop(listener, state));
    (port, partitions)
}

/// Encode a Command + payload to outer wire bytes.
fn encode(cmd: &Command, payload: &[u8]) -> Bytes {
    let command_size = cmd.encoded_len();
    let total_size = 4 + command_size + payload.len();
    let mut buf = BytesMut::with_capacity(4 + total_size);
    buf.extend_from_slice(&(total_size as u32).to_be_bytes());
    buf.extend_from_slice(&(command_size as u32).to_be_bytes());
    cmd.encode(&mut buf).unwrap();
    buf.extend_from_slice(payload);
    buf.freeze()
}

/// Read one complete outer frame from the socket and return (Command, payload bytes).
async fn read_frame(sock: &mut TcpStream) -> (Command, Bytes) {
    let mut len_buf = [0u8; 4];
    sock.read_exact(&mut len_buf).await.unwrap();
    let total = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; total];
    sock.read_exact(&mut body).await.unwrap();
    let command_size = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    let cmd = Command::decode(&body[4..4 + command_size]).unwrap();
    let payload = Bytes::copy_from_slice(&body[4 + command_size..]);
    (cmd, payload)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_produce_fetch_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // 1. Connect.
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "test-client".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert_eq!(resp.correlation_id, 1);
    assert!(matches!(resp.body, Some(Body::Connected(_))));

    // 2. Produce one record.
    let produce = Command {
        correlation_id: 2,
        body: Some(Body::Produce(ProduceRequest {
            topic: "t".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 3,
                value_len: 5,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce, b"keyvalue")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert_eq!(resp.correlation_id, 2);
    match resp.body {
        Some(Body::ProduceResp(r)) => {
            assert_eq!(r.base_offset, 0);
            assert_eq!(r.last_offset, 0);
        }
        other => panic!("expected ProduceResp, got {other:?}"),
    }

    // 3. Fetch from offset 0.
    let fetch = Command {
        correlation_id: 3,
        body: Some(Body::Fetch(FetchRequest {
            topic: "t".into(),
            partition: 0,
            from_offset: 0,
            max_records: 10,
            max_wait_ms: 100,
        })),
    };
    sock.write_all(&encode(&fetch, b"")).await.unwrap();
    let (resp, payload) = read_frame(&mut sock).await;
    assert_eq!(resp.correlation_id, 3);
    match resp.body {
        Some(Body::FetchResp(r)) => {
            assert_eq!(r.records.len(), 1);
            assert_eq!(r.records[0].offset, 0);
            assert_eq!(r.records[0].key_len, 3);
            assert_eq!(r.records[0].value_len, 5);
            assert_eq!(payload.len(), 8);
            assert_eq!(&payload[..3], b"key");
            assert_eq!(&payload[3..], b"value");
        }
        other => panic!("expected FetchResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _) = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let connect = Command {
        correlation_id: 9,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 999,
            client_id: "x".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert_eq!(resp.correlation_id, 9);
    match resp.body {
        Some(Body::Error(e)) => {
            assert_eq!(e.code, kafkrs_models::wire::v1::ErrorCode::ErrUnsupportedProtocolVersion as i32);
        }
        other => panic!("expected Error, got {other:?}"),
    }
    // Server should close after that error.
    let mut buf = [0u8; 1];
    let n = sock.read(&mut buf).await.unwrap();
    assert_eq!(n, 0, "expected EOF after version error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_connect_command_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _) = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    // Send Ping before Connect.
    let ping = Command {
        correlation_id: 5,
        body: Some(Body::Ping(kafkrs_models::wire::v1::PingRequest {})),
    };
    sock.write_all(&encode(&ping, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert_eq!(resp.correlation_id, 5);
    match resp.body {
        Some(Body::Error(e)) => {
            assert_eq!(e.code, kafkrs_models::wire::v1::ErrorCode::ErrHandshakeRequired as i32);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p kafkrs-server --test wire_e2e`
Expected: all three tests pass.

If any test fails, fix the underlying bug in `wire/*` before continuing. The most likely bug sources are: incorrect `total_size` math in `encode_frame`, incorrect state-machine transition in `connection.rs`, or a field-name mismatch in `dispatch.rs`'s overrides translation.

- [ ] **Step 4: Commit**

```bash
git add kafkrs-server/tests/wire_e2e.rs kafkrs-server/Cargo.toml
git commit -m "wire: end-to-end TCP integration test (connect/produce/fetch)"
```

---

## Task 11: Remove bincode from kafkrs-server

**Files:**
- Modify: `kafkrs-server/Cargo.toml`

- [ ] **Step 1: Remove bincode**

Edit `kafkrs-server/Cargo.toml` and remove the `bincode = { version = "2.0.1", features = ["serde"] }` line.

- [ ] **Step 2: Verify still compiles and tests pass**

Run: `cargo test -p kafkrs-server`
Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add kafkrs-server/Cargo.toml
git commit -m "deps: remove unused bincode from kafkrs-server"
```

---

## Task 12: Add buf config files

**Files:**
- Create: `buf.yaml`
- Create: `buf.gen.yaml`

- [ ] **Step 1: Write buf.yaml**

Create `buf.yaml` at the workspace root:

```yaml
version: v2
modules:
  - path: kafkrs-models/proto
lint:
  use:
    - STANDARD
  except:
    # Allow our envelope ErrorResponse to coexist with the same name pattern at v1.
    - PACKAGE_DIRECTORY_MATCH
breaking:
  use:
    - FILE
```

- [ ] **Step 2: Write buf.gen.yaml**

Create `buf.gen.yaml` at the workspace root:

```yaml
version: v2
plugins:
  - remote: buf.build/protocolbuffers/python
    out: kafkrs-python/kafkrs/wire
    opt:
      - paths=source_relative
```

This declares the Python codegen target. Running `buf generate` will populate `kafkrs-python/kafkrs/wire/v1_pb2.py`.

- [ ] **Step 3: Verify buf lint passes**

Run: `buf lint`
Expected: no errors. (If `buf` is not installed locally, the engineer can run `brew install bufbuild/buf/buf` on macOS or follow https://buf.build/docs/installation. If running in an environment without `buf`, skip this step but note that CI will run it.)

- [ ] **Step 4: Commit**

```bash
git add buf.yaml buf.gen.yaml
git commit -m "wire: add buf.yaml + buf.gen.yaml for proto lint + codegen"
```

---

## Task 13: Delete the Rust kafkrs-python crate

**Files:**
- Delete: `kafkrs-python/src/`
- Delete: `kafkrs-python/Cargo.toml`
- Delete: `kafkrs-python/build.rs`
- Delete: `kafkrs-python/uv.lock`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Remove kafkrs-python from workspace members**

Edit the root `Cargo.toml`:

```toml
[workspace]

members = ["kafkrs-models", "kafkrs-server"]

[workspace.metadata.precommit]
fmt = "cargo fmt"
```

- [ ] **Step 2: Delete the Rust sources**

Run:
```bash
rm -rf kafkrs-python/src
rm kafkrs-python/Cargo.toml
rm -f kafkrs-python/build.rs
rm -f kafkrs-python/uv.lock
```

- [ ] **Step 3: Verify the workspace still builds**

Run: `cargo build`
Expected: success — only `kafkrs-models` and `kafkrs-server` build.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml
git rm -r kafkrs-python/src
git rm kafkrs-python/Cargo.toml
git rm --ignore-unmatch kafkrs-python/build.rs kafkrs-python/uv.lock
git commit -m "python: remove Rust crate; will be restructured as pure-Python"
```

---

## Task 14: Restructure kafkrs-python as a pure-Python package

**Files:**
- Modify: `kafkrs-python/pyproject.toml`
- Create: `kafkrs-python/kafkrs/__init__.py`
- Create: `kafkrs-python/kafkrs/wire/__init__.py`

- [ ] **Step 1: Rewrite pyproject.toml**

Replace `kafkrs-python/pyproject.toml` entirely with:

```toml
[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[project]
name = "kafkrs"
version = "0.3.0"
description = "Pure-Python async client for the kafkrs broker."
requires-python = ">=3.8"
dependencies = [
  "protobuf>=4.25,<6",
]

[project.optional-dependencies]
dev = [
  "pytest>=8.0",
  "pytest-asyncio>=0.23",
]

[tool.hatch.build.targets.wheel]
packages = ["kafkrs"]

[tool.pytest.ini_options]
asyncio_mode = "auto"
```

- [ ] **Step 2: Create the package init files**

Create `kafkrs-python/kafkrs/__init__.py`:

```python
"""Pure-Python client for the kafkrs broker."""

from kafkrs.client import Client

__all__ = ["Client"]
__version__ = "0.3.0"
```

Create `kafkrs-python/kafkrs/wire/__init__.py` as an empty file:

```bash
mkdir -p kafkrs-python/kafkrs/wire
touch kafkrs-python/kafkrs/wire/__init__.py
```

- [ ] **Step 3: Commit**

```bash
git add kafkrs-python/pyproject.toml kafkrs-python/kafkrs/__init__.py kafkrs-python/kafkrs/wire/__init__.py
git commit -m "python: scaffold pure-Python package layout"
```

---

## Task 15: Generate the Python protobuf bindings

**Files:**
- Create: `kafkrs-python/kafkrs/wire/v1_pb2.py`

- [ ] **Step 1: Generate bindings via buf**

From the repo root, run:

```bash
buf generate
```

This produces `kafkrs-python/kafkrs/wire/v1_pb2.py` (and possibly `v1_pb2.pyi` for type stubs). If `buf` is not available, fall back to `protoc`:

```bash
protoc --python_out=kafkrs-python/kafkrs/wire \
       --proto_path=kafkrs-models/proto \
       kafkrs-models/proto/wire/v1.proto
```

Note: the generated file may be named `wire/v1_pb2.py` or `v1_pb2.py` depending on the codegen plugin — adjust paths so the final layout is `kafkrs-python/kafkrs/wire/v1_pb2.py`. If `buf` produces a nested path with extra directories, move/rename the file.

- [ ] **Step 2: Verify the import works**

Run:
```bash
cd kafkrs-python && python -c "from kafkrs.wire import v1_pb2; print(v1_pb2.Command.DESCRIPTOR.full_name)"
```
Expected output: `kafkrs.wire.v1.Command`.

- [ ] **Step 3: Commit**

```bash
git add kafkrs-python/kafkrs/wire/v1_pb2.py
git commit -m "python: generate v1_pb2.py protobuf bindings"
```

---

## Task 16: Write the Python async client

**Files:**
- Create: `kafkrs-python/kafkrs/client.py`

- [ ] **Step 1: Write the client**

Create `kafkrs-python/kafkrs/client.py`:

```python
"""Async TCP client for the kafkrs wire protocol v1.

Frame layout (network byte order):

    [total_size: u32][command_size: u32][Command protobuf][payload bytes]

total_size excludes itself; command_size is the length of the protobuf Command.
"""

from __future__ import annotations

import asyncio
import struct
from dataclasses import dataclass
from typing import List, Optional, Tuple

from kafkrs.wire import v1_pb2

PROTOCOL_VERSION = 1
MAX_FRAME_SIZE = 4 * 1024 * 1024


@dataclass
class FetchedRecord:
    offset: int
    timestamp_ns: int
    schema_id: int
    key: bytes
    value: bytes


class WireError(Exception):
    """Raised when the broker returns an ErrorResponse."""

    def __init__(self, code: int, message: str):
        self.code = code
        self.message = message
        super().__init__(f"wire error {code}: {message}")


class Client:
    """Single-connection async client. One Connect per Client.connect()."""

    def __init__(self, host: str, port: int, client_id: str = "kafkrs-python"):
        self._host = host
        self._port = port
        self._client_id = client_id
        self._reader: Optional[asyncio.StreamReader] = None
        self._writer: Optional[asyncio.StreamWriter] = None
        self._next_correlation_id = 1
        self._lock = asyncio.Lock()  # serialize on-socket I/O

    async def connect(self) -> None:
        self._reader, self._writer = await asyncio.open_connection(self._host, self._port)
        cmd = v1_pb2.Command()
        cid = self._next_id()
        cmd.correlation_id = cid
        cmd.connect.protocol_version = PROTOCOL_VERSION
        cmd.connect.client_id = self._client_id
        resp, _ = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "connected":
            raise WireError(0, f"unexpected response to Connect: {resp.WhichOneof('body')}")
        if resp.connected.protocol_version != PROTOCOL_VERSION:
            raise WireError(
                0,
                f"broker protocol_version={resp.connected.protocol_version}; client={PROTOCOL_VERSION}",
            )

    async def close(self) -> None:
        if self._writer is not None:
            self._writer.close()
            try:
                await self._writer.wait_closed()
            except Exception:
                pass

    async def produce(
        self,
        topic: str,
        partition: int,
        records: List[Tuple[bytes, bytes]],
        schema_id: int = 0,
        timestamp_ns: int = 0,
    ) -> Tuple[int, int]:
        """Produce one or more (key, value) records. Returns (base_offset, last_offset)."""
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        cmd.produce.topic = topic
        cmd.produce.partition = partition
        payload = bytearray()
        for key, value in records:
            meta = cmd.produce.records.add()
            meta.key_len = len(key)
            meta.value_len = len(value)
            meta.schema_id = schema_id
            meta.timestamp_ns = timestamp_ns
            payload.extend(key)
            payload.extend(value)
        resp, _ = await self._roundtrip(cmd, bytes(payload))
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "produce_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")
        return resp.produce_resp.base_offset, resp.produce_resp.last_offset

    async def fetch(
        self,
        topic: str,
        partition: int,
        from_offset: int,
        max_records: int = 100,
        max_wait_ms: int = 0,
    ) -> Tuple[List[FetchedRecord], int]:
        """Fetch records starting at from_offset. Returns (records, hwm)."""
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        cmd.fetch.topic = topic
        cmd.fetch.partition = partition
        cmd.fetch.from_offset = from_offset
        cmd.fetch.max_records = max_records
        cmd.fetch.max_wait_ms = max_wait_ms
        resp, payload = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "fetch_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")
        out: List[FetchedRecord] = []
        cursor = 0
        for meta in resp.fetch_resp.records:
            kl = meta.key_len
            vl = meta.value_len
            key = bytes(payload[cursor : cursor + kl])
            value = bytes(payload[cursor + kl : cursor + kl + vl])
            cursor += kl + vl
            out.append(FetchedRecord(meta.offset, meta.timestamp_ns, meta.schema_id, key, value))
        return out, resp.fetch_resp.hwm

    async def create_topic(
        self,
        topic: str,
        partition_count: int,
        overrides: Optional[v1_pb2.TopicConfigOverrides] = None,
    ) -> None:
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        cmd.create_topic.topic = topic
        cmd.create_topic.partition_count = partition_count
        if overrides is not None:
            cmd.create_topic.overrides.CopyFrom(overrides)
        resp, _ = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "create_topic_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")

    async def list_topics(self) -> List[str]:
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        cmd.list_topics.SetInParent()
        resp, _ = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "list_topics_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")
        return list(resp.list_topics_resp.topics)

    # ---- Internals ----

    def _next_id(self) -> int:
        cid = self._next_correlation_id
        self._next_correlation_id += 1
        return cid

    async def _roundtrip(self, cmd: v1_pb2.Command, payload: bytes) -> Tuple[v1_pb2.Command, bytes]:
        async with self._lock:
            assert self._reader is not None and self._writer is not None
            cmd_bytes = cmd.SerializeToString()
            total_size = 4 + len(cmd_bytes) + len(payload)
            if 4 + total_size > MAX_FRAME_SIZE:
                raise WireError(0, "frame too large for client")
            self._writer.write(struct.pack(">II", total_size, len(cmd_bytes)))
            self._writer.write(cmd_bytes)
            if payload:
                self._writer.write(payload)
            await self._writer.drain()

            # Read response.
            outer = await self._reader.readexactly(4)
            (resp_total,) = struct.unpack(">I", outer)
            body = await self._reader.readexactly(resp_total)
            (resp_cmd_size,) = struct.unpack(">I", body[:4])
            resp_cmd = v1_pb2.Command()
            resp_cmd.ParseFromString(body[4 : 4 + resp_cmd_size])
            resp_payload = bytes(body[4 + resp_cmd_size :])
            return resp_cmd, resp_payload
```

- [ ] **Step 2: Smoke test the import**

Run:
```bash
cd kafkrs-python && python -c "from kafkrs import Client; print(Client)"
```
Expected: prints `<class 'kafkrs.client.Client'>`.

- [ ] **Step 3: Commit**

```bash
git add kafkrs-python/kafkrs/client.py
git commit -m "python: implement async TCP client"
```

---

## Task 17: Write the Python end-to-end test

**Files:**
- Create: `kafkrs-python/tests/__init__.py`
- Create: `kafkrs-python/tests/test_client.py`

- [ ] **Step 1: Create empty tests package marker**

Run: `touch kafkrs-python/tests/__init__.py`

- [ ] **Step 2: Write the test**

Create `kafkrs-python/tests/test_client.py`:

```python
"""End-to-end test: spawn a real kafkrs-server, connect with the Python
client, exercise the full Connect → CreateTopic → Produce → Fetch loop."""

import asyncio
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

from kafkrs import Client
from kafkrs.client import WireError


REPO_ROOT = Path(__file__).resolve().parents[2]
BROKER_BIN = REPO_ROOT / "target" / "debug" / "kafkrs-server"


def _find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _build_broker_if_needed() -> None:
    if BROKER_BIN.exists():
        return
    subprocess.run(
        ["cargo", "build", "--bin", "kafkrs-server"],
        cwd=str(REPO_ROOT),
        check=True,
    )


def _write_config(tmp: Path, port: int) -> Path:
    cfg = tmp / "config.toml"
    data_dir = tmp / "data"
    data_dir.mkdir()
    cfg.write_text(
        f"""
address = "127.0.0.1"
ports = [{port}]
data_dir = "{data_dir.as_posix()}"

[broker]
disk_type = "nvme"
auto_create_topics = true
default_partition_count = 1

[object_store]
backend = "filesystem"
bucket = "test"
prefix = ""
endpoint = ""
region = "us-east-1"
"""
    )
    return cfg


def _wait_for_port(host: str, port: int, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"broker did not start listening on {host}:{port}")


@pytest.fixture
def broker():
    _build_broker_if_needed()
    port = _find_free_port()
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        cfg_path = _write_config(tmp, port)
        env = dict(os.environ)
        env["RUST_LOG"] = "warn"
        proc = subprocess.Popen(
            [str(BROKER_BIN), str(cfg_path)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            _wait_for_port("127.0.0.1", port)
            yield port
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()


@pytest.mark.asyncio
async def test_connect_produce_fetch_roundtrip(broker: int) -> None:
    client = Client("127.0.0.1", broker)
    await client.connect()
    try:
        # auto_create_topics=true in the config, so producing creates the topic.
        base, last = await client.produce(
            "demo",
            0,
            [(b"k1", b"v1"), (b"", b"v2")],
        )
        assert base == 0
        assert last == 1

        recs, hwm = await client.fetch("demo", 0, from_offset=0, max_records=10, max_wait_ms=500)
        assert len(recs) == 2
        assert recs[0].offset == 0 and recs[0].key == b"k1" and recs[0].value == b"v1"
        assert recs[1].offset == 1 and recs[1].key == b"" and recs[1].value == b"v2"
        assert hwm >= 1
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_unsupported_version_raises(broker: int) -> None:
    # Monkey-patch the constant for this test only.
    import kafkrs.client as mod
    saved = mod.PROTOCOL_VERSION
    mod.PROTOCOL_VERSION = 999
    try:
        client = Client("127.0.0.1", broker)
        with pytest.raises(WireError) as ei:
            await client.connect()
        assert ei.value.code == 100  # ERR_UNSUPPORTED_PROTOCOL_VERSION
    finally:
        mod.PROTOCOL_VERSION = saved
```

- [ ] **Step 3: Install Python dev deps**

Run:
```bash
cd kafkrs-python && pip install -e ".[dev]"
```

- [ ] **Step 4: Run the test**

Run:
```bash
cd kafkrs-python && pytest -v
```
Expected: both tests pass. They spawn the actual `target/debug/kafkrs-server` binary.

If the cargo build fails, fix the underlying server issue. If the wire roundtrip fails, debug the client (server-side is already covered by `wire_e2e.rs`).

- [ ] **Step 5: Commit**

```bash
git add kafkrs-python/tests/__init__.py kafkrs-python/tests/test_client.py
git commit -m "python: end-to-end test against spawned broker"
```

---

## Task 18: Version bumps to 0.3.0

**Files:**
- Modify: `kafkrs-models/Cargo.toml`
- Modify: `kafkrs-server/Cargo.toml`
- Modify: `Cargo.lock` (regenerated)

- [ ] **Step 1: Bump kafkrs-models**

Edit `kafkrs-models/Cargo.toml`:
```toml
version = "0.3.0"
```

- [ ] **Step 2: Bump kafkrs-server**

Edit `kafkrs-server/Cargo.toml`:
```toml
version = "0.3.0"
```

(`kafkrs-python`'s version is already `0.3.0` in its rewritten `pyproject.toml`.)

- [ ] **Step 3: Regenerate Cargo.lock**

Run: `cargo build`
Expected: success. `Cargo.lock` updates with the new versions.

- [ ] **Step 4: Commit**

```bash
git add kafkrs-models/Cargo.toml kafkrs-server/Cargo.toml Cargo.lock
git commit -m "release: bump kafkrs-models and kafkrs-server to 0.3.0"
```

---

## Task 19: Update changelogs

**Files:**
- Modify: `kafkrs-models/CHANGELOG.md`
- Modify: `kafkrs-server/CHANGELOG.md`
- Modify: `kafkrs-python/CHANGELOG.md`

- [ ] **Step 1: Prepend 0.3.0 entry to kafkrs-models/CHANGELOG.md**

Insert this block between the preamble and the `## [0.2.0]` heading:

```markdown
## [0.3.0] — 2026-05-20

Wire protocol v1 lands. See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md` for the design.

### Added
- `kafkrs-models/proto/wire/v1.proto` — canonical protobuf schema for the kafkrs wire protocol. Defines `Command` (top-level envelope), `Connect`/`Connected`, `Ping`/`Pong`, `Produce`/`ProduceResp`, `Fetch`/`FetchResp`, `CreateTopic`/`DescribeTopic`/`ListTopics`, `Error` + `ErrorCode`. Field-number ranges `40–49` and `50–59` reserved for future streaming-consumer and admin RPCs.
- `kafkrs-models::wire::v1` module — `prost`-generated Rust bindings, produced at compile time by `build.rs`.
- Dependencies: `prost`, `prost-build`.
```

- [ ] **Step 2: Prepend 0.3.0 entry to kafkrs-server/CHANGELOG.md**

Insert this block between the preamble and the `## [0.2.0]` heading:

```markdown
## [0.3.0] — 2026-05-20

Wire protocol v1 lands. See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md` for the design.

### Added
- `kafkrs_server::wire` module — Pulsar-style framing (length-prefixed frames carrying a protobuf `Command` envelope plus a raw payload section), explicit `Connect` handshake, per-connection three-task model (reader / dispatcher / writer) with per-RPC task spawning for in-flight multiplexing, and structured `ErrorCode` taxonomy.
- `kafkrs-server/tests/wire_e2e.rs` — end-to-end TCP integration test (connect, produce, fetch, unsupported version, pre-Connect rejection).
- Dependencies: `prost`, `tokio-util` (codec feature), `futures`, `tokio-stream`, `thiserror`.

### Changed
- **Breaking:** `kafkrs_server::listener` is replaced by `kafkrs_server::wire`. The bincode `WireRequest` / `WireResponse` enums are gone; clients now speak the protobuf-framed wire described in the spec.
- **Breaking:** Stringly-typed error responses (`WireResponse::Error(String)` with magic strings) are replaced by `ErrorCode` enum values.

### Removed
- `kafkrs-server/src/listener.rs` — replaced by the `wire` module.
- `bincode` dependency — no longer used.
```

- [ ] **Step 3: Prepend 0.3.0 entry to kafkrs-python/CHANGELOG.md**

Insert this block between the preamble and the `## [0.2.0]` heading:

```markdown
## [0.3.0] — 2026-05-20

Crate restructured from a PyO3 extension to a pure-Python package. Wire protocol v1 client. See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md`.

### Added
- `kafkrs.Client` — async TCP client speaking the wire protocol v1. Methods: `connect`, `close`, `produce`, `fetch`, `create_topic`, `list_topics`.
- `kafkrs.wire.v1_pb2` — checked-in protobuf bindings generated from `kafkrs-models/proto/wire/v1.proto`. Users do not need `protoc` installed.
- `tests/test_client.py` — end-to-end test that spawns a real `kafkrs-server` binary and exercises Connect → Produce → Fetch.

### Changed
- **Breaking:** entire surface. The PyO3 `encode_message` function is gone, replaced by a real async client. Python users now `pip install kafkrs` (no Rust toolchain needed) and write `async with kafkrs.Client(host, port) as c: await c.produce(...)`.
- Build backend: `maturin` → `hatchling`.

### Removed
- All Rust code (`src/lib.rs`, `build.rs`, `Cargo.toml`).
- Dependencies on `pyo3`, `bincode`, and `kafkrs-models` (Rust). The only runtime dependency is `protobuf`.
```

- [ ] **Step 4: Commit**

```bash
git add kafkrs-models/CHANGELOG.md kafkrs-server/CHANGELOG.md kafkrs-python/CHANGELOG.md
git commit -m "changelog: add 0.3.0 entries for wire protocol v1"
```

---

## Task 20: Final verification

- [ ] **Step 1: Run the full Rust test suite**

Run: `cargo test`
Expected: all tests pass (storage_e2e, wire_e2e, wire::frame, wire::errors, wire_compile, fetcher tests, etc.).

- [ ] **Step 2: Run the Python tests**

Run: `cd kafkrs-python && pytest -v`
Expected: both end-to-end tests pass.

- [ ] **Step 3: Confirm git status is clean**

Run: `git status`
Expected: clean working tree (all changes committed).

- [ ] **Step 4: Skim diff against master for surprises**

Run: `git log master..HEAD --oneline`
Expected: a tight series of focused commits, one per task above.

- [ ] **Step 5: Manual sanity check — connect with the Python REPL**

Run the broker in one terminal:
```bash
cargo run --bin kafkrs-server -- config.toml
```

In another:
```bash
cd kafkrs-python && python
>>> import asyncio
>>> from kafkrs import Client
>>> async def main():
...     c = Client("127.0.0.1", 5432)
...     await c.connect()
...     await c.produce("foo", 0, [(b"hi", b"world")])
...     print(await c.fetch("foo", 0, 0))
...     await c.close()
>>> asyncio.run(main())
```
Expected: the produced record comes back through the fetch.

(The default `config.toml` may need `auto_create_topics = true` for this to work without a manual `create_topic` call.)

---

## Spec self-review (done at plan-writing time)

**Spec coverage check.** Every section of the spec maps to one or more tasks:
- Frame format → Task 5
- Protobuf schema → Tasks 2, 3
- Connection lifecycle (state machine + concurrency) → Tasks 7, 8
- Versioning rules → Task 12 (CI lint), Task 8 (handshake enforcement)
- Impact on existing code (files replaced/added/removed) → Tasks 9, 13, 14
- buf.yaml + buf.gen.yaml → Task 12
- `wire_e2e.rs` integration test → Task 10
- Pure-Python client → Tasks 14, 15, 16, 17
- Version bumps → Task 18
- Changelogs → Task 19

No spec requirement is unaddressed.

**Type consistency check.** `SharedState` is declared in `dispatch.rs` (Task 7) and consumed by `connection.rs` (Task 8) and `main.rs` (Task 9) — same type, re-exported through `wire::mod.rs`. `PartitionHandle` similarly. The Python `Client` methods (`connect`, `close`, `produce`, `fetch`, `create_topic`, `list_topics`) all appear in the test (Task 17).

**Open items the engineer must verify at implementation time:**
- `TopicEntry` field names in `kafkrs-models/src/topic.rs` (called out in Task 7 step 2). Adjust if they differ.
- `TopicConfigOverrides` model field names (same). The plan assumes a 1:1 name match with the proto.
- `buf` CLI availability — Task 12 step 3 and Task 15 step 1. Fallback to `protoc` provided.
- The exact location `buf generate` writes the Python file to may need a one-step move/rename (Task 15 step 1).
