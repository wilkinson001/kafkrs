use crate::fetcher::{fetch, FetchRequest};
use crate::partition_writer::{IncomingRecord, PwMsg};
use crate::topic_registry::{RegistryError, RegistryMsg};
use bincode::config as bincode_config;
use bincode::serde::{decode_from_slice, encode_to_vec};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};

/// Per-partition handles the listener routes to.
#[derive(Clone)]
pub struct PartitionHandle {
    pub pw_tx: mpsc::Sender<PwMsg>,
    pub tail: broadcast::Sender<i64>,
}

#[derive(Clone)]
pub struct SharedState {
    pub partitions: Arc<tokio::sync::RwLock<HashMap<(String, u32), PartitionHandle>>>,
    pub registry: mpsc::Sender<RegistryMsg>,
    pub store: Arc<dyn object_store::ObjectStore>,
    pub prefix: String,
    pub auto_create: bool,
    pub default_partition_count: u32,
}

#[derive(Serialize, Deserialize)]
pub enum WireRequest {
    Produce {
        topic: String,
        partition: u32,
        key: Vec<u8>,
        value: Vec<u8>,
        schema_id: u32,
        timestamp_ns: i64,
    },
    Fetch {
        topic: String,
        partition: u32,
        from_offset: i64,
        max_records: usize,
        max_wait_ms: u64,
    },
}

#[derive(Serialize, Deserialize)]
pub enum WireResponse {
    Produced {
        base_offset: i64,
    },
    Fetched {
        records: Vec<(i64, i64, u32, Vec<u8>, Vec<u8>)>,
        hwm: i64,
    },
    Error(String),
}

pub struct Listener {
    socket: TcpStream,
    state: SharedState,
}

impl Listener {
    pub fn new(socket: TcpStream, state: SharedState) -> Listener {
        Listener { socket, state }
    }

    pub async fn process(&mut self) {
        let bc = bincode_config::standard();
        loop {
            let mut len_buf: [u8; 4] = [0u8; 4];
            if self.socket.read_exact(&mut len_buf).await.is_err() {
                return; // connection closed
            }
            let len: usize = u32::from_le_bytes(len_buf) as usize;
            let mut buf: Vec<u8> = vec![0u8; len];
            if self.socket.read_exact(&mut buf).await.is_err() {
                return;
            }
            let (req, _): (WireRequest, usize) = match decode_from_slice(&buf, bc) {
                Ok(v) => v,
                Err(e) => {
                    self.write(&WireResponse::Error(format!("decode: {e}")))
                        .await;
                    continue;
                }
            };
            let resp: WireResponse = self.handle(req).await;
            self.write(&resp).await;
        }
    }

    async fn handle(&self, req: WireRequest) -> WireResponse {
        match req {
            WireRequest::Produce {
                topic,
                partition,
                key,
                value,
                schema_id,
                timestamp_ns,
            } => {
                if self.state.auto_create {
                    let (r, rr): (
                        oneshot::Sender<Result<(), RegistryError>>,
                        oneshot::Receiver<Result<(), RegistryError>>,
                    ) = oneshot::channel();
                    let _ = self
                        .state
                        .registry
                        .send(RegistryMsg::EnsureExists {
                            name: topic.clone(),
                            partition_count: self.state.default_partition_count,
                            reply: r,
                        })
                        .await;
                    let _ = rr.await;
                }
                let handle: Option<PartitionHandle> = {
                    let guard: tokio::sync::RwLockReadGuard<
                        '_,
                        HashMap<(String, u32), PartitionHandle>,
                    > = self.state.partitions.read().await;
                    guard.get(&(topic.clone(), partition)).cloned()
                };
                let Some(handle) = handle else {
                    return WireResponse::Error("UnknownTopic".into());
                };
                let (ack, ack_rx): (oneshot::Sender<i64>, oneshot::Receiver<i64>) =
                    oneshot::channel();
                if handle
                    .pw_tx
                    .send(PwMsg::Produce {
                        records: vec![IncomingRecord {
                            schema_id,
                            key,
                            value,
                            timestamp_ns,
                        }],
                        ack,
                    })
                    .await
                    .is_err()
                {
                    return WireResponse::Error("BrokerNotReady".into());
                }
                match ack_rx.await {
                    Ok(hwm) => WireResponse::Produced { base_offset: hwm },
                    Err(_) => WireResponse::Error("BrokerNotReady".into()),
                }
            }
            WireRequest::Fetch {
                topic,
                partition,
                from_offset,
                max_records,
                max_wait_ms,
            } => {
                let handle: Option<PartitionHandle> = {
                    let guard: tokio::sync::RwLockReadGuard<
                        '_,
                        HashMap<(String, u32), PartitionHandle>,
                    > = self.state.partitions.read().await;
                    guard.get(&(topic.clone(), partition)).cloned()
                };
                let Some(handle) = handle else {
                    return WireResponse::Error("UnknownTopic".into());
                };
                match fetch(
                    FetchRequest {
                        topic,
                        partition,
                        from_offset,
                        max_records,
                        max_wait_ms,
                    },
                    &handle.pw_tx,
                    &handle.tail,
                    &self.state.store,
                    &self.state.prefix,
                )
                .await
                {
                    Ok(resp) => WireResponse::Fetched {
                        records: resp
                            .records
                            .into_iter()
                            .map(|r| (r.offset, r.timestamp_ns, r.schema_id, r.key, r.value))
                            .collect(),
                        hwm: resp.hwm,
                    },
                    Err(e) => WireResponse::Error(format!("{e:?}")),
                }
            }
        }
    }

    async fn write(&mut self, resp: &WireResponse) {
        let body: Vec<u8> = encode_to_vec(resp, bincode_config::standard()).unwrap();
        let _ = self
            .socket
            .write_all(&(body.len() as u32).to_le_bytes())
            .await;
        let _ = self.socket.write_all(&body).await;
    }
}
