//! End-to-end test of the wire protocol: drives the broker over a real TCP
//! socket using the actual frame format, not via the internal actors.

use bytes::{Bytes, BytesMut};
use kafkrs_models::config::{DiskType, ObjectStoreConfig};
use kafkrs_models::manifest::Manifest;
use kafkrs_models::topic::{
    ResolvedTopicConfig, TopicConfigOverrides, TopicEntry, TopicRegistryFile,
};
use kafkrs_models::wire::v1::{
    command::Body, Command, ConnectRequest, FetchRequest, InRecordMeta, ProduceRequest,
};
use kafkrs_server::broker_identity::BrokerIdentity;
use kafkrs_server::object_store::{build_store, manifest_key, put};
use kafkrs_server::partition_writer::{PartitionWriter, PwMsg};
use kafkrs_server::topic_registry::TopicRegistry;
use kafkrs_server::uploader::{Uploader, UploaderMsg};
use kafkrs_server::wire::{accept_loop, PartitionHandle, SharedState};
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, RwLock};

static METRICS_INIT: std::sync::Once = std::sync::Once::new();
static METRICS_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

const TOPIC_UUID: &str = "01936a80-0000-7000-8000-000000000000";

fn test_identity() -> BrokerIdentity {
    BrokerIdentity {
        broker_id: Arc::from("brk-testtest".to_string()),
        cluster_id: Arc::from("test-cluster".to_string()),
        advertised_host: Arc::from("127.0.0.1".to_string()),
        advertised_port: 5432,
    }
}

/// Installs the global Prometheus recorder exactly once for the whole test
/// binary (the `metrics` crate's global recorder can only be installed
/// once per process) and returns the ephemeral port it is listening on.
///
/// The install is performed from a dedicated std thread that has NO
/// ambient tokio runtime, so `PrometheusBuilder::install()` takes its
/// fallback branch and spawns its own runtime on a background thread
/// owned by the process — not the per-test `#[tokio::test]` runtime.
/// Without this, the exporter future is spawned onto whichever test's
/// runtime happened to win the race for `call_once`, and dies when
/// that test's runtime is dropped, breaking every subsequent test.
fn init_metrics_once() -> u16 {
    METRICS_INIT.call_once(|| {
        let (tx, rx) = std::sync::mpsc::channel::<u16>();
        std::thread::spawn(move || {
            // Grab a random ephemeral port before init.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind for port pick");
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let ports = kafkrs_models::config::PortsConfig {
                wire: vec![0],
                metrics: Some(port),
                // Reuse the same port so /metrics + /health + /ready are
                // all served on the merged listener — exercises the
                // same-port coupling path used by the health-endpoint tests.
                health: Some(port),
            };
            kafkrs_server::metrics::init(&ports, false).expect("metrics init");
            tx.send(port).expect("send port");
            // Keep this thread alive so any thread-local state the exporter
            // relies on outlives every test in the binary. `install()` on
            // this path spawns its own background thread with a
            // current-thread runtime, so this thread's own lifetime
            // doesn't strictly own the exporter — but keeping it parked
            // is defensive against future changes.
            std::thread::park();
        });
        let port = rx.recv().expect("recv port");
        METRICS_PORT.set(port).expect("port set");
        // Give exporter time to bind.
        std::thread::sleep(std::time::Duration::from_millis(50));
    });
    *METRICS_PORT.get().expect("metrics port set")
}

