//! Connection-lifecycle RPC handlers: `Ping`/`Pong` and the post-handshake
//! `Connected` response. Cheap synchronous handlers with no registry or
//! actor state to touch.

use super::{SharedState, PROTOCOL_VERSION};
use crate::wire::frame::Frame;
use bytes::Bytes;
use kafkrs_models::wire::v1::{command::Body, Command, ConnectedResponse, PongResponse};

pub fn handle_ping(correlation_id: u64) -> Frame {
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::Pong(PongResponse {})),
        },
        payload: Bytes::new(),
    }
}

pub fn handle_connected(correlation_id: u64, state: &SharedState) -> Frame {
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::Connected(ConnectedResponse {
                protocol_version: PROTOCOL_VERSION,
                broker_id: state.identity.broker_id.to_string(),
                cluster_id: state.identity.cluster_id.to_string(),
            })),
        },
        payload: Bytes::new(),
    }
}
