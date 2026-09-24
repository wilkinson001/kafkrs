//! End-to-end tests of produce/fetch: drives the broker over a real TCP
//! socket using the actual frame format, not via the internal actors.

mod common;
use common::*;

use kafkrs_models::wire::v1::{
    command::Body, Command, ConnectRequest, FetchRequest, InRecordMeta, ProduceRequest,
};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

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
