#![allow(dead_code)]
//! Shared helpers for the wire e2e test binaries: broker setup, wire
//! encode/decode, and metrics-scraping utilities.

use bytes::{Bytes, BytesMut};
use kafkrs_models::config::{DiskType, ObjectStoreConfig};
use kafkrs_models::manifest::Manifest;
use kafkrs_models::topic::{
    ResolvedTopicConfig, TopicConfigOverrides, TopicEntry, TopicRegistryFile,
};
use kafkrs_models::wire::v1::Command;
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
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, RwLock};

pub static METRICS_INIT: std::sync::Once = std::sync::Once::new();
pub static METRICS_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

pub const TOPIC_UUID: &str = "01936a80-0000-7000-8000-000000000000";

pub fn test_identity() -> BrokerIdentity {
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
pub fn init_metrics_once() -> u16 {
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
pub async fn scrape_metrics(port: u16) -> String {
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

pub async fn setup_broker_no_topics(dd: &str) -> u16 {
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
        identity: test_identity(),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(accept_loop(listener, state));
    port
}

pub async fn setup_broker_auto_create(dd: &str) -> u16 {
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
        identity: test_identity(),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(accept_loop(listener, state));
    port
}

pub async fn setup_broker(dd: &str) -> (u16, Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>) {
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

pub async fn setup_broker_with_retention(
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

pub async fn setup_broker_with_max_fetch_wait(
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
        &manifest_key("", "t", TOPIC_UUID, 0),
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
    (port, partitions)
}

/// Encode a Command + payload to outer wire bytes.
pub fn encode(cmd: &Command, payload: &[u8]) -> Bytes {
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
pub async fn read_frame(sock: &mut TcpStream) -> (Command, Bytes) {
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

/// Scrape an arbitrary path on the admin port using the same raw-TCP + retry
/// pattern as [`scrape_metrics`]. Sends `Connection: close` so the response
/// terminates promptly.
pub async fn scrape_path(port: u16, path: &str) -> String {
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
