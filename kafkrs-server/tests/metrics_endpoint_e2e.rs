//! End-to-end tests of the metrics/health/ready admin endpoints: drives the
//! broker over a real TCP socket using the actual frame format, not via the
//! internal actors.

mod common;
use common::*;

use kafkrs_models::wire::v1::{
    command::Body, Command, ConnectRequest, FetchRequest, InRecordMeta, ProduceRequest,
};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_endpoint_serves_prometheus_text() {
    let port = init_metrics_once();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let body = scrape_metrics(port).await;
    assert!(body.starts_with("HTTP/1.1 200"), "got: {body}");
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
