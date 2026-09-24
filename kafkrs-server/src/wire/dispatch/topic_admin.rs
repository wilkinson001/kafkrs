//! Topic-lifecycle admin RPC handlers: `CreateTopic`, `DescribeTopic`,
//! `ListTopics`, `DeleteTopic`, `AlterTopicConfig`. All share the
//! registry-actor round-trip pattern; `DeleteTopic` additionally
//! orchestrates actor shutdown + pending-delete persistence + the
//! background sweep.

use super::{model_overrides_to_wire, wire_overrides_to_model, PartitionHandle, SharedState};
use crate::partition_writer::PwMsg;
use crate::startup::spawn_partition;
use crate::topic_registry::{RegistryError, RegistryMsg};
use crate::wire::errors::{make_error, registry_error_code};
use crate::wire::frame::Frame;
use bytes::Bytes;
use kafkrs_models::topic::{
    ResolvedTopicConfig, TopicConfigOverrides as TopicConfigOverridesModel,
};
use kafkrs_models::wire::v1::{
    command::Body, Command, CreateTopicResponse, DeleteTopicResponse, DescribeTopicResponse,
    ErrorCode, ListTopicsResponse,
};
use tokio::sync::oneshot;

pub async fn handle_create_topic(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::CreateTopicRequest,
) -> Frame {
    let topic_name = req.topic.clone();
    let partition_count = req.partition_count;
    let overrides = wire_overrides_to_model(req.overrides.unwrap_or_default());
    let resolved_cfg = ResolvedTopicConfig::resolve(&overrides, state.disk_type.clone());

    let (tx, rx) = oneshot::channel::<Result<String, RegistryError>>();
    if state
        .registry
        .send(RegistryMsg::Create {
            name: req.topic,
            partition_count: req.partition_count,
            overrides,
            reply: tx,
        })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    match rx.await {
        Ok(Ok(uuid)) => {
            // Spawn partition workers so subsequent Produce/Fetch RPCs find them.
            for p in 0..partition_count {
                spawn_partition(
                    &state.data_dir,
                    &topic_name,
                    uuid.clone(),
                    p,
                    resolved_cfg,
                    state.store.clone(),
                    state.prefix.clone(),
                    state.partitions.clone(),
                    state.spawn_locks.clone(),
                )
                .await;
            }
            Frame {
                command: Command {
                    correlation_id,
                    body: Some(Body::CreateTopicResp(CreateTopicResponse {})),
                },
                payload: Bytes::new(),
            }
        }
        Ok(Err(e)) => Frame {
            command: make_error(correlation_id, registry_error_code(&e), format!("{e:?}")),
            payload: Bytes::new(),
        },
        Err(_) => Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        },
    }
}

pub async fn handle_describe_topic(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::DescribeTopicRequest,
) -> Frame {
    let (tx, rx) = oneshot::channel();
    if state
        .registry
        .send(RegistryMsg::Describe {
            name: req.topic.clone(),
            reply: tx,
        })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    // Distinguish a closed reply channel (registry actor died — client sees
    // ErrBrokerNotReady) from a legitimate "topic doesn't exist" answer
    // (ErrUnknownTopic). Prior code conflated both via `.ok().flatten()`.
    match rx.await {
        Err(_) => Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        },
        Ok(None) => Frame {
            command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
            payload: Bytes::new(),
        },
        Ok(Some(entry)) => Frame {
            command: Command {
                correlation_id,
                body: Some(Body::DescribeTopicResp(DescribeTopicResponse {
                    topic: entry.name,
                    partition_count: entry.partition_count,
                    created_at_ns: entry.created_at_ns,
                    config: Some(model_overrides_to_wire(entry.config)),
                })),
            },
            payload: Bytes::new(),
        },
    }
}

pub async fn handle_list_topics(correlation_id: u64, state: &SharedState) -> Frame {
    let (tx, rx) = oneshot::channel();
    if state
        .registry
        .send(RegistryMsg::List { reply: tx })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    let topics = rx.await.unwrap_or_default();
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::ListTopicsResp(ListTopicsResponse { topics })),
        },
        payload: Bytes::new(),
    }
}

