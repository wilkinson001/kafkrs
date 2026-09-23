//! `Metadata` handler. Read-only view of the cluster's brokers and topics.
//! Empty request filter returns all topics; non-empty filter returns matched
//! entries plus per-topic `ErrUnknownTopic` markers for names that don't
//! exist. Single-broker: the `brokers` list always contains just self.

use super::SharedState;
use crate::topic_registry::RegistryMsg;
use crate::wire::errors::make_error;
use crate::wire::frame::Frame;
use bytes::Bytes;
use kafkrs_models::topic::TopicEntry;
use kafkrs_models::wire::v1::{
    command::Body, BrokerInfo, Command, ErrorCode, MetadataResponse, PartitionMetadata,
    TopicMetadata,
};
use std::collections::{HashMap, HashSet};
use tokio::sync::oneshot;

pub async fn handle_metadata(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::MetadataRequest,
) -> Frame {
    // Snapshot the registry (one round trip).
    let (tx, rx) = oneshot::channel::<Vec<TopicEntry>>();
    if state
        .registry
        .send(RegistryMsg::Snapshot { reply: tx })
        .await
        .is_err()
    {
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
            payload: Bytes::new(),
        };
    }
    let snapshot: Vec<TopicEntry> = match rx.await {
        Ok(s) => s,
        Err(_) => {
            return Frame {
                command: make_error(correlation_id, ErrorCode::ErrBrokerNotReady, ""),
                payload: Bytes::new(),
            };
        }
    };
    let by_name: HashMap<String, TopicEntry> =
        snapshot.into_iter().map(|t| (t.name.clone(), t)).collect();

    // Determine the topic list to return.
    let leader_id = state.identity.broker_id.to_string();
    let topics: Vec<TopicMetadata> = if req.topics.is_empty() {
        // All topics.
        by_name
            .values()
            .map(|entry| topic_meta_from_entry(entry, &leader_id))
            .collect()
    } else {
        // Filtered — dedupe request while preserving first-seen order.
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out: Vec<TopicMetadata> = Vec::with_capacity(req.topics.len());
        for name in &req.topics {
            if !seen.insert(name.as_str()) {
                continue;
            }
            match by_name.get(name) {
                Some(entry) => out.push(topic_meta_from_entry(entry, &leader_id)),
                None => out.push(TopicMetadata {
                    topic: name.clone(),
                    error_code: ErrorCode::ErrUnknownTopic as u32,
                    topic_uuid: String::new(),
                    partitions: Vec::new(),
                }),
            }
        }
        out
    };

    let brokers = vec![BrokerInfo {
        broker_id: state.identity.broker_id.to_string(),
        host: state.identity.advertised_host.to_string(),
        port: state.identity.advertised_port as u32,
    }];

    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::MetadataResp(MetadataResponse {
                cluster_id: state.identity.cluster_id.to_string(),
                brokers,
                topics,
            })),
        },
        payload: Bytes::new(),
    }
}

fn topic_meta_from_entry(entry: &TopicEntry, leader_broker_id: &str) -> TopicMetadata {
    let partitions: Vec<PartitionMetadata> = (0..entry.partition_count)
        .map(|p| PartitionMetadata {
            partition: p,
            leader_broker_id: leader_broker_id.to_string(),
        })
        .collect();
    TopicMetadata {
        topic: entry.name.clone(),
        error_code: 0,
        topic_uuid: entry.uuid.clone(),
        partitions,
    }
}
