//! End-to-end tests of AlterTopicConfig: drives the broker over a real TCP
//! socket using the actual frame format, not via the internal actors.

mod common;
use common::*;

use kafkrs_models::wire::v1::{command::Body, Command, ConnectRequest, InRecordMeta, ProduceRequest};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

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
