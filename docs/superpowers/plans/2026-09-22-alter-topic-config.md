# AlterTopicConfig Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an `AlterTopicConfig` admin RPC that mutates a topic's per-topic overrides at runtime with partial-patch semantics, propagated to running actors via new `UpdateConfig` messages, plus a shared `TopicConfigOverrides::validate()` gate applied at `CreateTopic`, `AlterTopicConfig`, and broker startup.

**Architecture:** The `TopicRegistry` actor owns merge + validate + persist for a patch, then the wire handler resolves the merged overrides against `state.disk_type` and pushes `PwMsg::UpdateConfig(new_resolved)` / `UploaderMsg::UpdateConfig(new_resolved)` down each partition's existing mpsc channels, and swaps `PartitionHandle.cfg` under the partitions `RwLock`. Persistence-first: `topics.json` is fsync'd before any push, so a crash between persist and push self-heals on restart.

**Tech Stack:** Rust (tokio, prost, anyhow, serde), protobuf (buf lint/breaking), pytest (Python async client tests), cargo test (unit + integration).

**Spec:** `docs/superpowers/specs/2026-09-22-alter-topic-config-design.md`

## Global Constraints

- Version bump: all three crates from `0.6.1` to `0.6.2` in lockstep.
- Wire protocol version stays at v1 — additive proto changes only.
- No new modules or new dependencies.
- Persist-first ordering: `topics.json` is fsync'd BEFORE any `UpdateConfig` push. Reverse order is forbidden.
- Wire response returns the merged `TopicConfigOverrides` (stored form), not the fully-resolved config.
- Never mutate `partition_count` via this RPC — only fields on `TopicConfigOverrides` are alterable.
- Never run `git commit`; when a commit point is reached, stop and prompt the user.

---

### Task 1: Proto additions + regenerate Python bindings

**Files:**
- Modify: `kafkrs-models/proto/wire/v1.proto`
- Modify: `kafkrs-python/kafkrs/wire/v1_pb2.py` (regenerated from the proto)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `kafkrs_models::wire::v1::AlterTopicConfigRequest { topic: String, overrides: Option<TopicConfigOverrides> }`
  - `kafkrs_models::wire::v1::AlterTopicConfigResponse { overrides: Option<TopicConfigOverrides> }`
  - `command::Body::AlterTopicConfig(AlterTopicConfigRequest)` at field 52
  - `command::Body::AlterTopicConfigResp(AlterTopicConfigResponse)` at field 53
  - `ErrorCode::ErrInvalidConfig = 207`
  - Python: `v1_pb2.AlterTopicConfigRequest`, `v1_pb2.AlterTopicConfigResponse`, `v1_pb2.ERR_INVALID_CONFIG`.

- [ ] **Step 1: Edit `kafkrs-models/proto/wire/v1.proto`**

Change the `reserved` range in `Command` and add the two new `oneof` arms:

```proto
message Command {
  // Reserved for v1.5+ streaming-consumer RPCs:
  //   Subscribe / Subscribed / Flow / Message / Ack / Unsubscribe
  reserved 40 to 49;
  // Reserved for v1.5+ admin RPCs:
  //   AlterConfig / BrokerInfo
  reserved 54 to 59;

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

    // Control plane (30-39, 50-53)
    CreateTopicRequest     create_topic          = 30;
    CreateTopicResponse    create_topic_resp     = 31;
    DescribeTopicRequest   describe_topic        = 32;
    DescribeTopicResponse  describe_topic_resp   = 33;
    ListTopicsRequest      list_topics           = 34;
    ListTopicsResponse     list_topics_resp      = 35;
    DeleteTopicRequest     delete_topic          = 50;
    DeleteTopicResponse    delete_topic_resp     = 51;
    AlterTopicConfigRequest   alter_topic_config      = 52;
    AlterTopicConfigResponse  alter_topic_config_resp = 53;

    // Errors
    ErrorResponse          error                 = 99;
  }
}
```

Add the two new messages under the existing `DeleteTopicRequest` / `DeleteTopicResponse` block:

```proto
message AlterTopicConfigRequest {
  string topic = 1;
  TopicConfigOverrides overrides = 2;
}
message AlterTopicConfigResponse {
  TopicConfigOverrides overrides = 1;
}
```

Add the new error code to `enum ErrorCode` after `ERR_BROKER_NOT_READY = 205`:

```proto
  ERR_INVALID_CONFIG               = 207;
```

- [ ] **Step 2: Regenerate the Python bindings**

Run: `cd kafkrs-python && python3 -m grpc_tools.protoc -I ../kafkrs-models/proto --python_out=kafkrs/wire ../kafkrs-models/proto/wire/v1.proto`

If `grpc_tools` isn't available, use plain `protoc`: `protoc -I ../kafkrs-models/proto --python_out=kafkrs/wire ../kafkrs-models/proto/wire/v1.proto`. Prefer whichever tool was used for the last regeneration (check `git log -p kafkrs-python/kafkrs/wire/v1_pb2.py` if unsure).

Expected: `kafkrs-python/kafkrs/wire/v1_pb2.py` grows to include `AlterTopicConfigRequest`, `AlterTopicConfigResponse`, `ERR_INVALID_CONFIG`, and the `alter_topic_config` / `alter_topic_config_resp` fields on the `Command` oneof.

- [ ] **Step 3: Verify Rust bindings compile**

Run: `cargo build -p kafkrs-models`
Expected: PASS. The `prost-build` step regenerates `kafkrs-models/src/wire/v1.rs` (or the equivalent generated file under `OUT_DIR`) so `kafkrs_models::wire::v1::AlterTopicConfigRequest` is now a type.

- [ ] **Step 4: Verify buf lint passes**

Run: `buf lint`
Expected: PASS. If it fails on `RESERVED_MESSAGE_NO_DELETE` or `RESERVED_ENUM_NO_DELETE`, `buf.yaml` already carries those exemptions from the DeleteTopic branch — investigate before proceeding.

- [ ] **Step 5: Verify buf breaking passes vs `main`**

Run: `buf breaking --against '.git#branch=master,subdir=.'`
Expected: PASS. The oneof additions and error-code addition are additive; the shrunk reserved range is exempt via `RESERVED_MESSAGE_NO_DELETE` / `RESERVED_ENUM_NO_DELETE`.

- [ ] **Step 6: Commit point**

Prompt the user to commit. Suggested message: `wire: add AlterTopicConfig RPC + ERR_INVALID_CONFIG (proto only)`.

---

### Task 2: `TopicConfigOverrides::validate()` + `ConfigValidationError` in `kafkrs-models`

**Files:**
- Modify: `kafkrs-models/src/topic.rs`
- Test: `kafkrs-models/src/topic.rs` (the existing `#[cfg(test)] mod tests` block)

**Interfaces:**
- Consumes: `kafkrs_models::topic::TopicConfigOverrides` (already exists).
- Produces:
  - `pub enum ConfigValidationError { FieldOutOfRange { field: &'static str, value: String, reason: &'static str } }`
  - `impl std::fmt::Display for ConfigValidationError`
  - `impl std::error::Error for ConfigValidationError`
  - `impl TopicConfigOverrides { pub fn validate(&self) -> Result<(), ConfigValidationError> }`

