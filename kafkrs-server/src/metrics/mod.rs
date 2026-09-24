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

mod describe;
mod labels;
pub mod names;
mod runtime;

pub use labels::{partition_label, partition_label_with};
pub use names::*;
pub use runtime::uptime_updater;

use kafkrs_models::config::PortsConfig;
use std::sync::atomic::{AtomicBool, Ordering};

/// Readiness flag: `false` until [`set_ready`] flips it. Read by the `/ready`
/// admin-port route to distinguish "process alive but not yet accepting
/// traffic" (503) from "ready to serve" (200). `main.rs` calls
/// `set_ready(true)` immediately after spawning the wire listeners.
static READY: AtomicBool = AtomicBool::new(false);

/// Mark the broker as ready (or not-ready) to serve traffic.
///
/// Called by `main.rs` after wire listeners are bound. In tests, only the
/// dedicated ready-endpoint test calls this — no other test touches the flag.
pub fn set_ready(ready: bool) {
    READY.store(ready, Ordering::Relaxed);
}

/// Read the current readiness flag. Primarily for the `/ready` handler and
/// its unit test.
pub(crate) fn is_ready() -> bool {
    READY.load(Ordering::Relaxed)
}

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

    // Plan (port, serves_metrics, serves_health) for each unique admin port.
    let listeners = plan_admin_listeners(ports);

    // Spawn one bare `std::thread` per listener so each owns its own
    // tokio runtime — same rationale as retention/metrics tests: callers
    // of `init` from a `#[tokio::test]` context tear down their runtime
    // with the test, which would kill an inherited-runtime listener.
    for (port, serves_metrics, serves_health) in listeners {
        let addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
        let handle = if serves_metrics {
            prom_handle.clone()
        } else {
            None
        };
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("admin-port {port} runtime build failed: {e:?}");
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) = serve_admin(addr, handle, serves_health).await {
                    log::error!("admin-port {port} listener exited: {e:?}");
                }
            });
        });
    }

    Ok(())
}

/// Given a `PortsConfig`, decide which admin-port HTTP listeners `init`
/// should spawn. Each returned entry is `(port, serves_metrics,
/// serves_health)`. Deduped by port value so a single config with
/// `metrics == health` yields one merged listener.
fn plan_admin_listeners(ports: &PortsConfig) -> Vec<(u16, bool, bool)> {
    let mut listeners: Vec<(u16, bool, bool)> = Vec::new();
    if let Some(mp) = ports.metrics {
        listeners.push((mp, true, false));
    }
    if let Some(hp) = ports.health {
        if let Some(entry) = listeners.iter_mut().find(|(p, _, _)| *p == hp) {
            entry.2 = true;
        } else {
            listeners.push((hp, false, true));
        }
    }
    listeners
}

/// Accept loop for an admin HTTP endpoint. Routes served depend on the
/// listener's role:
///
/// - `GET /metrics` → 200 with Prometheus exposition (when `handle` is `Some`).
/// - `GET /health` → 200 `ok` (when `serve_health` is `true`).
/// - `GET /ready` → 200 `ok` if [`is_ready`], else 503 `not ready` (when `serve_health` is `true`).
/// - anything else, including routes this listener isn't configured to serve → 404.
///
/// Deliberately hand-rolled instead of pulling in `hyper` or `axum` — we
/// serve at most three static routes with fixed responses; the entire
/// request-parse path fits inline.
async fn serve_admin(
    addr: std::net::SocketAddr,
    handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    serve_health: bool,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    loop {
        let (sock, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("admin accept error: {e:?}");
                continue;
            }
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_admin_conn(sock, handle, serve_health).await {
                log::debug!("admin conn error: {e:?}");
            }
        });
    }
}

async fn handle_admin_conn(
    mut sock: tokio::net::TcpStream,
    handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    serve_health: bool,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read up to the end of the request line — we don't need headers or body
    // for any of our routes. Bounded to 8 KiB so a hostile client can't DoS
    // us into unbounded allocation.
    let mut buf = [0u8; 8192];
    let mut filled = 0usize;
    loop {
        if filled == buf.len() {
            let _ = sock
                .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                .await;
            return Ok(());
        }
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sock.read(&mut buf[filled..]),
        )
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "admin read timeout"))??;
        if n == 0 {
            return Ok(());
        }
        filled += n;
        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf[..filled].contains(&b'\n') && buf[..filled].contains(&b'\r') {
            // Have at least one line; try to parse the request line even
            // without full headers so short requests don't hang.
            break;
        }
    }
    let request = std::str::from_utf8(&buf[..filled]).unwrap_or("");
    let request_line = request.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let response = match (method, path) {
        ("GET", "/metrics") if handle.is_some() => {
            let body = handle.as_ref().unwrap().render();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        }
        ("GET", "/health") if serve_health => {
            let body = "ok";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        }
        ("GET", "/ready") if serve_health => {
            let (status, body) = if is_ready() {
                ("200 OK", "ok")
            } else {
                ("503 Service Unavailable", "not ready")
            };
            format!(
                "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                body.len(),
                body
            )
        }
        _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    };
    sock.write_all(response.as_bytes()).await?;
    sock.shutdown().await.ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_ready_toggles_flag() {
        // Test-local: keep the global static in a known state around this test.
        // No cross-test synchronization needed because no other unit test
        // reads or writes READY.
        set_ready(false);
        assert!(!is_ready(), "expected READY=false after set_ready(false)");
        set_ready(true);
        assert!(is_ready(), "expected READY=true after set_ready(true)");
        set_ready(false); // reset for cleanliness
    }

    #[test]
    fn init_with_no_admin_ports_is_noop() {
        let ports = PortsConfig {
            wire: vec![5432],
            metrics: None,
            health: None,
        };
        assert!(init(&ports, false).is_ok());
    }

    #[test]
    fn plan_admin_listeners_none_when_both_off() {
        let ports = PortsConfig {
            wire: vec![5432],
            metrics: None,
            health: None,
        };
        assert_eq!(
            plan_admin_listeners(&ports),
            Vec::<(u16, bool, bool)>::new()
        );
    }

    #[test]
    fn plan_admin_listeners_metrics_only() {
        let ports = PortsConfig {
            wire: vec![5432],
            metrics: Some(9464),
            health: None,
        };
        assert_eq!(plan_admin_listeners(&ports), vec![(9464, true, false)]);
    }

    #[test]
    fn plan_admin_listeners_health_only() {
        let ports = PortsConfig {
            wire: vec![5432],
            metrics: None,
            health: Some(9465),
        };
        assert_eq!(plan_admin_listeners(&ports), vec![(9465, false, true)]);
    }

    #[test]
    fn plan_admin_listeners_merges_same_port() {
        let ports = PortsConfig {
            wire: vec![5432],
            metrics: Some(9464),
            health: Some(9464),
        };
        // One listener serving both.
        assert_eq!(plan_admin_listeners(&ports), vec![(9464, true, true)]);
    }

    #[test]
    fn plan_admin_listeners_splits_different_ports() {
        let ports = PortsConfig {
            wire: vec![5432],
            metrics: Some(9464),
            health: Some(9465),
        };
        assert_eq!(
            plan_admin_listeners(&ports),
            vec![(9464, true, false), (9465, false, true)]
        );
    }
}
