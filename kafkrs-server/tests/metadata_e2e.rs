//! End-to-end tests of Metadata: drives the broker over a real TCP socket
//! using the actual frame format, not via the internal actors.

mod common;
use common::*;

use kafkrs_models::wire::v1::{command::Body, Command, ConnectRequest};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

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