/// Raw TCP HTTP scrape of `/metrics`. Sends `Connection: close` so the
/// exporter's HTTP server closes the socket once the response is written,
/// letting `read_to_end` return promptly instead of hanging on keep-alive.
///
/// The initial `TcpStream::connect` is wrapped in a bounded retry loop
/// because `metrics::init` re-binds the exporter port asynchronously after
/// `init_metrics_once` picks it via an ephemeral-port `TcpListener` and
/// drops it — there is a genuine TOCTOU window between drop and re-bind
/// during which `connect` returns `ConnectionRefused`. Up to 30 × 50ms =
/// 1.5s of retries covers that window on loaded CI runners.
async fn scrape_metrics(port: u16) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let mut sock = None;
    for _ in 0..30 {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => {
                sock = Some(s);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => panic!("connect to metrics port {port}: {e}"),
        }
    }
    let mut sock = sock.unwrap_or_else(|| panic!("metrics exporter never accepted on port {port}"));
    sock.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sock.read_to_end(&mut buf),
    )
    .await
    .expect("scrape timeout");
    String::from_utf8_lossy(&buf).into_owned()
}

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
        &manifest_key("", "t", TOPIC_UUID, 0),
        Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap()),
    )
    .await
    .unwrap();

    // Spin up an Uploader + PartitionWriter for ("t", 0).
    let o = TopicConfigOverrides {
        group_commit_record_count: Some(1),
        ..Default::default()
    };
    let cfg = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
    let (utx, urx) = mpsc::channel::<UploaderMsg>(64);
    let (dtx, mut drx) = mpsc::channel(64);
    tokio::spawn(
        Uploader::new(
            store.clone(),
            "".into(),
            "t".into(),
            TOPIC_UUID.into(),
            0,
            cfg,
            urx,
            dtx,
        )
        .run(),
    );
    let (pw_tx, pw_rx) = mpsc::channel(256);
    let (tail, _) = broadcast::channel(1024);
    let pw_tx_d = pw_tx.clone();
    tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let _ = pw_tx_d.send(PwMsg::SegmentDurable(d)).await;
        }
    });
    let pw = PartitionWriter::new(
        dd.into(),
        "t".into(),
        TOPIC_UUID.into(),
        0,
        cfg,
        0,
        vec![],
        pw_rx,
        utx.clone(),
        tail.clone(),
    )
    .unwrap();
    tokio::spawn(pw.run());

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));
    partitions.write().await.insert(
        ("t".into(), 0),
        PartitionHandle {
            pw_tx,
            tail,
            cfg,
            uploader_tx: utx,
            uuid: TOPIC_UUID.into(),
        },
    );

    // Seed topics.json with an entry for "t" mirroring the PartitionHandle
    // wired above (same UUID, same overrides) so registry-backed RPCs
    // (AlterTopicConfig, DescribeTopic, ...) see "t" as a known topic.
    std::fs::write(
        std::path::Path::new(dd).join("topics.json"),
        serde_json::to_vec(&TopicRegistryFile {
            topics: vec![TopicEntry {
                name: "t".into(),
                uuid: TOPIC_UUID.into(),
                partition_count: 1,
                created_at_ns: 0,
                config: o,
            }],
        })
        .unwrap(),
    )
    .unwrap();

    // Spin up a topic registry actor (needed for SharedState even if not used by
    // produce/fetch in this test).
    let (reg_tx, reg_rx) = mpsc::channel(8);
    let registry =
        TopicRegistry::load(dd.into(), DiskType::Nvme, store.clone(), "".into(), reg_rx).unwrap();
    tokio::spawn(registry.run());

    let state = SharedState {
        partitions: partitions.clone(),
        registry: reg_tx,
        store,
        prefix: "".into(),
        auto_create: false,
        default_partition_count: 1,
        data_dir: dd.into(),
        disk_type: DiskType::Nvme,
        spawn_locks: Arc::new(StdMutex::new(HashMap::new())),
        identity: test_identity(),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(accept_loop(listener, state));
    (port, partitions)
}

