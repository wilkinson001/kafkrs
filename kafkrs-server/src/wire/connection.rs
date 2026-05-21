//! Per-connection state machine. Spawns three cooperating tasks:
//!
//! - Reader: pulls frames off the socket, decodes them, forwards Commands to
//!   the dispatcher.
//! - Dispatcher: enforces the Connect handshake, spawns a per-RPC task for
//!   each subsequent Command so a long-polling Fetch does not block other
//!   RPCs on the same connection.
//! - Writer: drains a response-Command mpsc and writes one frame at a time so
//!   no two responses interleave bytes on the socket.

use crate::wire::dispatch::{
    handle_connected, handle_create_topic, handle_describe_topic, handle_fetch, handle_list_topics,
    handle_ping, handle_produce, SharedState, PROTOCOL_VERSION,
};
use crate::wire::errors::make_error;
use crate::wire::frame::{decode_frame_body, encode_frame, Frame, MAX_FRAME_SIZE};
use bytes::Bytes;
use kafkrs_models::wire::v1::{command::Body, ErrorCode};
use log::{debug, error, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

const CONNECTION_RESPONSE_BUFFER: usize = 256;
const CONNECTION_REQUEST_BUFFER: usize = 256;

/// Accept loop bound to one TCP listener. Spawns one connection task per
/// accepted socket. Replaces the loop in `main.rs` that called
/// `Listener::new(...).process()`.
pub async fn accept_loop(listener: TcpListener, state: SharedState) {
    loop {
        match listener.accept().await {
            Ok((socket, peer)) => {
                debug!("accepted connection from {peer}");
                let st = state.clone();
                tokio::spawn(async move {
                    run_connection(socket, st).await;
                    debug!("connection {peer} closed");
                });
            }
            Err(e) => error!("accept error: {e}"),
        }
    }
}

async fn run_connection(socket: tokio::net::TcpStream, state: SharedState) {
    let (rd, mut wr) = socket.into_split();
    // Reader uses LengthDelimitedCodec to strip the outer total_size prefix.
    // The bytes it hands us start with command_size + protobuf + payload.
    let codec = LengthDelimitedCodec::builder()
        .length_field_offset(0)
        .length_field_length(4)
        .length_field_type::<u32>()
        .big_endian()
        .max_frame_length(MAX_FRAME_SIZE)
        .new_codec();
    let mut frames = FramedRead::new(rd, codec);

    // Channel: requests reader -> dispatcher.
    let (req_tx, mut req_rx) = mpsc::channel::<Frame>(CONNECTION_REQUEST_BUFFER);
    // Channel: responses (from per-RPC tasks) -> writer.
    let (resp_tx, mut resp_rx) = mpsc::channel::<Frame>(CONNECTION_RESPONSE_BUFFER);

    // Writer task: drain resp_rx, encode, write. Serializes socket writes.
    let writer = tokio::spawn(async move {
        while let Some(frame) = resp_rx.recv().await {
            let bytes = match encode_frame(&frame) {
                Ok(b) => b,
                Err(e) => {
                    error!("failed to encode outbound frame: {e}; closing connection");
                    break;
                }
            };
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    // Reader task: pull frames off socket -> req_tx.
    let req_tx_for_reader = req_tx.clone();
    let reader = tokio::spawn(async move {
        while let Some(item) = frames.next().await {
            let body = match item {
                Ok(b) => b,
                Err(e) => {
                    warn!("framing error: {e}");
                    break;
                }
            };
            let frame = match decode_frame_body(&body) {
                Ok(f) => f,
                Err(e) => {
                    warn!("frame decode error: {e}");
                    // Send a malformed-frame error with correlation_id = 0
                    // (we could not decode the inbound command).
                    let _ = req_tx_for_reader
                        .send(Frame {
                            command: make_error(0, ErrorCode::ErrMalformedFrame, format!("{e}")),
                            payload: Bytes::new(),
                        })
                        .await;
                    break;
                }
            };
            if req_tx_for_reader.send(frame).await.is_err() {
                break;
            }
        }
        drop(req_tx_for_reader);
    });
    drop(req_tx); // dispatcher closes req_rx when reader's clone is dropped

    // Dispatcher: state machine + per-RPC spawn.
    let mut connected = false;
    while let Some(frame) = req_rx.recv().await {
        let cid = frame.command.correlation_id;
        // Malformed-frame sentinel from the reader: forward to writer and exit.
        if matches!(frame.command.body, Some(Body::Error(_))) {
            let _ = resp_tx.send(frame).await;
            break;
        }
        match (connected, frame.command.body.clone()) {
            (false, Some(Body::Connect(req))) => {
                if req.protocol_version != PROTOCOL_VERSION {
                    let _ = resp_tx
                        .send(Frame {
                            command: make_error(
                                cid,
                                ErrorCode::ErrUnsupportedProtocolVersion,
                                format!(
                                    "server speaks protocol_version={}; client requested {}",
                                    PROTOCOL_VERSION, req.protocol_version
                                ),
                            ),
                            payload: Bytes::new(),
                        })
                        .await;
                    break;
                }
                let _ = resp_tx.send(handle_connected(cid)).await;
                connected = true;
            }
            (false, _) => {
                let _ = resp_tx
                    .send(Frame {
                        command: make_error(
                            cid,
                            ErrorCode::ErrHandshakeRequired,
                            "first frame must be Connect",
                        ),
                        payload: Bytes::new(),
                    })
                    .await;
                break;
            }
            (true, Some(Body::Connect(_))) => {
                let _ = resp_tx
                    .send(Frame {
                        command: make_error(cid, ErrorCode::ErrAlreadyConnected, ""),
                        payload: Bytes::new(),
                    })
                    .await;
                break;
            }
            (true, Some(body)) => {
                // Spawn a per-RPC task so long-poll fetches don't block others.
                let resp_tx = resp_tx.clone();
                let state = state.clone();
                let payload = frame.payload.clone();
                // TODO: abort in-flight per-RPC tasks on connection teardown rather than
                // relying on resp_tx.send returning Err. A long-poll Fetch can hold a task
                // alive for up to max_wait_ms after the client disconnects.
                tokio::spawn(async move {
                    let response = dispatch_one(cid, body, payload, &state).await;
                    let _ = resp_tx.send(response).await;
                });
            }
            (true, None) => {
                let _ = resp_tx
                    .send(Frame {
                        command: make_error(
                            cid,
                            ErrorCode::ErrInvalidCommand,
                            "command body absent",
                        ),
                        payload: Bytes::new(),
                    })
                    .await;
            }
        }
    }

    // Drop resp_tx so the writer's loop completes.
    drop(resp_tx);
    let _ = reader.await;
    let _ = writer.await;
}

async fn dispatch_one(
    correlation_id: u64,
    body: Body,
    payload: Bytes,
    state: &SharedState,
) -> Frame {
    match body {
        Body::Ping(_) => handle_ping(correlation_id),
        Body::Produce(req) => {
            handle_produce(
                correlation_id,
                state,
                req.topic,
                req.partition,
                req.records,
                payload,
            )
            .await
        }
        Body::Fetch(req) => handle_fetch(correlation_id, state, req).await,
        Body::CreateTopic(req) => handle_create_topic(correlation_id, state, req).await,
        Body::DescribeTopic(req) => handle_describe_topic(correlation_id, state, req).await,
        Body::ListTopics(_) => handle_list_topics(correlation_id, state).await,
        // Variants the broker should never receive as a request:
        Body::Connect(_)
        | Body::Connected(_)
        | Body::Pong(_)
        | Body::ProduceResp(_)
        | Body::FetchResp(_)
        | Body::CreateTopicResp(_)
        | Body::DescribeTopicResp(_)
        | Body::ListTopicsResp(_)
        | Body::Error(_) => Frame {
            command: make_error(
                correlation_id,
                ErrorCode::ErrInvalidCommand,
                "this command is not valid as a request",
            ),
            payload: Bytes::new(),
        },
    }
}