pub async fn handle_delete_topic(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::DeleteTopicRequest,
) -> Frame {
    let topic = req.topic.clone();
    let delete_data = req.delete_data.unwrap_or(true);

    // Ask the registry to atomically remove + persist, returning the topic's
    // uuid + partition_count in the same message. Previously we did a
    // separate Describe first, which opened a Describe → Delete TOCTOU where
    // a concurrent Delete could win between the two calls. Bundling both
    // into Delete's reply closes that window.
    let (topic_uuid, partition_count) = {
        let (tx, rx) = oneshot::channel();
        if state
            .registry
            .send(RegistryMsg::Delete {
                name: topic.clone(),
                delete_data,
                reply: tx,
            })
            .await
            .is_err()
        {
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
        }
        match rx.await {
            Err(_) => {
                return Frame {
                    command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                    payload: Bytes::new(),
                };
            }
            Ok(Err(e)) => {
                return Frame {
                    command: make_error(
                        correlation_id,
                        registry_error_code(&e),
                        format!("{e:?}"),
                    ),
                    payload: Bytes::new(),
                };
            }
            Ok(Ok((uuid, pcount))) => (uuid, pcount),
        }
    };

    // Registry entry is gone. Now shut down partition actors, remove them
    // from state.partitions, clean spawn_locks, remove the WAL directory,
    // and (if delete_data) snapshot manifests + append a pending-delete
    // record + spawn the sweep.
    let mut handles_to_shutdown: Vec<(u32, PartitionHandle)> =
        Vec::with_capacity(partition_count as usize);
    {
        let mut guard = state.partitions.write().await;
        for p in 0..partition_count {
            if let Some(h) = guard.remove(&(topic.clone(), p)) {
                handles_to_shutdown.push((p, h));
            }
        }
    }

    // Send Shutdown to each partition writer and await its ack so the WAL
    // and manifest state are quiesced before this RPC responds. A timeout
    // here indicates a stuck writer — log so ops can distinguish the
    // failing incarnation from any later same-name topic (uuid identifies
    // this topic's specific instance).
    for (partition, h) in &handles_to_shutdown {
        let (ack_tx, ack_rx) = oneshot::channel();
        let _ = h.pw_tx.send(PwMsg::Shutdown { ack: ack_tx }).await;
        if tokio::time::timeout(std::time::Duration::from_secs(10), ack_rx)
            .await
            .is_err()
        {
            log::warn!(
                "partition_writer shutdown ack timeout: topic={topic} partition={partition} uuid={}",
                h.uuid
            );
        }
    }

    // Send Shutdown to each partition's Uploader and await its ack. The
    // PartitionWriter Shutdown above already ran seal_and_handoff, so any
    // final sealed batch is already enqueued on uploader_tx; draining to
    // this Shutdown message guarantees the segment PUT + manifest update
    // are durable before we snapshot manifests below (spec invariant:
    // WAL/manifest state is quiesced when the client sees success).
    for (partition, h) in &handles_to_shutdown {
        let (ack_tx, ack_rx) = oneshot::channel();
        let _ = h
            .uploader_tx
            .send(crate::uploader::UploaderMsg::Shutdown { ack: ack_tx })
            .await;
        if tokio::time::timeout(std::time::Duration::from_secs(10), ack_rx)
            .await
            .is_err()
        {
            log::warn!(
                "uploader shutdown ack timeout: topic={topic} partition={partition} uuid={}",
                h.uuid
            );
        }
    }

    // Clean up spawn_locks entries for this topic's partitions.
    {
        let mut locks = state.spawn_locks.lock().unwrap();
        for p in 0..partition_count {
            locks.remove(&(topic.clone(), p));
        }
    }

    if delete_data {
        use crate::deletion::sweep_deletion;
        use crate::object_store::{get, manifest_key};
        use crate::pending_deletes::{append, PendingDelete};
        use kafkrs_models::manifest::Manifest;
        use std::collections::BTreeMap;

        // Remove the WAL directory for this topic. Gated on delete_data
        // because `delete_data = false` is "detach" semantics per spec:
        // WAL files and object-store data are left untouched for the user
        // to handle out-of-band.
        let wal_dir = std::path::Path::new(&state.data_dir)
            .join("wal")
            .join(&topic);
        if wal_dir.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&wal_dir).await {
                log::warn!("failed to remove WAL dir {}: {e:?}", wal_dir.display());
            }
        }

        let mut manifests_by_partition: BTreeMap<u32, Manifest> = BTreeMap::new();
        for p in 0..partition_count {
            let mkey = manifest_key(&state.prefix, &topic, &topic_uuid, p);
            match get(&state.store, &mkey).await {
                Ok(bytes) => {
                    if let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) {
                        manifests_by_partition.insert(p, m);
                    }
                }
                Err(_) => {
                    // Manifest missing (partition may never have uploaded).
                    manifests_by_partition.insert(p, Manifest::empty(&topic, p));
                }
            }
        }

        let record = PendingDelete {
            topic: topic.clone(),
            uuid: topic_uuid.clone(),
            manifests_by_partition,
            created_ns: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0),
        };
        if let Err(e) = append(&state.data_dir, record.clone()).await {
            log::warn!("failed to persist pending_deletes.json: {e:?}");
            return Frame {
                command: make_error(
                    correlation_id,
                    ErrorCode::ErrInternal,
                    format!("pending_deletes persistence failed: {e:?}"),
                ),
                payload: Bytes::new(),
            };
        }

        metrics::gauge!(crate::metrics::DELETE_PENDING_TOPICS).increment(1.0);

        let store = state.store.clone();
        let prefix = state.prefix.clone();
        let data_dir = state.data_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = sweep_deletion(record, store, prefix, data_dir).await {
                log::warn!("sweep_deletion failed: {e:?}");
                // Pending record stays in pending_deletes.json; next boot replays.
            }
        });
    }

    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::DeleteTopicResp(DeleteTopicResponse {})),
        },
        payload: Bytes::new(),
    }
}

