//! End-to-end tests of the connect handshake: drives the broker over a real
//! TCP socket using the actual frame format, not via the internal actors.

mod common;
use common::*;

use kafkrs_models::wire::v1::{command::Body, Command, ConnectRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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
async fn connect_response_carries_broker_id_and_cluster_id() {
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
    let (resp, _) = read_frame(&mut sock).await;
    match resp.body {
        Some(Body::Connected(c)) => {
            assert_eq!(c.broker_id, "brk-testtest");
            assert_eq!(c.cluster_id, "test-cluster");
        }
        other => panic!("expected Connected, got {other:?}"),
    }
}
