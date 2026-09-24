//! End-to-end tests of topic creation/admin flows: drives the broker over a
//! real TCP socket using the actual frame format, not via the internal
//! actors.

mod common;
use common::*;

use kafkrs_models::wire::v1::{
    command::Body, Command, ConnectRequest, InRecordMeta, ProduceRequest,
};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

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