async fn setup_broker_with_retention(
    dd: &str,
    retention_ms: i64,
    sweep_interval_ms: u64,
) -> (u16, Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>) {
    use kafkrs_server::retention_sweeper::RetentionSweeper;
    use tokio::time::Duration;

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
        &manifest_key("", "t", TOPIC_UUID, 0),
        Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap()),
    )
    .await
    .unwrap();

    let (utx, urx) = mpsc::channel(64);
    let (dtx, mut drx) = mpsc::channel(64);
    let o = TopicConfigOverrides {
        segment_size_bytes: Some(1),
        group_commit_record_count: Some(1),
        retention_ms: Some(retention_ms),
        ..Default::default()
    };
    let cfg = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
    tokio::spawn(
        Uploader::new(
            store.clone(),
            "".into(),
            "t".into(),
            TOPIC_UUID.into(),
            0,
            cfg,
            urx,
            dtx,
        )
        .run(),
    );
    let (pw_tx, pw_rx) = mpsc::channel(256);
    let (tail, _) = broadcast::channel(1024);
    let pw_tx_d = pw_tx.clone();
    tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let _ = pw_tx_d.send(PwMsg::SegmentDurable(d)).await;
        }
    });
    let pw = PartitionWriter::new(
        dd.into(),
        "t".into(),
        TOPIC_UUID.into(),
        0,
        cfg,
        0,
        vec![],
        pw_rx,
        utx.clone(),
        tail.clone(),
    )
    .unwrap();
    tokio::spawn(pw.run());

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));
    partitions.write().await.insert(
        ("t".into(), 0),
        PartitionHandle {
            pw_tx,
            tail,
            cfg,
            uploader_tx: utx,
            uuid: TOPIC_UUID.into(),
        },
    );

    let (reg_tx, reg_rx) = mpsc::channel(8);
    let registry =
        TopicRegistry::load(dd.into(), DiskType::Nvme, store.clone(), "".into(), reg_rx).unwrap();
    tokio::spawn(registry.run());

    let state = SharedState {
        partitions: partitions.clone(),
        registry: reg_tx,
        store,
        prefix: "".into(),
        auto_create: false,
        default_partition_count: 1,
        data_dir: dd.into(),
        disk_type: DiskType::Nvme,
        spawn_locks: Arc::new(StdMutex::new(HashMap::new())),
        identity: test_identity(),
    };

    tokio::spawn(
        RetentionSweeper::new(partitions.clone(), Duration::from_millis(sweep_interval_ms)).run(),
    );

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
async fn metrics_endpoint_serves_prometheus_text() {
    let port = init_metrics_once();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let body = scrape_metrics(port).await;
    assert!(body.starts_with("HTTP/1.1 200"), "got: {body}");
}

