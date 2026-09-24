//! Metrics module. Installs a global Prometheus recorder at boot when
//! `ports.metrics` is set; otherwise remains a no-op via the default
//! recorder shipped by the `metrics` crate.
//!
//! Every metric name and every label key used across the broker is
//! defined as a `pub const` here. Call sites reference the constants,
//! never string literals — this keeps the metrics catalogue in one
//! place and gives us compile-time typo detection.
//!
//! See `docs/superpowers/specs/2026-09-21-metrics-design.md`.

mod admin;
mod describe;
mod labels;
pub mod names;
mod runtime;

pub use admin::set_ready;
pub use labels::{partition_label, partition_label_with};
pub use names::*;
pub use runtime::uptime_updater;

use kafkrs_models::config::PortsConfig;

/// Install the global Prometheus recorder (when `ports.metrics` is set) and
/// spawn any admin-port HTTP listeners implied by `ports.metrics` /
/// `ports.health`:
///
/// - Neither set → no listeners, no recorder. No-op.
/// - Only `metrics` set → one listener on that port, serving only `/metrics`.
/// - Only `health` set → one listener on that port, serving only `/health` +
///   `/ready`. No recorder installed.
/// - Both set to different ports → two listeners, each serving its own subset.
/// - Both set to the same port value → one listener serving the merged route
///   set (`/metrics` + `/health` + `/ready`).
///
/// Does not spawn the uptime-updater background task itself — `init` must
/// stay usable without an ambient tokio runtime (see the test helper in
/// `tests/wire_e2e.rs`, which calls `init` from a bare `std::thread`).
/// Callers running under a tokio runtime (i.e. `main.rs`) should spawn
/// [`uptime_updater`] themselves alongside the `init` call.
pub fn init(ports: &PortsConfig, high_cardinality: bool) -> anyhow::Result<()> {
    labels::set_high_cardinality(high_cardinality);

    // If metrics is opted in, install the recorder and grab its handle.
    let prom_handle = if let Some(_port) = ports.metrics {
        use metrics_exporter_prometheus::PrometheusBuilder;
        let handle = PrometheusBuilder::new()
            .set_buckets(LATENCY_BUCKETS_MS)?
            .install_recorder()?;
        runtime::install();
        describe::describe_all();
        Some(handle)
    } else {
        None
    };

    admin::install(ports, prom_handle)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_with_no_admin_ports_is_noop() {
        let ports = PortsConfig {
            wire: vec![5432],
            metrics: None,
            health: None,
        };
        assert!(init(&ports, false).is_ok());
    }
}
