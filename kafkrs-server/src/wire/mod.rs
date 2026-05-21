//! TCP wire protocol implementation.
//!
//! See `docs/superpowers/specs/2026-05-20-wire-protocol-design.md`.

pub mod connection;
pub mod dispatch;
pub mod errors;
pub mod frame;

pub use connection::accept_loop;
pub use dispatch::{PartitionHandle, SharedState};