/// Scrape an arbitrary path on the admin port using the same raw-TCP + retry
/// pattern as [`scrape_metrics`]. Sends `Connection: close` so the response
/// terminates promptly.
async fn scrape_path(port: u16, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let mut sock = None;
    for _ in 0..30 {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => {
                sock = Some(s);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => panic!("connect to admin port {port}: {e}"),
        }
    }
    let mut sock = sock.unwrap_or_else(|| panic!("admin listener never accepted on port {port}"));
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sock.read_to_end(&mut buf),
    )
    .await
    .expect("scrape timeout");
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_endpoint_returns_200() {
    let port = init_metrics_once();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let body = scrape_path(port, "/health").await;
    assert!(body.starts_with("HTTP/1.1 200"), "got: {body}");
    assert!(body.contains("ok"), "expected 'ok' body, got: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_endpoint_starts_503_then_flips_to_200() {
    let port = init_metrics_once();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Nothing else in the test binary calls set_ready(true), so this
    // assertion is stable regardless of test ordering.
    let body = scrape_path(port, "/ready").await;
    assert!(
        body.starts_with("HTTP/1.1 503"),
        "expected 503 before set_ready, got: {body}"
    );
    kafkrs_server::metrics::set_ready(true);
    let body = scrape_path(port, "/ready").await;
    assert!(
        body.starts_with("HTTP/1.1 200"),
        "expected 200 after set_ready(true), got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_produce_counter_increments() {
    let metrics_port = init_metrics_once();

    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect + produce 3 records on topic "t".
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "t".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    for i in 0..3u64 {
        let produce = Command {
            correlation_id: 2 + i,
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
        let (resp, _) = read_frame(&mut sock).await;
        assert!(matches!(resp.body, Some(Body::ProduceResp(_))));
    }

    let body = scrape_metrics(metrics_port).await;
    // Prometheus exposition: dots become underscores.
    assert!(
        body.contains("messaging_kafkrs_produce_records"),
        "produce records metric missing; body:\n{body}"
    );
    // Assert the counter reflects at least the 3 records produced above.
    // Not an exact match: this test shares topic "t" and the process-global
    // Prometheus recorder (see `init_metrics_once`) with other metrics e2e
    // tests (e.g. `metrics_fetch_counters_increment`), which may add their
    // own produce activity on the same topic when tests run concurrently.
    let has_count = body.lines().any(|l| {
        if !l.contains("messaging_kafkrs_produce_records") || !l.contains(r#"topic="t""#) {
            return false;
        }
        l.trim()
            .rsplit(' ')
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .is_some_and(|v| v >= 3.0)
    });
    assert!(has_count, "expected count >= 3 for topic=t; body:\n{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_fetch_counters_increment() {
    let metrics_port = init_metrics_once();
    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // `setup_broker` only wires up topic "t" partition 0, so we must use
    // that topic name. This test only checks for the presence of metric
    // families (not exact counts), so it tolerates topic="t" activity from
    // other metrics tests sharing the process-global Prometheus recorder
    // (see `init_metrics_once`).
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "t".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // Produce one record then fetch it.
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
    let (_resp, _) = read_frame(&mut sock).await;

    let fetch = Command {
        correlation_id: 3,
        body: Some(Body::Fetch(FetchRequest {
            topic: "t".into(),
            partition: 0,
            from_offset: 0,
            max_records: 10,
            max_wait_ms: 0,
        })),
    };
    sock.write_all(&encode(&fetch, b"")).await.unwrap();
    let (_resp, _) = read_frame(&mut sock).await;

    let body = scrape_metrics(metrics_port).await;
    assert!(
        body.contains("messaging_kafkrs_fetch_requests"),
        "fetch.requests missing; body:\n{body}"
    );
    assert!(
        body.contains("messaging_kafkrs_fetch_source"),
        "fetch.source missing; body:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_uploader_and_retention_appear_after_produce() {
    let metrics_port = init_metrics_once();
    let dir = tempfile::tempdir().unwrap();
    // setup_broker_with_retention exists from the retention feature; use tight
    // retention to force eviction quickly.
    let (port, _partitions) =
        setup_broker_with_retention(dir.path().to_str().unwrap(), 200, 100).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "t".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    for i in 0..3u64 {
        let produce = Command {
            correlation_id: 2 + i,
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
        let (_resp, _) = read_frame(&mut sock).await;
    }
    // Wait past retention to trigger eviction.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    let body = scrape_metrics(metrics_port).await;
    assert!(
        body.contains("kafkrs_uploader_segments_uploaded"),
        "uploader.segments_uploaded missing; body:\n{body}"
    );
    assert!(
        body.contains("kafkrs_retention_passes"),
        "retention.passes missing; body:\n{body}"
    );
    assert!(
        body.contains("kafkrs_retention_sweep_kicks_sent"),
        "retention.sweep_kicks_sent missing; body:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alter_topic_config_unknown_topic_returns_err_unknown_topic() {
    use kafkrs_models::wire::v1::ErrorCode;

    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
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
    use kafkrs_models::wire::v1::ErrorCode;

    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
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
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
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
    use kafkrs_models::wire::v1::ErrorCode;

    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
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

    // setup_broker's "t" partition only sets group_commit_record_count = 1,
    // which forces a WAL flush per record but NOT a seal (seals only fire
    // once active_bytes >= segment_size_bytes, default 128 MiB). Tighten
    // segment_size_bytes first so the very next flush seals immediately,
    // giving us a real uploaded segment to evict later.
    let alter_segment_size = Command {
        correlation_id: 2,
        body: Some(Body::AlterTopicConfig(
            kafkrs_models::wire::v1::AlterTopicConfigRequest {
                topic: "t".into(),
                overrides: Some(kafkrs_models::wire::v1::TopicConfigOverrides {
                    segment_size_bytes: Some(1),
                    group_commit_record_count: Some(1),
                    ..Default::default()
                }),
            },
        )),
    };
    sock.write_all(&encode(&alter_segment_size, b""))
        .await
        .unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::AlterTopicConfigResp(_))));

    // Produce one record so we have a segment to evict later.
    let produce = Command {
        correlation_id: 3,
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

    // Give the uploader time to seal + upload (segment_size_bytes = 1 above
    // forces a seal on the very next flush_commit, which already happened
    // synchronously as part of handling the produce above; this sleep gives
    // the async Uploader time to actually PUT the segment + manifest).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Alter to a very tight retention_ms.
    let alter = Command {
        correlation_id: 4,
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
        correlation_id: 5,
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
        correlation_id: 6,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_empty_filter_returns_all_topics_and_self_broker() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect.
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

    // Metadata request with empty filter.
    let md = Command {
        correlation_id: 2,
        body: Some(Body::Metadata(kafkrs_models::wire::v1::MetadataRequest {
            topics: vec![],
        })),
    };
    sock.write_all(&encode(&md, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::MetadataResp(m)) => {
            assert_eq!(m.cluster_id, "test-cluster");
            assert_eq!(m.brokers.len(), 1);
            assert_eq!(m.brokers[0].broker_id, "brk-testtest");
            assert_eq!(m.brokers[0].host, "127.0.0.1");
            assert_eq!(m.brokers[0].port, 5432);
            // setup_broker seeds topic "t" — expect at least that.
            let names: Vec<&str> = m.topics.iter().map(|t| t.topic.as_str()).collect();
            assert!(
                names.contains(&"t"),
                "expected topic 't' in metadata, got {names:?}"
            );
            let t = m.topics.iter().find(|t| t.topic == "t").unwrap();
            assert_eq!(t.error_code, 0);
            assert!(!t.topic_uuid.is_empty());
            assert!(!t.partitions.is_empty());
            for p in &t.partitions {
                assert_eq!(p.leader_broker_id, "brk-testtest");
            }
        }
        other => panic!("expected MetadataResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_filter_returns_only_requested_topics() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
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

    let md = Command {
        correlation_id: 2,
        body: Some(Body::Metadata(kafkrs_models::wire::v1::MetadataRequest {
            topics: vec!["t".into()],
        })),
    };
    sock.write_all(&encode(&md, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::MetadataResp(m)) => {
            assert_eq!(m.topics.len(), 1);
            assert_eq!(m.topics[0].topic, "t");
            assert_eq!(m.topics[0].error_code, 0);
        }
        other => panic!("expected MetadataResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_filter_with_unknown_topic_returns_per_topic_error() {
    use kafkrs_models::wire::v1::ErrorCode;

    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
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

    let md = Command {
        correlation_id: 2,
        body: Some(Body::Metadata(kafkrs_models::wire::v1::MetadataRequest {
            topics: vec!["t".into(), "no-such-topic".into()],
        })),
    };
    sock.write_all(&encode(&md, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::MetadataResp(m)) => {
            assert_eq!(m.topics.len(), 2);
            let ok = m.topics.iter().find(|t| t.topic == "t").expect("t missing");
            assert_eq!(ok.error_code, 0);
            assert!(!ok.topic_uuid.is_empty());
            assert!(!ok.partitions.is_empty());

            let bad = m
                .topics
                .iter()
                .find(|t| t.topic == "no-such-topic")
                .expect("no-such-topic missing");
            assert_eq!(bad.error_code, ErrorCode::ErrUnknownTopic as u32);
            assert!(bad.topic_uuid.is_empty());
            assert!(bad.partitions.is_empty());
        }
        other => panic!("expected MetadataResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_partition_metadata_lists_self_as_leader_for_every_partition() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
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

    // Create a topic with 3 partitions.
    let create = Command {
        correlation_id: 2,
        body: Some(Body::CreateTopic(
            kafkrs_models::wire::v1::CreateTopicRequest {
                topic: "multi".into(),
                partition_count: 3,
                overrides: None,
            },
        )),
    };
    sock.write_all(&encode(&create, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    let md = Command {
        correlation_id: 3,
        body: Some(Body::Metadata(kafkrs_models::wire::v1::MetadataRequest {
            topics: vec!["multi".into()],
        })),
    };
    sock.write_all(&encode(&md, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::MetadataResp(m)) => {
            let t = &m.topics[0];
            assert_eq!(t.partitions.len(), 3);
            let mut pids: Vec<u32> = t.partitions.iter().map(|p| p.partition).collect();
            pids.sort();
            assert_eq!(pids, vec![0, 1, 2]);
            for p in &t.partitions {
                assert_eq!(p.leader_broker_id, "brk-testtest");
            }
        }
        other => panic!("expected MetadataResp, got {other:?}"),
    }
}
