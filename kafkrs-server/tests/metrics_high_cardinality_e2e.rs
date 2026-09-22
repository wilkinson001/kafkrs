//! End-to-end test of the `metrics_high_cardinality` mode: installs a
//! Prometheus recorder with `high_cardinality: true` (a separate process-
//! global recorder from the one `wire_e2e.rs` installs — the `metrics`
//! crate allows exactly one recorder install per process, so this mode
//! must live in its own test binary), brings up an in-process broker, and
//! asserts the scraped exposition carries a `partition` label.

use bytes::{Bytes, BytesMut};
use kafkrs_models::config::{DiskType, ObjectStoreConfig};
use kafkrs_models::manifest::Manifest;
use kafkrs_models::topic::{ResolvedTopicConfig, TopicConfigOverrides};
use kafkrs_models::wire::v1::{
    command::Body, Command, ConnectRequest, InRecordMeta, ProduceRequest,
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

/// Installs the global Prometheus recorder exactly once for this test binary
/// with `high_cardinality: true`. Mirrors `init_metrics_once` in
/// `wire_e2e.rs`: install from a dedicated std thread with no ambient tokio
/// runtime so the exporter's own background runtime outlives any single
/// `#[tokio::test]`'s runtime.
fn init_metrics_once() -> u16 {
    METRICS_INIT.call_once(|| {
        let (tx, rx) = std::sync::mpsc::channel::<u16>();
        std::thread::spawn(move || {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind for port pick");
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let ports = kafkrs_models::config::PortsConfig {
                wire: vec![0],
                metrics: Some(port),
                health: None,
            };
            kafkrs_server::metrics::init(&ports, true).expect("metrics init");
            tx.send(port).expect("send port");
            std::thread::park();
        });
        let port = rx.recv().expect("recv port");
        METRICS_PORT.set(port).expect("port set");
        std::thread::sleep(std::time::Duration::from_millis(50));
    });
    *METRICS_PORT.get().expect("metrics port set")
}

/// Raw TCP HTTP scrape of `/metrics`. See `wire_e2e.rs::scrape_metrics` for
/// the rationale behind the bounded connect-retry loop.
async fn scrape_metrics(port: u16) -> String {
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

/// Brings up one broker with topic "t" partition 0 wired (produce + fetch),
/// duplicated from `wire_e2e.rs::setup_broker` since test binaries cannot
/// share non-`pub` helpers across files.
async fn setup_broker(dd: &str) -> u16 {
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
    port
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
async fn high_cardinality_mode_emits_partition_label() {
    let metrics_port = init_metrics_once();

    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker(dir.path().to_str().unwrap()).await;
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Connect.
    let connect = Command {
        correlation_id: 1,
        body: Some(Body::Connect(ConnectRequest {
            protocol_version: 1,
            client_id: "hc-test".into(),
            auth_data: vec![],
        })),
    };
    sock.write_all(&encode(&connect, b"")).await.unwrap();
    let _ = read_frame(&mut sock).await;

    // Produce 1 record to topic "t" partition 0.
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
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::ProduceResp(_))));

    let body = scrape_metrics(metrics_port).await;
    assert!(
        body.contains("messaging_kafkrs_produce_records"),
        "produce.records missing; body:\n{body}"
    );
    let has_partition_label = body.lines().any(|l| {
        l.contains("messaging_kafkrs_produce_records")
            && l.contains(r#"topic="t""#)
            && l.contains(r#"partition="0""#)
    });
    assert!(
        has_partition_label,
        "expected messaging_kafkrs_produce_records with topic=\"t\" partition=\"0\" under high_cardinality mode; body:\n{body}"
    );
}
