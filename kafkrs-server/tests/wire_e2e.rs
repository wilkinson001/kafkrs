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
        data_dir: dd.into(),
        disk_type: DiskType::Nvme,
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
    let (utx, urx) = mpsc::channel::<UploaderMsg>(64);
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
        .insert(("t".into(), 0), PartitionHandle { pw_tx, tail, cfg });

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
    assert!(matches!(resp.body, Some(Body::Connected(ConnectedResponse { .. }))));

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