- [ ] **Step 1: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests { ... }` block at the bottom of `kafkrs-models/src/topic.rs`:

```rust
    #[test]
    fn validate_accepts_defaults() {
        assert!(TopicConfigOverrides::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_segment_size_zero() {
        let o = TopicConfigOverrides {
            segment_size_bytes: Some(0),
            ..Default::default()
        };
        match o.validate() {
            Err(ConfigValidationError::FieldOutOfRange { field, .. }) => {
                assert_eq!(field, "segment_size_bytes");
            }
            other => panic!("expected FieldOutOfRange for segment_size_bytes, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_segment_seal_time_zero() {
        let o = TopicConfigOverrides {
            segment_seal_time_ms: Some(0),
            ..Default::default()
        };
        match o.validate() {
            Err(ConfigValidationError::FieldOutOfRange { field, .. }) => {
                assert_eq!(field, "segment_seal_time_ms");
            }
            other => panic!("expected FieldOutOfRange for segment_seal_time_ms, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_retention_ms_below_minus_one() {
        let o = TopicConfigOverrides {
            retention_ms: Some(-2),
            ..Default::default()
        };
        match o.validate() {
            Err(ConfigValidationError::FieldOutOfRange { field, .. }) => {
                assert_eq!(field, "retention_ms");
            }
            other => panic!("expected FieldOutOfRange for retention_ms, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_retention_bytes_below_minus_one() {
        let o = TopicConfigOverrides {
            retention_bytes: Some(-2),
            ..Default::default()
        };
        match o.validate() {
            Err(ConfigValidationError::FieldOutOfRange { field, .. }) => {
                assert_eq!(field, "retention_bytes");
            }
            other => panic!("expected FieldOutOfRange for retention_bytes, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_valid_full_config() {
        let o = TopicConfigOverrides {
            segment_size_bytes: Some(1024),
            segment_seal_time_ms: Some(1000),
            max_key_size_bytes: Some(512),
            max_value_size_bytes: Some(65_536),
            group_commit_time_ms: Some(10),
            group_commit_size_bytes: Some(1024),
            group_commit_record_count: Some(64),
            max_fetch_wait_ms: Some(100),
            retention_ms: Some(60_000),
            retention_bytes: Some(1_000_000_000),
        };
        assert!(o.validate().is_ok());
    }

    #[test]
    fn validate_accepts_retention_ms_negative_one_sentinel() {
        let o = TopicConfigOverrides {
            retention_ms: Some(-1),
            ..Default::default()
        };
        assert!(o.validate().is_ok());
    }

    #[test]
    fn validate_accepts_retention_bytes_negative_one_sentinel() {
        let o = TopicConfigOverrides {
            retention_bytes: Some(-1),
            ..Default::default()
        };
        assert!(o.validate().is_ok());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kafkrs-models topic::tests::validate_ --no-fail-fast`
Expected: FAIL with `no variant or associated item named 'FieldOutOfRange' found` (or `no method named 'validate' found for struct 'TopicConfigOverrides'`).

- [ ] **Step 3: Implement `ConfigValidationError` and `validate()`**

Add just above the existing `#[cfg(test)] mod tests {` in `kafkrs-models/src/topic.rs`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValidationError {
    FieldOutOfRange {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
}

impl std::fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigValidationError::FieldOutOfRange {
                field,
                value,
                reason,
            } => write!(f, "field `{field}` value `{value}` out of range: {reason}"),
        }
    }
}

impl std::error::Error for ConfigValidationError {}

impl TopicConfigOverrides {
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        if let Some(v) = self.segment_size_bytes {
            if v < 1 {
                return Err(ConfigValidationError::FieldOutOfRange {
                    field: "segment_size_bytes",
                    value: v.to_string(),
                    reason: "must be >= 1 (0 would seal every record then re-seal instantly)",
                });
            }
        }
        if let Some(v) = self.segment_seal_time_ms {
            if v < 1 {
                return Err(ConfigValidationError::FieldOutOfRange {
                    field: "segment_seal_time_ms",
                    value: v.to_string(),
                    reason: "must be >= 1 (0 defeats seal-by-time)",
                });
            }
        }
        if let Some(v) = self.retention_ms {
            if v < -1 {
                return Err(ConfigValidationError::FieldOutOfRange {
                    field: "retention_ms",
                    value: v.to_string(),
                    reason: "must be >= -1 (-1 = never; other negatives are meaningless)",
                });
            }
        }
        if let Some(v) = self.retention_bytes {
            if v < -1 {
                return Err(ConfigValidationError::FieldOutOfRange {
                    field: "retention_bytes",
                    value: v.to_string(),
                    reason: "must be >= -1 (-1 = no cap; other negatives are meaningless)",
                });
            }
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kafkrs-models topic::tests::validate_ --no-fail-fast`
Expected: all 8 new tests PASS.

- [ ] **Step 5: Run the full model test suite**

Run: `cargo test -p kafkrs-models`
Expected: all tests PASS (validate doesn't affect anything else).

- [ ] **Step 6: Commit point**

Prompt the user to commit. Suggested message: `models: TopicConfigOverrides::validate() with ConfigValidationError`.

---

### Task 3: `PwMsg::UpdateConfig` variant in `PartitionWriter`

**Files:**
- Modify: `kafkrs-server/src/partition_writer.rs`
- Test: `kafkrs-server/src/partition_writer.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `kafkrs_models::topic::ResolvedTopicConfig` (already imported).
- Produces: `PwMsg::UpdateConfig(ResolvedTopicConfig)`.

- [ ] **Step 1: Write the failing test**

Append inside the `#[cfg(test)] mod tests { ... }` block at the bottom of `kafkrs-server/src/partition_writer.rs`. Match the shape of the existing tests (which spawn a `PartitionWriter::new(...).run()`). Add this test:

```rust
    #[tokio::test]
    async fn update_config_replaces_cfg_on_next_batch() {
        // Build a writer with segment_seal_time_ms = 60_000 (default). Send
        // UpdateConfig with a shorter time, produce, and verify no seal
        // occurred (we can't easily observe cfg directly, so we assert
        // the writer stays alive and produces successfully after the
        // UpdateConfig message — a smoke test that the arm compiles and
        // runs).
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let cfg = ResolvedTopicConfig::resolve(
            &TopicConfigOverrides::default(),
            kafkrs_models::config::DiskType::Nvme,
        );
        let (utx, _urx) = mpsc::channel::<UploaderMsg>(16);
        let (pw_tx, pw_rx) = mpsc::channel(16);
        let (tail, _) = tokio::sync::broadcast::channel(16);
        let pw = PartitionWriter::new(
            dd,
            "t".into(),
            "01936a80-0000-7000-8000-000000000000".into(),
            0,
            cfg,
            0,
            vec![],
            pw_rx,
            utx,
            tail,
        )
        .unwrap();
        tokio::spawn(pw.run());

        let new_cfg = ResolvedTopicConfig::resolve(
            &TopicConfigOverrides {
                segment_seal_time_ms: Some(1),
                ..Default::default()
            },
            kafkrs_models::config::DiskType::Nvme,
        );
        pw_tx.send(PwMsg::UpdateConfig(new_cfg)).await.unwrap();

        // Produce after the update to prove the writer is still running.
        let (ack, arx) = oneshot::channel();
        pw_tx
            .send(PwMsg::Produce {
                records: vec![IncomingRecord {
                    schema_id: 0,
                    key: b"k".to_vec(),
                    value: b"v".to_vec(),
                    timestamp_ns: 0,
                }],
                ack,
            })
            .await
            .unwrap();
        assert_eq!(arx.await.unwrap(), 0);
    }
```

Add whatever `use` statements the file needs at the top of the tests module (`IncomingRecord`, `oneshot`, `TopicConfigOverrides`, etc. — check the existing tests for the pattern already in use).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-server partition_writer::tests::update_config_replaces_cfg_on_next_batch`
Expected: FAIL to compile with `no variant or associated item named 'UpdateConfig' found for enum 'PwMsg'`.

- [ ] **Step 3: Implement the variant + handler arm**

Edit the `PwMsg` enum at `kafkrs-server/src/partition_writer.rs:23`. Add a new variant at the end (before `Shutdown`):

```rust
    /// Live config replacement. FIFO ordering guarantees any pending
    /// Produce or seal has already been dequeued before this arrives;
    /// the next batch after this message uses the new cfg. In-flight
    /// batches complete under the old cfg because thresholds are
    /// captured on entry to the batch's group-commit / seal path.
    UpdateConfig(ResolvedTopicConfig),
```

Then in the `match msg { ... }` block inside `PartitionWriter::run` at `kafkrs-server/src/partition_writer.rs:157-190`, add a new arm before the `None` arm:

```rust
                        Some(PwMsg::UpdateConfig(new_cfg)) => {
                            self.cfg = new_cfg;
                        }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p kafkrs-server partition_writer::tests::update_config_replaces_cfg_on_next_batch`
Expected: PASS.

- [ ] **Step 5: Run the full partition_writer test suite**

Run: `cargo test -p kafkrs-server partition_writer::tests`
Expected: all existing tests still PASS.

- [ ] **Step 6: Commit point**

Prompt the user to commit. Suggested message: `partition_writer: PwMsg::UpdateConfig for live config replacement`.

---

### Task 4: `UploaderMsg::UpdateConfig` variant in `Uploader`

**Files:**
- Modify: `kafkrs-server/src/uploader.rs`
- Test: `kafkrs-server/src/uploader.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `kafkrs_models::topic::ResolvedTopicConfig` (already imported).
- Produces: `UploaderMsg::UpdateConfig(ResolvedTopicConfig)`.

- [ ] **Step 1: Write the failing test**

Append inside the `#[cfg(test)] mod tests { ... }` block at the bottom of `kafkrs-server/src/uploader.rs`. Match the existing test pattern for spawning an `Uploader::new(...).run()`:

```rust
    #[tokio::test]
    async fn update_config_replaces_cfg_for_next_retention_pass() {
        let dir = tempfile::tempdir().unwrap();
        let store = build_test_store(dir.path());
        let cfg = ResolvedTopicConfig::resolve(
            &TopicConfigOverrides::default(),
            kafkrs_models::config::DiskType::Nvme,
        );
        let (tx, rx) = mpsc::channel::<UploaderMsg>(16);
        let (dtx, _drx) = mpsc::channel(16);
        let up = Uploader::new(
            store,
            "".into(),
            "t".into(),
            "01936a80-0000-7000-8000-000000000000".into(),
            0,
            cfg,
            rx,
            dtx,
        );
        tokio::spawn(up.run());

        let new_cfg = ResolvedTopicConfig::resolve(
            &TopicConfigOverrides {
                retention_ms: Some(500),
                ..Default::default()
            },
            kafkrs_models::config::DiskType::Nvme,
        );
        tx.send(UploaderMsg::UpdateConfig(new_cfg)).await.unwrap();

        // Prove the actor is still alive and processing messages after the
        // update by sending a RetentionKick and letting it drain without
        // panicking. There's no seg to evict; the kick just runs and returns.
        tx.send(UploaderMsg::RetentionKick).await.unwrap();

        // Cleanly shut down.
        let (ack, arx) = oneshot::channel();
        tx.send(UploaderMsg::Shutdown { ack }).await.unwrap();
        arx.await.unwrap();
    }
```

Reuse whatever `build_test_store` helper (or its equivalent) the existing tests use; check the test module for the pattern already in place. If none exists, inline the equivalent of the `store(&dir.path())` helper from `topic_registry.rs::tests`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-server uploader::tests::update_config_replaces_cfg_for_next_retention_pass`
Expected: FAIL to compile with `no variant or associated item named 'UpdateConfig' found for enum 'UploaderMsg'`.

- [ ] **Step 3: Implement the variant + handler arm**

Edit the `UploaderMsg` enum at `kafkrs-server/src/uploader.rs:28`. Add a new variant at the end (after `Shutdown`, or before it — position doesn't matter):

```rust
    /// Live config replacement. FIFO ordering: any pending `Upload` or
    /// `RetentionKick` is drained before this arrives, so the update
    /// affects only subsequent operations. `retention_pass` reads
    /// `self.cfg.retention_ms` / `self.cfg.retention_bytes` on each call,
    /// so the next kick or upload picks up the new values.
    UpdateConfig(ResolvedTopicConfig),
```

Then in the `match msg { ... }` block inside `Uploader::run` at `kafkrs-server/src/uploader.rs:87-146`, add a new arm before the `UploaderMsg::Shutdown` arm:

```rust
                UploaderMsg::UpdateConfig(new_cfg) => {
                    self.cfg = new_cfg;
                }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p kafkrs-server uploader::tests::update_config_replaces_cfg_for_next_retention_pass`
Expected: PASS.

- [ ] **Step 5: Run the full uploader test suite**

Run: `cargo test -p kafkrs-server uploader::tests`
Expected: all existing tests still PASS.

- [ ] **Step 6: Commit point**

Prompt the user to commit. Suggested message: `uploader: UploaderMsg::UpdateConfig for live config replacement`.

---

### Task 5: `RegistryMsg::Alter` + `RegistryError::InvalidConfig` + `Create` validation

**Files:**
- Modify: `kafkrs-server/src/topic_registry.rs`
- Test: `kafkrs-server/src/topic_registry.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes:
  - `TopicConfigOverrides::validate()` from Task 2.
  - `kafkrs_models::topic::TopicConfigOverrides` (already imported).
- Produces:
  - `RegistryMsg::Alter { name: String, patch: TopicConfigOverrides, reply: oneshot::Sender<Result<TopicConfigOverrides, RegistryError>> }`
  - `RegistryError::InvalidConfig(String)`

- [ ] **Step 1: Write the failing tests**

Append these tests inside the `#[cfg(test)] mod tests { ... }` block at the bottom of `kafkrs-server/src/topic_registry.rs`:

```rust
    #[tokio::test]
    async fn create_rejects_invalid_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd, DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r, rr) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "bad".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides {
                segment_size_bytes: Some(0),
                ..Default::default()
            },
            reply: r,
        })
        .await
        .unwrap();
        match rr.await.unwrap() {
            Err(RegistryError::InvalidConfig(msg)) => {
                assert!(msg.contains("segment_size_bytes"), "got: {msg}");
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn alter_unknown_topic_returns_unknown_topic() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd, DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r, rr) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "nope".into(),
            patch: TopicConfigOverrides::default(),
            reply: r,
        })
        .await
        .unwrap();
        assert_eq!(rr.await.unwrap().unwrap_err(), RegistryError::UnknownTopic);
    }

    #[tokio::test]
    async fn alter_invalid_patch_returns_invalid_config_and_leaves_state_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        // Create with valid config.
        let (r1, rr1) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "t".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides::default(),
            reply: r1,
        })
        .await
        .unwrap();
        rr1.await.unwrap().unwrap();

        // Alter with an invalid patch.
        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "t".into(),
            patch: TopicConfigOverrides {
                segment_size_bytes: Some(0),
                ..Default::default()
            },
            reply: r2,
        })
        .await
        .unwrap();
        match rr2.await.unwrap() {
            Err(RegistryError::InvalidConfig(msg)) => {
                assert!(msg.contains("segment_size_bytes"), "got: {msg}");
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }

        // Re-read topics.json and confirm on-disk config is untouched.
        let raw = std::fs::read_to_string(std::path::Path::new(&dd).join("topics.json")).unwrap();
        let parsed: TopicRegistryFile = serde_json::from_str(&raw).unwrap();
        let entry = parsed.topics.iter().find(|t| t.name == "t").unwrap();
        assert_eq!(entry.config.segment_size_bytes, None);
    }

    #[tokio::test]
    async fn alter_valid_patch_persists_and_returns_merged_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r1, rr1) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "t".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides::default(),
            reply: r1,
        })
        .await
        .unwrap();
        rr1.await.unwrap().unwrap();

        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "t".into(),
            patch: TopicConfigOverrides {
                retention_ms: Some(60_000),
                ..Default::default()
            },
            reply: r2,
        })
        .await
        .unwrap();
        let merged = rr2.await.unwrap().unwrap();
        assert_eq!(merged.retention_ms, Some(60_000));
        assert_eq!(merged.segment_size_bytes, None);

        let raw = std::fs::read_to_string(std::path::Path::new(&dd).join("topics.json")).unwrap();
        let parsed: TopicRegistryFile = serde_json::from_str(&raw).unwrap();
        let entry = parsed.topics.iter().find(|t| t.name == "t").unwrap();
        assert_eq!(entry.config.retention_ms, Some(60_000));
    }

    #[tokio::test]
    async fn alter_second_patch_composes_on_first() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r1, rr1) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "t".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides::default(),
            reply: r1,
        })
        .await
        .unwrap();
        rr1.await.unwrap().unwrap();

        // First patch: only retention_ms.
        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "t".into(),
            patch: TopicConfigOverrides {
                retention_ms: Some(1000),
                ..Default::default()
            },
            reply: r2,
        })
        .await
        .unwrap();
        rr2.await.unwrap().unwrap();

        // Second patch: only max_fetch_wait_ms.
        let (r3, rr3) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "t".into(),
            patch: TopicConfigOverrides {
                max_fetch_wait_ms: Some(200),
                ..Default::default()
            },
            reply: r3,
        })
        .await
        .unwrap();
        let merged = rr3.await.unwrap().unwrap();

        // Both fields are present in the merged result.
        assert_eq!(merged.retention_ms, Some(1000));
        assert_eq!(merged.max_fetch_wait_ms, Some(200));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kafkrs-server topic_registry::tests::alter_ topic_registry::tests::create_rejects_invalid_overrides`
Expected: FAIL to compile with `no variant or associated item named 'Alter' found for enum 'RegistryMsg'` (and similar for `InvalidConfig`).

- [ ] **Step 3: Add the `RegistryError::InvalidConfig` variant**

In `kafkrs-server/src/topic_registry.rs`, extend the `RegistryError` enum at line 54:

```rust
#[derive(Debug, PartialEq)]
pub enum RegistryError {
    AlreadyExists,
    Io(String),
    UnknownTopic,
    InvalidConfig(String),
}
```

- [ ] **Step 4: Add the `RegistryMsg::Alter` variant**

Extend the `RegistryMsg` enum (in the same file at line 17) with a new variant. Add it just before the closing `}`:

```rust
    /// Merge a partial-patch of `TopicConfigOverrides` onto the topic's
    /// current config, validate the merged result, and persist to
    /// `topics.json` atomically. Returns the merged overrides so the
    /// caller can resolve them + push `UpdateConfig` messages to running
    /// actors. Persist-first: `topics.json` is fsync'd before the reply
    /// fires (spec invariant 1).
    Alter {
        name: String,
        patch: TopicConfigOverrides,
        reply: oneshot::Sender<Result<TopicConfigOverrides, RegistryError>>,
    },
```

- [ ] **Step 5: Add the `alter` method and wire it into `run`**

Add a new `async fn alter` method to `impl TopicRegistry` (place it next to `create` at `kafkrs-server/src/topic_registry.rs:169`):

```rust
    async fn alter(
        &mut self,
        name: &str,
        patch: TopicConfigOverrides,
    ) -> Result<TopicConfigOverrides, RegistryError> {
        let Some(entry) = self.topics.get(name).cloned() else {
            return Err(RegistryError::UnknownTopic);
        };

        // Merge patch onto current config.
        let mut merged = entry.config.clone();
        if let Some(v) = patch.segment_size_bytes {
            merged.segment_size_bytes = Some(v);
        }
        if let Some(v) = patch.segment_seal_time_ms {
            merged.segment_seal_time_ms = Some(v);
        }
        if let Some(v) = patch.max_key_size_bytes {
            merged.max_key_size_bytes = Some(v);
        }
        if let Some(v) = patch.max_value_size_bytes {
            merged.max_value_size_bytes = Some(v);
        }
        if let Some(v) = patch.group_commit_time_ms {
            merged.group_commit_time_ms = Some(v);
        }
        if let Some(v) = patch.group_commit_size_bytes {
            merged.group_commit_size_bytes = Some(v);
        }
        if let Some(v) = patch.group_commit_record_count {
            merged.group_commit_record_count = Some(v);
        }
        if let Some(v) = patch.max_fetch_wait_ms {
            merged.max_fetch_wait_ms = Some(v);
        }
        if let Some(v) = patch.retention_ms {
            merged.retention_ms = Some(v);
        }
        if let Some(v) = patch.retention_bytes {
            merged.retention_bytes = Some(v);
        }

        // Validate BEFORE touching in-memory state.
        if let Err(e) = merged.validate() {
            return Err(RegistryError::InvalidConfig(e.to_string()));
        }

        // Swap in-memory config, then persist. On IO failure, roll back so
        // topics.json and in-memory state stay consistent.
        let prev_config = entry.config.clone();
        let mut new_entry = entry;
        new_entry.config = merged.clone();
        self.topics.insert(name.to_string(), new_entry);

        let next = TopicRegistryFile {
            topics: self.topics.values().cloned().collect(),
        };
        if let Err(e) = atomic_write_registry(&self.data_dir, &next) {
            // Roll back the in-memory swap.
            if let Some(t) = self.topics.get_mut(name) {
                t.config = prev_config;
            }
            return Err(RegistryError::Io(e.to_string()));
        }

        Ok(merged)
    }
```

Then add an arm to the `match msg { ... }` block in `run` at `kafkrs-server/src/topic_registry.rs:110-147`, next to `Delete`:

```rust
                RegistryMsg::Alter { name, patch, reply } => {
                    let _ = reply.send(self.alter(&name, patch).await);
                }
```

- [ ] **Step 6: Add validation to the `Create` handler**

In `impl TopicRegistry::create` at `kafkrs-server/src/topic_registry.rs:169`, insert a validation check as the very first line inside the function body (before the `contains_key` check):

```rust
        if let Err(e) = overrides.validate() {
            return Err(RegistryError::InvalidConfig(e.to_string()));
        }
```

- [ ] **Step 7: Add necessary imports**

Ensure `kafkrs-server/src/topic_registry.rs` imports `TopicConfigOverrides` from `kafkrs_models::topic` (already imported on line 5). No new imports needed for the model change; the `validate()` method is inherent so it's automatically in scope wherever `TopicConfigOverrides` is imported.

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p kafkrs-server topic_registry::tests`
Expected: all existing tests still PASS, plus the 5 new tests PASS.

- [ ] **Step 9: Commit point**

Prompt the user to commit. Suggested message: `topic_registry: RegistryMsg::Alter + InvalidConfig + Create validation`.

---

### Task 6: Startup validation gate in `TopicRegistry::load`

**Files:**
- Modify: `kafkrs-server/src/topic_registry.rs`
- Test: `kafkrs-server/src/topic_registry.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `TopicConfigOverrides::validate()` from Task 2.
- Produces: startup-time panic naming the offending topic + field when `topics.json` contains invalid overrides.

- [ ] **Step 1: Write the failing test**

Append inside the `#[cfg(test)] mod tests { ... }` block:

```rust
    #[tokio::test]
    #[should_panic(expected = "segment_size_bytes")]
    async fn load_panics_on_invalid_topics_json() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();

        // Pre-write a topics.json with an invalid config.
        let bad = TopicRegistryFile {
            topics: vec![TopicEntry {
                name: "corrupt".into(),
                uuid: "01936a80-0000-7000-8000-000000000000".into(),
                partition_count: 1,
                created_at_ns: 1,
                config: TopicConfigOverrides {
                    segment_size_bytes: Some(0),
                    ..Default::default()
                },
            }],
        };
        std::fs::write(
            std::path::Path::new(&dd).join("topics.json"),
            serde_json::to_vec_pretty(&bad).unwrap(),
        )
        .unwrap();

        let (_tx, rx) = mpsc::channel(1);
        // This should panic with a message naming the offending field.
        let _ = TopicRegistry::load(dd, DiskType::Nvme, store(dir.path()), "".into(), rx);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-server topic_registry::tests::load_panics_on_invalid_topics_json`
Expected: FAIL — test does not panic (the current `load` accepts anything).

- [ ] **Step 3: Add the startup validation gate**

Edit `TopicRegistry::load` at `kafkrs-server/src/topic_registry.rs:74`. After the `serde_json::from_slice` call that produces `file`, but before the collect into `HashMap`, walk the entries and validate:

```rust
        let file: TopicRegistryFile = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path)?)?
        } else {
            TopicRegistryFile::default()
        };
        for t in &file.topics {
            if let Err(e) = t.config.validate() {
                panic!(
                    "topics.json entry `{name}` has invalid config: {e}",
                    name = t.name,
                );
            }
        }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p kafkrs-server topic_registry::tests::load_panics_on_invalid_topics_json`
Expected: PASS (the test uses `#[should_panic(expected = "segment_size_bytes")]`).

- [ ] **Step 5: Run the full topic_registry test suite**

Run: `cargo test -p kafkrs-server topic_registry::tests`
Expected: all tests PASS.

- [ ] **Step 6: Commit point**

Prompt the user to commit. Suggested message: `topic_registry: fail-fast at startup on invalid topics.json config`.

---

### Task 7: Map `RegistryError::InvalidConfig` → `ErrorCode::ErrInvalidConfig`

**Files:**
- Modify: `kafkrs-server/src/wire/errors.rs`
- Test: `kafkrs-server/src/wire/errors.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes:
  - `RegistryError::InvalidConfig(String)` from Task 5.
  - `ErrorCode::ErrInvalidConfig` from Task 1 (the prost-generated Rust name for `ERR_INVALID_CONFIG`).
- Produces: `registry_error_code(&RegistryError::InvalidConfig(_)) → ErrorCode::ErrInvalidConfig`.

- [ ] **Step 1: Write the failing test**

Append inside the existing `#[cfg(test)] mod tests { ... }` block in `kafkrs-server/src/wire/errors.rs`:

```rust
    #[test]
    fn registry_invalid_config_maps_to_err_invalid_config() {
        assert_eq!(
            registry_error_code(&RegistryError::InvalidConfig("segment_size_bytes = 0".into())),
            ErrorCode::ErrInvalidConfig,
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kafkrs-server wire::errors::tests::registry_invalid_config_maps_to_err_invalid_config`
Expected: FAIL — the `match` in `registry_error_code` is not exhaustive over the new variant, so compilation fails with `non-exhaustive patterns: '&RegistryError::InvalidConfig(_)' not covered`.

- [ ] **Step 3: Extend the mapping**

Edit `registry_error_code` in `kafkrs-server/src/wire/errors.rs:27-33`:

```rust
pub fn registry_error_code(e: &RegistryError) -> ErrorCode {
    match e {
        RegistryError::AlreadyExists => ErrorCode::ErrTopicAlreadyExists,
        RegistryError::Io(_) => ErrorCode::ErrInternal,
        RegistryError::UnknownTopic => ErrorCode::ErrUnknownTopic,
        RegistryError::InvalidConfig(_) => ErrorCode::ErrInvalidConfig,
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p kafkrs-server wire::errors::tests`
Expected: all tests PASS.

- [ ] **Step 5: Commit point**

Prompt the user to commit. Suggested message: `wire/errors: map RegistryError::InvalidConfig to ErrInvalidConfig`.

---

### Task 8: `handle_alter_topic_config` in `wire/dispatch.rs`

**Files:**
- Modify: `kafkrs-server/src/wire/dispatch.rs`

**Interfaces:**
- Consumes:
  - `RegistryMsg::Alter` from Task 5.
  - `PwMsg::UpdateConfig` from Task 3.
  - `UploaderMsg::UpdateConfig` from Task 4.
  - `wire_overrides_to_model` / `model_overrides_to_wire` (existing helpers at the bottom of `dispatch.rs`).
  - `registry_error_code` from Task 7.
- Produces: `pub async fn handle_alter_topic_config(correlation_id: u64, state: &SharedState, req: kafkrs_models::wire::v1::AlterTopicConfigRequest) -> Frame`.

- [ ] **Step 1: Add the handler function**

Add this function at the bottom of `kafkrs-server/src/wire/dispatch.rs`, just above the `fn wire_overrides_to_model` block:

```rust
pub async fn handle_alter_topic_config(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::AlterTopicConfigRequest,
) -> Frame {
    let topic = req.topic.clone();
    let patch = wire_overrides_to_model(req.overrides.unwrap_or_default());

    // Registry does merge + validate + persist. On success it returns the
    // fully-merged overrides so we can resolve them for the actors.
    let (tx, rx) = oneshot::channel::<Result<TopicConfigOverridesModel, RegistryError>>();
    if state
        .registry
        .send(RegistryMsg::Alter {
            name: topic.clone(),
            patch,
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
    let merged = match rx.await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            return Frame {
                command: make_error(correlation_id, registry_error_code(&e), format!("{e:?}")),
                payload: Bytes::new(),
            };
        }
        Err(_) => {
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
        }
    };

    // Resolve merged overrides against the broker's disk profile so the
    // actors can just swap their `cfg` field.
    let new_cfg = ResolvedTopicConfig::resolve(&merged, state.disk_type.clone());

    // Snapshot the handles under a read lock (do NOT hold across the mpsc
    // sends). A DeleteTopic that races removes the entry; the send fails
    // silently. An auto-create that races spawns actors from the
    // already-persisted new config in topics.json, so they start correct.
    let handles: Vec<PartitionHandle> = {
        let guard = state.partitions.read().await;
        guard
            .iter()
            .filter_map(|((t, _p), h)| (t == &topic).then(|| h.clone()))
            .collect()
    };
    for h in &handles {
        let _ = h.pw_tx.send(PwMsg::UpdateConfig(new_cfg)).await;
        let _ = h
            .uploader_tx
            .send(crate::uploader::UploaderMsg::UpdateConfig(new_cfg))
            .await;
    }

    // Now take a write lock and update PartitionHandle.cfg on every
    // matching entry. A concurrent auto-create/delete that mutates the
    // map between the read snapshot and the write scan is fine — we
    // just walk whatever's present.
    {
        let mut guard = state.partitions.write().await;
        for ((t, _p), h) in guard.iter_mut() {
            if t == &topic {
                h.cfg = new_cfg;
            }
        }
    }

    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::AlterTopicConfigResp(
                kafkrs_models::wire::v1::AlterTopicConfigResponse {
                    overrides: Some(model_overrides_to_wire(merged)),
                },
            )),
        },
        payload: Bytes::new(),
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p kafkrs-server`
Expected: PASS. If a name (`Body::AlterTopicConfigResp`, `AlterTopicConfigResponse`) doesn't resolve, check that Task 1 regenerated the prost bindings; run `cargo clean -p kafkrs-models && cargo build -p kafkrs-server` if needed.

- [ ] **Step 3: Commit point**

Prompt the user to commit. Suggested message: `wire/dispatch: handle_alter_topic_config`.

---

### Task 9: Wire `handle_alter_topic_config` into the dispatcher

**Files:**
- Modify: `kafkrs-server/src/wire/connection.rs`

**Interfaces:**
- Consumes: `handle_alter_topic_config` from Task 8; `Body::AlterTopicConfig` from Task 1.
- Produces: `dispatch_one` routing `Body::AlterTopicConfig` → `handle_alter_topic_config`, and `WIRE_RPC_REQUESTS` label `"alter_topic_config"`.

- [ ] **Step 1: Add the handler to the import list**

Edit the `use` at the top of `kafkrs-server/src/wire/connection.rs:15-18`:

```rust
use crate::wire::dispatch::{
    handle_alter_topic_config, handle_connected, handle_create_topic, handle_delete_topic,
    handle_describe_topic, handle_fetch, handle_list_topics, handle_ping, handle_produce,
    SharedState, PROTOCOL_VERSION,
};
```

- [ ] **Step 2: Add the RPC label**

Edit `dispatch_one` at `kafkrs-server/src/wire/connection.rs:265-274`. Extend the `__rpc` match:

```rust
    let __rpc = match &body {
        Body::Ping(_) => "ping",
        Body::Produce(_) => "produce",
        Body::Fetch(_) => "fetch",
        Body::CreateTopic(_) => "create_topic",
        Body::DescribeTopic(_) => "describe_topic",
        Body::ListTopics(_) => "list_topics",
        Body::DeleteTopic(_) => "delete_topic",
        Body::AlterTopicConfig(_) => "alter_topic_config",
        _ => "unknown",
    };
```

- [ ] **Step 3: Add the dispatch arm**

Extend the `match body { ... }` at `kafkrs-server/src/wire/connection.rs:275-311`. Add a new arm alongside `Body::DeleteTopic`:

```rust
        Body::AlterTopicConfig(req) => handle_alter_topic_config(correlation_id, state, req).await,
```

Add `Body::AlterTopicConfigResp(_)` to the exhaustive "never a request" catch-all pattern list so pattern-match exhaustiveness is preserved:

```rust
        Body::Connect(_)
        | Body::Connected(_)
        | Body::Pong(_)
        | Body::ProduceResp(_)
        | Body::FetchResp(_)
        | Body::CreateTopicResp(_)
        | Body::DescribeTopicResp(_)
        | Body::ListTopicsResp(_)
        | Body::DeleteTopicResp(_)
        | Body::AlterTopicConfigResp(_)
        | Body::Error(_) => Frame { ... }
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build -p kafkrs-server`
Expected: PASS.

- [ ] **Step 5: Run the full server test suite**

Run: `cargo test -p kafkrs-server`
Expected: all tests PASS (integration tests still cover the pre-existing RPCs).

- [ ] **Step 6: Commit point**

Prompt the user to commit. Suggested message: `wire/connection: dispatch AlterTopicConfig`.

---

### Task 10: Integration tests in `wire_e2e.rs`

**Files:**
- Test: `kafkrs-server/tests/wire_e2e.rs`

**Interfaces:**
- Consumes: everything from Tasks 1-9. Uses the existing `setup_broker` / `encode` / `read_frame` helpers already present in `wire_e2e.rs`.
- Produces: four new integration tests.

- [ ] **Step 1: Write the failing tests**

Append these tests at the end of `kafkrs-server/tests/wire_e2e.rs`. Each test's structure mirrors the existing DeleteTopic e2e tests. If any test needs Connect + CreateTopic + Alter, use the helpers already in the file (`setup_broker`, `encode`, `read_frame`, and any Connect/Create helpers already defined). If you're unsure of the exact helper names, grep the file first: `grep -n 'async fn\|fn ' kafkrs-server/tests/wire_e2e.rs`.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alter_topic_config_unknown_topic_returns_err_unknown_topic() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect handshake.
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "e2e".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // Alter unknown topic.
    let alter = Command {
        correlation_id: 2,
        body: Some(Body::AlterTopicConfig(
            kafkrs_models::wire::v1::AlterTopicConfigRequest {
                topic: "no-such-topic".into(),
                overrides: Some(kafkrs_models::wire::v1::TopicConfigOverrides::default()),
            },
        )),
    };
    sock.write_all(&encode(&alter, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::Error(e)) => {
            assert_eq!(e.code, ErrorCode::ErrUnknownTopic as i32);
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alter_topic_config_rejects_invalid_and_leaves_state_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "e2e".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // setup_broker inserts a topic "t" pre-wired; alter with invalid patch.
    let alter = Command {
        correlation_id: 2,
        body: Some(Body::AlterTopicConfig(
            kafkrs_models::wire::v1::AlterTopicConfigRequest {
                topic: "t".into(),
                overrides: Some(kafkrs_models::wire::v1::TopicConfigOverrides {
                    segment_size_bytes: Some(0),
                    ..Default::default()
                }),
            },
        )),
    };
    sock.write_all(&encode(&alter, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::Error(e)) => {
            assert_eq!(e.code, ErrorCode::ErrInvalidConfig as i32);
            assert!(e.message.contains("segment_size_bytes"), "{}", e.message);
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alter_topic_config_respects_partial_patch() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "e2e".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // Alter only retention_ms.
    let alter = Command {
        correlation_id: 2,
        body: Some(Body::AlterTopicConfig(
            kafkrs_models::wire::v1::AlterTopicConfigRequest {
                topic: "t".into(),
                overrides: Some(kafkrs_models::wire::v1::TopicConfigOverrides {
                    retention_ms: Some(60_000),
                    ..Default::default()
                }),
            },
        )),
    };
    sock.write_all(&encode(&alter, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::AlterTopicConfigResp(r)) => {
            let o = r.overrides.expect("overrides present");
            assert_eq!(o.retention_ms, Some(60_000));
            // Every other field remains as setup_broker left them:
            // group_commit_record_count = Some(1) from setup_broker's overrides,
            // others None.
            assert_eq!(o.group_commit_record_count, Some(1));
            assert_eq!(o.segment_size_bytes, None);
            assert_eq!(o.max_fetch_wait_ms, None);
        }
        other => panic!("expected AlterTopicConfigResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alter_topic_config_updates_uploader_retention() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "e2e".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // Produce one record so we have a segment to evict later.
    let produce = Command {
        correlation_id: 2,
        body: Some(Body::Produce(ProduceRequest {
            topic: "t".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 1,
                value_len: 1,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce, b"kv")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // Give the uploader time to seal + upload (group_commit_record_count = 1
    // in setup_broker forces a seal per record).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Alter to a very tight retention_ms.
    let alter = Command {
        correlation_id: 3,
        body: Some(Body::AlterTopicConfig(
            kafkrs_models::wire::v1::AlterTopicConfigRequest {
                topic: "t".into(),
                overrides: Some(kafkrs_models::wire::v1::TopicConfigOverrides {
                    retention_ms: Some(1),
                    group_commit_record_count: Some(1),
                    ..Default::default()
                }),
            },
        )),
    };
    sock.write_all(&encode(&alter, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::AlterTopicConfigResp(_))));

    // Wait for the next retention pass to fire. A subsequent produce will
    // trigger a retention_pass in the Uploader (retention runs
    // opportunistically after every successful upload per the retention
    // spec).
    let produce2 = Command {
        correlation_id: 4,
        body: Some(Body::Produce(ProduceRequest {
            topic: "t".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 1,
                value_len: 1,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    // Sleep past the new watermark before the second produce so the first
    // segment's records are older than retention_ms.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    sock.write_all(&encode(&produce2, b"kv")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // Give the retention pass a moment to complete.
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Fetch from offset 0 should now fail with ErrOffsetOutOfRange
    // because the old segment has been evicted.
    let fetch = Command {
        correlation_id: 5,
        body: Some(Body::Fetch(kafkrs_models::wire::v1::FetchRequest {
            topic: "t".into(),
            partition: 0,
            from_offset: 0,
            max_records: 10,
            max_wait_ms: 0,
        })),
    };
    sock.write_all(&encode(&fetch, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::Error(e)) => {
            assert_eq!(
                e.code,
                ErrorCode::ErrOffsetOutOfRange as i32,
                "expected ErrOffsetOutOfRange, got {} ({})",
                e.code,
                e.message
            );
        }
        Some(Body::FetchResp(r)) => {
            // Some execution schedules may complete retention before the
            // fetch. If so, records should be empty and hwm >= 1.
            assert!(
                r.records.is_empty(),
                "expected no records at offset 0 after retention, got {:?}",
                r.records
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }
}
```

Note: the load-bearing behavioural test's exact timing may need small tweaks depending on how quickly the retention pass runs in CI; if it flakes, extend the sleeps by 500ms increments. Do NOT weaken the invariant it's asserting — a passing test that doesn't observe the eviction under the new retention isn't useful.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kafkrs-server --test wire_e2e alter_topic_config`
Expected: FAIL to compile if any AlterTopicConfig* Rust type name doesn't resolve. If compile succeeds, expected: FAIL at runtime because the dispatcher would need the handler wired (Task 9 already covers this — if failing here, revisit Task 9's imports/dispatch arm).

- [ ] **Step 3: Ensure they pass**

Run: `cargo test -p kafkrs-server --test wire_e2e alter_topic_config`
Expected: all 4 tests PASS.

- [ ] **Step 4: Run every server test to catch cross-suite regressions**

Run: `cargo test -p kafkrs-server`
Expected: PASS.

- [ ] **Step 5: Commit point**

Prompt the user to commit. Suggested message: `wire_e2e: integration tests for AlterTopicConfig`.

---

### Task 11: Python client `alter_topic_config` + tests

**Files:**
- Modify: `kafkrs-python/kafkrs/client.py`
- Test: `kafkrs-python/tests/test_client.py`

**Interfaces:**
- Consumes: `v1_pb2.AlterTopicConfigRequest`, `v1_pb2.AlterTopicConfigResponse`, `v1_pb2.ERR_INVALID_CONFIG` (from Task 1).
- Produces: `Client.alter_topic_config(topic: str, overrides: v1_pb2.TopicConfigOverrides) -> v1_pb2.TopicConfigOverrides`.

- [ ] **Step 1: Write the failing tests**

Append to `kafkrs-python/tests/test_client.py`. Match the shape of the existing `test_create_topic_*` tests — they should already provide a helper for spinning up a broker fixture.

```python
async def test_alter_topic_config_round_trip(client_and_topic):
    client, topic = client_and_topic
    overrides = v1_pb2.TopicConfigOverrides()
    overrides.retention_ms = 60_000
    resp = await client.alter_topic_config(topic, overrides)
    assert resp.retention_ms == 60_000


async def test_alter_topic_config_unknown_raises(client):
    overrides = v1_pb2.TopicConfigOverrides()
    with pytest.raises(WireError) as excinfo:
        await client.alter_topic_config("does-not-exist", overrides)
    assert excinfo.value.code == v1_pb2.ERR_UNKNOWN_TOPIC


async def test_alter_topic_config_invalid_value_raises(client_and_topic):
    client, topic = client_and_topic
    overrides = v1_pb2.TopicConfigOverrides()
    overrides.segment_size_bytes = 0
    with pytest.raises(WireError) as excinfo:
        await client.alter_topic_config(topic, overrides)
    assert excinfo.value.code == v1_pb2.ERR_INVALID_CONFIG
```

If the existing tests don't have a `client_and_topic` fixture, use whatever fixture the DeleteTopic tests use (they follow the same shape: connect + create + call). Grep for `@pytest.fixture` in the test file to find the pattern.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd kafkrs-python && python3 -m pytest tests/test_client.py::test_alter_topic_config_round_trip -v`
Expected: FAIL with `AttributeError: 'Client' object has no attribute 'alter_topic_config'`.

- [ ] **Step 3: Implement the client method**

Add to `kafkrs-python/kafkrs/client.py`, just after the `delete_topic` method (around line 190):

```python
    async def alter_topic_config(
        self,
        topic: str,
        overrides: v1_pb2.TopicConfigOverrides,
    ) -> v1_pb2.TopicConfigOverrides:
        """Alter a topic's config using partial-patch semantics.

        Fields set to a non-default value on ``overrides`` overwrite the
        corresponding field in the stored config; unset fields are unchanged.
        Returns the merged overrides post-patch.
        """
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        cmd.alter_topic_config.topic = topic
        cmd.alter_topic_config.overrides.CopyFrom(overrides)
        resp, _ = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "alter_topic_config_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")
        return resp.alter_topic_config_resp.overrides
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd kafkrs-python && python3 -m pytest tests/test_client.py::test_alter_topic_config_round_trip tests/test_client.py::test_alter_topic_config_unknown_raises tests/test_client.py::test_alter_topic_config_invalid_value_raises -v`
Expected: PASS.

- [ ] **Step 5: Run the full Python test suite**

Run: `cd kafkrs-python && python3 -m pytest tests/ -v`
Expected: PASS.

- [ ] **Step 6: Commit point**

Prompt the user to commit. Suggested message: `python: Client.alter_topic_config + tests`.

---

### Task 12: Version bump to 0.6.2 + CHANGELOG entries

**Files:**
- Modify: `kafkrs-models/Cargo.toml`
- Modify: `kafkrs-server/Cargo.toml`
- Modify: `kafkrs-python/pyproject.toml`
- Modify: `kafkrs-python/kafkrs/__init__.py`
- Modify: `Cargo.lock`
- Modify: `kafkrs-models/CHANGELOG.md`
- Modify: `kafkrs-server/CHANGELOG.md`
- Modify: `kafkrs-python/CHANGELOG.md`
- Modify: `README.md` (status line)

**Interfaces:**
- Consumes: everything from Tasks 1-11.
- Produces: 0.6.2 released across all three crates.

- [ ] **Step 1: Bump crate versions**

- `kafkrs-models/Cargo.toml`: change `version = "0.6.1"` → `version = "0.6.2"`.
- `kafkrs-server/Cargo.toml`: change `version = "0.6.1"` → `version = "0.6.2"`.
- `kafkrs-python/pyproject.toml`: change `version = "0.6.1"` → `version = "0.6.2"`.
- `kafkrs-python/kafkrs/__init__.py`: update the `__version__` string (grep for `__version__` if the exact name differs) from `"0.6.1"` → `"0.6.2"`.

If `kafkrs-server`'s `Cargo.toml` pins `kafkrs-models` by a version (e.g. `kafkrs-models = { path = "../kafkrs-models", version = "0.6.1" }`), bump that pin too.

- [ ] **Step 2: Rebuild to refresh `Cargo.lock`**

Run: `cargo build --workspace`
Expected: PASS. Verify `Cargo.lock` now shows `kafkrs-models 0.6.2` and `kafkrs-server 0.6.2`.

- [ ] **Step 3: Add CHANGELOG entries**

Prepend to each of the three CHANGELOG files. Follow the shape of the most recent `## 0.6.1` entry (check the file for the exact header pattern).

`kafkrs-models/CHANGELOG.md`:

```markdown
## 0.6.2

- Added `AlterTopicConfigRequest` / `AlterTopicConfigResponse` at oneof fields 52 / 53. Shrunk the admin reserved range to `[54, 59]`.
- Added `ERR_INVALID_CONFIG = 207` error code.
- Added `TopicConfigOverrides::validate()` pure function and `ConfigValidationError` type. Enforces `segment_size_bytes >= 1`, `segment_seal_time_ms >= 1`, `retention_ms >= -1`, `retention_bytes >= -1`.
```

`kafkrs-server/CHANGELOG.md`:

```markdown
## 0.6.2

- Added `AlterTopicConfig` admin RPC. Partial-patch semantics: fields set on the request overwrite the stored config; unset fields are unchanged. Merged config is persisted to `topics.json` before running `PartitionWriter` and `Uploader` actors are pushed `UpdateConfig` messages, so a crash between persist and push self-heals on restart.
- Added `RegistryMsg::Alter` and `RegistryError::InvalidConfig`. `CreateTopic` and `AlterTopicConfig` both validate their config against `TopicConfigOverrides::validate()`.
- Added `PwMsg::UpdateConfig(ResolvedTopicConfig)` and `UploaderMsg::UpdateConfig(ResolvedTopicConfig)` for live config replacement. FIFO ordering guarantees in-flight batches complete under the old config.
- Added startup validation gate: `TopicRegistry::load` panics with a message naming the topic and offending field if `topics.json` contains an out-of-range value.
```

`kafkrs-python/CHANGELOG.md`:

```markdown
## 0.6.2

- Added `Client.alter_topic_config(topic, overrides) -> TopicConfigOverrides`. Partial-patch semantics matching the broker's wire semantics.
```

- [ ] **Step 4: Update README status line**

Edit `README.md:7`. Change:

```
Current release is **0.6.0** across all three crates (versioned in lockstep).
```

to (assuming 0.6.1 shipped between; if the README was already updated to 0.6.1 at that time, just bump the number):

```
Current release is **0.6.2** across all three crates (versioned in lockstep).
```

Grep for the current version string first: `grep -n '0.6.' README.md`. If the README already reads `0.6.1`, change to `0.6.2`; if it reads `0.6.0`, still change to `0.6.2` (0.6.1 shipped without a README bump).

- [ ] **Step 5: Full workspace test**

Run: `cargo test --workspace`
Expected: all tests PASS.

Run: `cd kafkrs-python && python3 -m pytest tests/`
Expected: all tests PASS.

- [ ] **Step 6: Manual smoke test**

Rebuild the broker, then in one terminal:

```
cargo run --bin kafkrs-server -- config.toml
```

In a second terminal, run this Python one-liner (or paste into an ipython session):

```python
import asyncio
from kafkrs import Client
from kafkrs.wire import v1_pb2

async def main():
    async with Client("127.0.0.1", 5432) as c:
        await c.create_topic("smoke", partition_count=1)
        base, last = await c.produce("smoke", 0, [(b"k", b"v")])
        print("produced", base, last)
        o = v1_pb2.TopicConfigOverrides()
        o.retention_ms = 1
        resp = await c.alter_topic_config("smoke", o)
        print("altered:", resp.retention_ms)
        await asyncio.sleep(2)
        try:
            recs, hwm = await c.fetch("smoke", 0, from_offset=0)
            print("fetched", len(recs), "records at hwm", hwm)
        except Exception as e:
            print("expected fetch error:", e)

asyncio.run(main())
```

Expected: produce succeeds, alter returns `retention_ms=1`, fetch either returns 0 records or raises `WireError(code=202)` (`ErrOffsetOutOfRange`) — either is a pass; both mean the retention sweep observed the new value.

- [ ] **Step 7: Commit point**

Prompt the user to commit. Suggested message: `release: 0.6.2 (AlterTopicConfig)`.

---
