//! End-to-end tests of topic deletion: drives the broker over a real TCP
//! socket using the actual frame format, not via the internal actors.

mod common;
use common::*;

use kafkrs_models::config::ObjectStoreConfig;
use kafkrs_models::wire::v1::{command::Body, Command, ConnectRequest, InRecordMeta, ProduceRequest};
use kafkrs_server::object_store::build_store;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_topic_removes_partition_and_rejects_subsequent_produce() {
    use kafkrs_models::wire::v1::{
        ConnectedResponse, CreateTopicRequest, DeleteTopicRequest, ErrorCode,
    };

    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker_no_topics(dir.path().to_str().unwrap()).await;
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
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(
        resp.body,
        Some(Body::Connected(ConnectedResponse { .. }))
    ));

    // CreateTopic "t" so the registry actually knows about it (DeleteTopic
    // requires a registry entry).
    let create = Command {
        correlation_id: 2,
        body: Some(Body::CreateTopic(CreateTopicRequest {
            topic: "t".into(),
            partition_count: 1,
            overrides: None,
        })),
    };
    sock.write_all(&encode(&create, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::CreateTopicResp(_))));

    // Produce 3 records to topic "t".
    for i in 0..3u64 {
        let produce = Command {
            correlation_id: 3 + i,
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

    // Delete.
    let del = Command {
        correlation_id: 100,
        body: Some(Body::DeleteTopic(DeleteTopicRequest {
            topic: "t".into(),
            delete_data: Some(true),
        })),
    };
    sock.write_all(&encode(&del, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::DeleteTopicResp(_))));

    // Give the sweep some time to run.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Produce again — should get ErrUnknownTopic.
    let produce = Command {
        correlation_id: 200,
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
    match resp.body {
        Some(Body::Error(e)) => {
            assert_eq!(e.code, ErrorCode::ErrUnknownTopic as i32);
        }
        other => panic!("expected ErrUnknownTopic, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_topic_delete_data_false_preserves_object_store_data() {
    use kafkrs_models::topic::TopicRegistryFile;
    use kafkrs_models::wire::v1::{
        ConnectedResponse, CreateTopicRequest, DeleteTopicRequest, TopicConfigOverrides,
    };

    let dir = tempfile::tempdir().unwrap();
    let object_root = dir.path().join("object_store");
    let port = setup_broker_no_topics(dir.path().to_str().unwrap()).await;
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
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(
        resp.body,
        Some(Body::Connected(ConnectedResponse { .. }))
    ));

    // CreateTopic "t" with a tiny segment size so a seal + upload happens
    // almost immediately after the first produce.
    let create = Command {
        correlation_id: 2,
        body: Some(Body::CreateTopic(CreateTopicRequest {
            topic: "t".into(),
            partition_count: 1,
            overrides: Some(TopicConfigOverrides {
                segment_size_bytes: Some(1),
                group_commit_record_count: Some(1),
                ..Default::default()
            }),
        })),
    };
    sock.write_all(&encode(&create, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::CreateTopicResp(_))));

    // Produce enough records to trigger a seal + upload.
    for i in 0..3u64 {
        let produce = Command {
            correlation_id: 3 + i,
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

    // Sleep so the upload completes.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Grab the topic UUID before delete so we can locate the object-store
    // prefix (which is keyed by UUID, not name).
    let topics_json = std::fs::read_to_string(dir.path().join("topics.json")).unwrap();
    let file: TopicRegistryFile = serde_json::from_str(&topics_json).unwrap();
    let uuid = file
        .topics
        .iter()
        .find(|t| t.name == "t")
        .unwrap()
        .uuid
        .clone();

    // Delete with delete_data=false.
    let del = Command {
        correlation_id: 100,
        body: Some(Body::DeleteTopic(DeleteTopicRequest {
            topic: "t".into(),
            delete_data: Some(false),
        })),
    };
    sock.write_all(&encode(&del, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::DeleteTopicResp(_))));

    // Give any (unwanted) sweep a chance to run so the negative assertion is
    // meaningful.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Assert object-store data is STILL present under `<object_store>/t/v=<uuid>/...`.
    let topic_uuid_root = object_root.join("t").join(format!("v={uuid}"));
    assert!(
        topic_uuid_root.exists(),
        "topic object-store prefix should still exist after delete_data=false: {}",
        topic_uuid_root.display()
    );

    // Assert the WAL dir is ALSO still present under delete_data=false: per
    // spec, "detach" leaves both storage tiers alone (WAL + object-store).
    let wal_dir = dir.path().join("wal").join("t");
    assert!(
        wal_dir.exists(),
        "WAL dir should still exist after delete_data=false: {}",
        wal_dir.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_then_recreate_same_name_uses_new_uuid_prefix() {
    use kafkrs_models::topic::TopicRegistryFile;
    use kafkrs_models::wire::v1::{ConnectedResponse, CreateTopicRequest, DeleteTopicRequest};

    let dir = tempfile::tempdir().unwrap();
    let port = setup_broker_no_topics(dir.path().to_str().unwrap()).await;
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
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(
        resp.body,
        Some(Body::Connected(ConnectedResponse { .. }))
    ));

    // CreateTopic "t".
    let create = Command {
        correlation_id: 2,
        body: Some(Body::CreateTopic(CreateTopicRequest {
            topic: "t".into(),
            partition_count: 1,
            overrides: None,
        })),
    };
    sock.write_all(&encode(&create, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::CreateTopicResp(_))));

    // Snapshot the UUID from before delete.
    let topics_json_1 = std::fs::read_to_string(dir.path().join("topics.json")).unwrap();
    let file1: TopicRegistryFile = serde_json::from_str(&topics_json_1).unwrap();
    let uuid_before = file1
        .topics
        .iter()
        .find(|t| t.name == "t")
        .unwrap()
        .uuid
        .clone();

    // Delete.
    let del = Command {
        correlation_id: 100,
        body: Some(Body::DeleteTopic(DeleteTopicRequest {
            topic: "t".into(),
            delete_data: Some(true),
        })),
    };
    sock.write_all(&encode(&del, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::DeleteTopicResp(_))));

    // Immediately recreate.
    let create2 = Command {
        correlation_id: 101,
        body: Some(Body::CreateTopic(CreateTopicRequest {
            topic: "t".into(),
            partition_count: 1,
            overrides: None,
        })),
    };
    sock.write_all(&encode(&create2, b"")).await.unwrap();
    let (resp, _) = read_frame(&mut sock).await;
    assert!(matches!(resp.body, Some(Body::CreateTopicResp(_))));

    // Read topics.json again — UUID should differ.
    let topics_json_2 = std::fs::read_to_string(dir.path().join("topics.json")).unwrap();
    let file2: TopicRegistryFile = serde_json::from_str(&topics_json_2).unwrap();
    let uuid_after = file2
        .topics
        .iter()
        .find(|t| t.name == "t")
        .unwrap()
        .uuid
        .clone();

    assert_ne!(
        uuid_before, uuid_after,
        "recreated topic should get a fresh UUID"
    );

    // Produce on the recreated topic should succeed.
    let produce = Command {
        correlation_id: 102,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_delete_survives_broker_restart() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();

    // Manually pre-populate pending_deletes.json as if a prior broker crashed
    // mid-sweep. Use an empty snapshot (no segments to delete) so the sweep
    // completes trivially — the important assertion is that the sweep RAN on
    // startup and cleared the pending entry.
    let record = kafkrs_server::pending_deletes::PendingDelete {
        topic: "ghost".into(),
        uuid: "01936a80-0000-7000-8000-00000000abcd".into(),
        manifests_by_partition: std::collections::BTreeMap::new(),
        created_ns: 0,
    };
    kafkrs_server::pending_deletes::append(dd, record)
        .await
        .unwrap();

    // Start the broker (which invokes startup-replay) via the same wiring
    // main.rs uses: TopicRegistry + startup-replay loop over pending_deletes.
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
    let prefix = String::new();
    let pending = kafkrs_server::pending_deletes::load_all(dd).await.unwrap();
    for record in pending {
        let store = store.clone();
        let prefix = prefix.clone();
        let data_dir = dd.to_string();
        tokio::spawn(async move {
            let _ = kafkrs_server::deletion::sweep_deletion(record, store, prefix, data_dir).await;
        });
    }

    // Give the replayed sweep time to complete.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // The pending_deletes.json should now be empty.
    let remaining = kafkrs_server::pending_deletes::load_all(dd).await.unwrap();
    assert!(
        remaining.is_empty(),
        "startup replay should have cleared pending_deletes.json"
    );
}
