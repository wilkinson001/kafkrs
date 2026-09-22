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
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, RwLock};

static METRICS_INIT: std::sync::Once = std::sync::Once::new();
static METRICS_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

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

async fn setup_broker_no_topics(dd: &str) -> u16 {
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

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));

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
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(accept_loop(listener, state));
    port
}

async fn setup_broker_auto_create(dd: &str) -> u16 {
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

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));

    let (reg_tx, reg_rx) = mpsc::channel(8);
    let registry =
        TopicRegistry::load(dd.into(), DiskType::Nvme, store.clone(), "".into(), reg_rx).unwrap();
    tokio::spawn(registry.run());

    let state = SharedState {
        partitions: partitions.clone(),
        registry: reg_tx,
        store,
        prefix: "".into(),
        auto_create: true,
        default_partition_count: 1,
        data_dir: dd.into(),
        disk_type: DiskType::Nvme,
        spawn_locks: Arc::new(StdMutex::new(HashMap::new())),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(accept_loop(listener, state));
    port
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
        &manifest_key("", "t", 0),
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
    tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, cfg, urx, dtx).run());
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
        },
    );

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
        &manifest_key("", "t", 0),
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
    tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, cfg, urx, dtx).run());
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
    sock.write_all(&encode(&produce, b"keyvalue"))
        .await
        .unwrap();
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
            assert_eq!(
                e.code,
                kafkrs_models::wire::v1::ErrorCode::ErrUnsupportedProtocolVersion as i32
            );
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
            assert_eq!(
                e.code,
                kafkrs_models::wire::v1::ErrorCode::ErrHandshakeRequired as i32
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_topic_then_produce_succeeds() {
    use kafkrs_models::wire::v1::{ConnectedResponse, CreateTopicRequest};

    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker_no_topics(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // 1. Connect.
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "test".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(
        resp.body,
        Some(Body::Connected(ConnectedResponse { .. }))
    ));

    // 2. CreateTopic.
    let create = Command {
        correlation_id: 2,
        body: Some(Body::CreateTopic(CreateTopicRequest {
            topic: "explicit".into(),
            partition_count: 1,
            overrides: None,
        })),
    };
    sock.write_all(&encode(&create, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert_eq!(resp.correlation_id, 2);
    match resp.body {
        Some(Body::CreateTopicResp(_)) => {}
        other => panic!("expected CreateTopicResp, got {other:?}"),
    }

    // 3. Produce to the just-created topic. Without Fix 1 this returns ErrUnknownTopic.
    let produce = Command {
        correlation_id: 3,
        body: Some(Body::Produce(ProduceRequest {
            topic: "explicit".into(),
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
    assert_eq!(resp.correlation_id, 3);
    match resp.body {
        Some(Body::ProduceResp(r)) => {
            assert_eq!(r.base_offset, 0);
            assert_eq!(r.last_offset, 0);
        }
        other => panic!("expected ProduceResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversize_key_returns_err_key_too_large() {
    use kafkrs_models::wire::v1::ErrorCode;
    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect.
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

    // Produce a key of 1025 bytes (exceeds the default 1 KiB limit).
    let key = vec![0u8; 1025];
    let value = vec![0u8; 1];
    let mut payload = Vec::new();
    payload.extend_from_slice(&key);
    payload.extend_from_slice(&value);
    let produce = Command {
        correlation_id: 2,
        body: Some(Body::Produce(ProduceRequest {
            topic: "t".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 1025,
                value_len: 1,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce, &payload)).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert_eq!(resp.correlation_id, 2);
    match resp.body {
        Some(Body::Error(e)) => assert_eq!(e.code, ErrorCode::ErrKeyTooLarge as i32),
        other => panic!("expected Error(ErrKeyTooLarge), got {other:?}"),
    }
}

async fn setup_broker_with_max_fetch_wait(
    dd: &str,
    max_fetch_wait_ms: u64,
) -> (u16, Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>) {
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

    let o = TopicConfigOverrides {
        group_commit_record_count: Some(1),
        max_fetch_wait_ms: Some(max_fetch_wait_ms),
        ..Default::default()
    };
    let cfg = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
    let (utx, urx) = mpsc::channel(64);
    let (dtx, mut drx) = mpsc::channel(64);
    tokio::spawn(Uploader::new(store.clone(), "".into(), "t".into(), 0, cfg, urx, dtx).run());
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
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(accept_loop(listener, state));
    (port, partitions)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_max_wait_ms_is_capped() {
    use tokio::time::Instant;
    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) =
        setup_broker_with_max_fetch_wait(dir.path().to_str().unwrap(), 100).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect.
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

    // Fetch with max_wait_ms = 5000, but the topic caps it at 100.
    let fetch = Command {
        correlation_id: 2,
        body: Some(Body::Fetch(FetchRequest {
            topic: "t".into(),
            partition: 0,
            from_offset: 0,
            max_records: 10,
            max_wait_ms: 5_000,
        })),
    };
    let start = Instant::now();
    sock.write_all(&encode(&fetch, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    let elapsed = start.elapsed();
    assert_eq!(resp.correlation_id, 2);
    assert!(
        elapsed.as_millis() < 500,
        "fetch should be capped at ~100 ms, took {} ms",
        elapsed.as_millis()
    );
    match resp.body {
        Some(Body::FetchResp(r)) => assert!(r.records.is_empty()),
        other => panic!("expected FetchResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversize_value_returns_err_record_too_large() {
    use kafkrs_models::wire::v1::ErrorCode;
    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect.
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

    // Produce a value of 1 MiB + 1 byte (exceeds the default 1 MiB limit).
    let value = vec![0u8; (1024 * 1024) + 1];
    let mut payload = Vec::new();
    payload.extend_from_slice(&value);
    let produce = Command {
        correlation_id: 2,
        body: Some(Body::Produce(ProduceRequest {
            topic: "t".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 0,
                value_len: (1024 * 1024 + 1) as u32,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce, &payload)).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert_eq!(resp.correlation_id, 2);
    match resp.body {
        Some(Body::Error(e)) => assert_eq!(e.code, ErrorCode::ErrRecordTooLarge as i32),
        other => panic!("expected Error(ErrRecordTooLarge), got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broker_stays_responsive_after_disconnect_midpoll() {
    use kafkrs_models::wire::v1::PingRequest;
    use tokio::time::{sleep, Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) = setup_broker(dir.path().to_str().unwrap()).await;

    // First connection: start a long-poll Fetch, then drop without reading the response.
    {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let connect = Command {
            correlation_id: 1,
            body: Some(Body::Connect(ConnectRequest {
                protocol_version: 1,
                client_id: "first".into(),
                auth_data: vec![],
            })),
        };
        sock.write_all(&encode(&connect, b"")).await.unwrap();
        let _ = read_frame(&mut sock).await;
        let fetch = Command {
            correlation_id: 2,
            body: Some(Body::Fetch(FetchRequest {
                topic: "t".into(),
                partition: 0,
                from_offset: 0,
                max_records: 10,
                max_wait_ms: 30_000, // long poll
            })),
        };
        sock.write_all(&encode(&fetch, b"")).await.unwrap();
        // Drop sock without reading the response.
    }

    // Give the broker a moment to notice the disconnect.
    sleep(Duration::from_millis(200)).await;

    // Second connection: Connect + Ping should complete quickly.
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "second".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    let ping = Command {
        correlation_id: 2,
        body: Some(Body::Ping(PingRequest {})),
    };
    let start = Instant::now();
    sock.write_all(&encode(&ping, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    let elapsed = start.elapsed();
    assert_eq!(resp.correlation_id, 2);
    assert!(matches!(resp.body, Some(Body::Pong(_))));
    assert!(
        elapsed.as_millis() < 1_000,
        "Ping after disconnect-midpoll should complete quickly, took {} ms",
        elapsed.as_millis()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_create_existing_topic_does_not_respawn() {
    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker_auto_create(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect.
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

    // First produce auto-creates "demo" and writes offset 0.
    let produce1 = Command {
        correlation_id: 2,
        body: Some(Body::Produce(ProduceRequest {
            topic: "demo".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 1,
                value_len: 1,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce1, b"ab")).await.unwrap();
    let (resp1, _) = read_frame(&mut sock).await;
    match resp1.body {
        Some(Body::ProduceResp(r)) => assert_eq!(r.base_offset, 0),
        other => panic!("expected ProduceResp, got {other:?}"),
    }

    // Second produce to the SAME topic must not re-spawn workers and must
    // advance the offset to 1.
    let produce2 = Command {
        correlation_id: 3,
        body: Some(Body::Produce(ProduceRequest {
            topic: "demo".into(),
            partition: 0,
            records: vec![InRecordMeta {
                key_len: 1,
                value_len: 1,
                schema_id: 0,
                timestamp_ns: 0,
            }],
        })),
    };
    sock.write_all(&encode(&produce2, b"cd")).await.unwrap();
    let (resp2, _) = read_frame(&mut sock).await;
    match resp2.body {
        Some(Body::ProduceResp(r)) => assert_eq!(r.base_offset, 1),
        other => panic!("expected ProduceResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_topic_same_name_one_wins() {
    use kafkrs_models::wire::v1::{CreateTopicRequest, ErrorCode};

    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker_no_topics(dir.path().to_str().unwrap()).await;

    // Helper to drive a single CreateTopic and return the resulting Body.
    async fn create(port: u16) -> Option<Body> {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let connect = Command {
            correlation_id: 1,
            body: Some(Body::Connect(ConnectRequest {
                protocol_version: 1,
                client_id: "racer".into(),
                auth_data: vec![],
            })),
        };
        sock.write_all(&encode(&connect, b"")).await.unwrap();
        let _ = read_frame(&mut sock).await;
        let create = Command {
            correlation_id: 2,
            body: Some(Body::CreateTopic(CreateTopicRequest {
                topic: "racey".into(),
                partition_count: 1,
                overrides: None,
            })),
        };
        sock.write_all(&encode(&create, b"")).await.unwrap();
        let (resp, _) = read_frame(&mut sock).await;
        resp.body
    }

    // Fire both CreateTopic RPCs concurrently.
    let (a, b) = tokio::join!(create(port), create(port));

    // Exactly one CreateTopicResp and exactly one Error(ErrTopicAlreadyExists).
    let codes: Vec<_> = [a, b]
        .into_iter()
        .map(|body| match body {
            Some(Body::CreateTopicResp(_)) => "ok",
            Some(Body::Error(e)) if e.code == ErrorCode::ErrTopicAlreadyExists as i32 => {
                "already_exists"
            }
            other => panic!("unexpected response body: {other:?}"),
        })
        .collect();
    let mut sorted = codes.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["already_exists", "ok"]);

    // Produce against the topic — confirms no orphaning is externally visible.
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "producer".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    let produce = Command {
        correlation_id: 2,
        body: Some(Body::Produce(ProduceRequest {
            topic: "racey".into(),
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
    match resp.body {
        Some(Body::ProduceResp(r)) => assert_eq!(r.base_offset, 0),
        other => panic!("expected ProduceResp, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_evicts_old_segments_via_sweeper() {
    use kafkrs_models::wire::v1::ErrorCode;

    let dir = tempfile::tempdir().unwrap();
    let (port, _partitions) =
        setup_broker_with_retention(dir.path().to_str().unwrap(), 200, 100).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect.
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

    // Produce 3 records; segment_size_bytes = 1 forces sealing after each.
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

    // Wait past retention_ms + several sweep intervals so old segments expire
    // and the sweeper has had time to trigger retention on the partition.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    // Produce one more record to keep the tail active and guarantee a
    // subsequent kick doesn't race the produce.
    let produce_tail = Command {
        correlation_id: 100,
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
    sock.write_all(&encode(&produce_tail, b"kv")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::ProduceResp(_))));

    // Fetch from offset 0. Older segments have been evicted → ErrOffsetOutOfRange.
    let fetch = Command {
        correlation_id: 200,
        body: Some(Body::Fetch(FetchRequest {
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
            assert_eq!(e.code, ErrorCode::ErrOffsetOutOfRange as i32);
        }
        other => panic!("expected Error(ErrOffsetOutOfRange), got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_endpoint_serves_prometheus_text() {
    let port = init_metrics_once();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let body = scrape_metrics(port).await;
    assert!(body.starts_with("HTTP/1.1 200"), "got: {body}");
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