pub async fn handle_alter_topic_config(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::AlterTopicConfigRequest,
) -> Frame {
    let topic = req.topic.clone();
    let patch = wire_overrides_to_model(req.overrides.unwrap_or_default());

    // Registry does merge + validate + persist. On success it returns the
    // fully-merged overrides so we can resolve them for the actors.
    let (tx, rx) = oneshot::channel::<Result<TopicConfigOverridesModel, RegistryError>>();
    if state
        .registry
        .send(RegistryMsg::Alter {
            name: topic.clone(),
            patch,
            reply: tx,
        })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    let merged = match rx.await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            return Frame {
                command: make_error(correlation_id, registry_error_code(&e), format!("{e:?}")),
                payload: Bytes::new(),
            };
        }
        Err(_) => {
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
        }
    };

    // Resolve merged overrides against the broker's disk profile so the
    // actors can just swap their `cfg` field.
    let new_cfg = ResolvedTopicConfig::resolve(&merged, state.disk_type.clone());

    // Snapshot the handles under a read lock (do NOT hold across the mpsc
    // sends). A DeleteTopic that races removes the entry; the send fails
    // silently. An auto-create that races spawns actors from the
    // already-persisted new config in topics.json, so they start correct.
    let handles: Vec<PartitionHandle> = {
        let guard = state.partitions.read().await;
        guard
            .iter()
            .filter(|((t, _p), _)| t == &topic)
            .map(|(_, h)| h.clone())
            .collect()
    };
    for h in &handles {
        let _ = h.pw_tx.send(PwMsg::UpdateConfig(new_cfg)).await;
        let _ = h
            .uploader_tx
            .send(crate::uploader::UploaderMsg::UpdateConfig(new_cfg))
            .await;
    }

    // Now take a write lock and update PartitionHandle.cfg on every
    // matching entry. A concurrent auto-create/delete that mutates the
    // map between the read snapshot and the write scan is fine — we
    // just walk whatever's present.
    {
        let mut guard = state.partitions.write().await;
        for ((t, _p), h) in guard.iter_mut() {
            if t == &topic {
                h.cfg = new_cfg;
            }
        }
    }

    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::AlterTopicConfigResp(
                kafkrs_models::wire::v1::AlterTopicConfigResponse {
                    overrides: Some(model_overrides_to_wire(merged)),
                },
            )),
        },
        payload: Bytes::new(),
    }
}
