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

use kafkrs_models::config::PortsConfig;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

// ---------- Metric name constants (37 metrics total) ----------

// Wire path — produce
pub const PRODUCE_RECORDS: &str = "messaging.kafkrs.produce.records";
pub const PRODUCE_BYTES: &str = "messaging.kafkrs.produce.bytes";
pub const PRODUCE_LATENCY_MS: &str = "messaging.kafkrs.produce.latency_ms";
pub const PRODUCE_ERRORS: &str = "messaging.kafkrs.produce.errors";

// Wire path — fetch
pub const FETCH_REQUESTS: &str = "messaging.kafkrs.fetch.requests";
pub const FETCH_RECORDS: &str = "messaging.kafkrs.fetch.records";
pub const FETCH_BYTES: &str = "messaging.kafkrs.fetch.bytes";
pub const FETCH_LATENCY_MS: &str = "messaging.kafkrs.fetch.latency_ms";
pub const FETCH_LONG_POLL_WAIT_MS: &str = "messaging.kafkrs.fetch.long_poll_wait_ms";
pub const FETCH_SOURCE: &str = "messaging.kafkrs.fetch.source";
pub const FETCH_ERRORS: &str = "messaging.kafkrs.fetch.errors";

// Uploader
pub const UPLOADER_SEGMENTS_UPLOADED: &str = "kafkrs.uploader.segments_uploaded";
pub const UPLOADER_BYTES_UPLOADED: &str = "kafkrs.uploader.bytes_uploaded";
pub const UPLOADER_UPLOAD_LATENCY_MS: &str = "kafkrs.uploader.upload_latency_ms";
pub const UPLOADER_UPLOAD_RETRIES: &str = "kafkrs.uploader.upload_retries";

// Retention
pub const RETENTION_SEGMENTS_EVICTED: &str = "kafkrs.retention.segments_evicted";
pub const RETENTION_BYTES_EVICTED: &str = "kafkrs.retention.bytes_evicted";
pub const RETENTION_PASSES: &str = "kafkrs.retention.passes";
pub const RETENTION_DELETE_FAILURES: &str = "kafkrs.retention.delete_failures";
pub const RETENTION_SWEEP_KICKS_SENT: &str = "kafkrs.retention.sweep_kicks_sent";
pub const RETENTION_SWEEP_KICKS_DROPPED: &str = "kafkrs.retention.sweep_kicks_dropped";

// Partition state
pub const PARTITION_COUNT: &str = "kafkrs.partition.count";
pub const PARTITION_RECORDS_IN_MEMORY: &str = "kafkrs.partition.records_in_memory";
pub const PARTITION_BYTES_IN_MEMORY: &str = "kafkrs.partition.bytes_in_memory";
pub const PARTITION_SEGMENTS_UPLOADED: &str = "kafkrs.partition.segments_uploaded";
pub const PARTITION_HWM_OFFSET: &str = "kafkrs.partition.hwm_offset";
pub const PARTITION_WAL_FILES: &str = "kafkrs.partition.wal_files";

// Runtime + wire
pub const RUNTIME_UPTIME_SECONDS: &str = "kafkrs.runtime.uptime_seconds";
pub const RUNTIME_BUILD_INFO: &str = "kafkrs.runtime.build_info";
pub const WIRE_CONNECTIONS_ACTIVE: &str = "kafkrs.wire.connections_active";
pub const WIRE_CONNECTIONS_ACCEPTED: &str = "kafkrs.wire.connections_accepted";
pub const WIRE_RPC_REQUESTS: &str = "kafkrs.wire.rpc_requests";

// Deletion sweep
pub const DELETE_PENDING_TOPICS: &str = "kafkrs.delete.pending_topics";
pub const DELETE_SEGMENTS_REMOVED: &str = "kafkrs.delete.segments_removed";
pub const DELETE_BYTES_REMOVED: &str = "kafkrs.delete.bytes_removed";
pub const DELETE_DURATION_MS: &str = "kafkrs.delete.duration_ms";
pub const DELETE_ERRORS: &str = "kafkrs.delete.errors";

// ---------- Label key constants ----------
pub const LABEL_TOPIC: &str = "topic";
pub const LABEL_PARTITION: &str = "partition";
pub const LABEL_ERROR_CODE: &str = "error_code";
pub const LABEL_SOURCE: &str = "source";
pub const LABEL_TRIGGER: &str = "trigger";
pub const LABEL_RPC: &str = "rpc";
pub const LABEL_VERSION: &str = "version";
pub const LABEL_GIT_SHA: &str = "git_sha";

// ---------- Shared config ----------

/// One shared bucket set for every latency histogram in the broker.
/// Covers 0.5ms (in-memory fetch tail) through 5s (slow object-store upload).
pub const LATENCY_BUCKETS_MS: &[f64] = &[
    0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0,
];

/// Set once at `init`; read from `partition_label`.
static HIGH_CARDINALITY: AtomicBool = AtomicBool::new(false);

/// Process start time, set once at `init`. Read by `uptime_updater`.
static START_TIME: OnceLock<Instant> = OnceLock::new();

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

/// Install the global Prometheus recorder and start the HTTP scrape listener.
/// No-op (returns `Ok(())`) when `ports.metrics` is `None`.
///
/// Does not spawn the uptime-updater background task itself — `init` must
/// stay usable without an ambient tokio runtime (see the test helper in
/// `tests/wire_e2e.rs`, which calls `init` from a bare `std::thread`).
/// Callers running under a tokio runtime (i.e. `main.rs`) should spawn
/// [`uptime_updater`] themselves alongside the `init` call.
pub fn init(ports: &PortsConfig, high_cardinality: bool) -> anyhow::Result<()> {
    HIGH_CARDINALITY.store(high_cardinality, Ordering::Relaxed);
    let Some(port) = ports.metrics else {
        return Ok(());
    };
    use metrics_exporter_prometheus::PrometheusBuilder;
    use std::net::SocketAddr;

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    // `install_recorder()` installs the global recorder and returns a handle
    // whose `.render()` produces the Prometheus exposition text on demand.
    // We skip the crate's `.with_http_listener(...)` so we can serve
    // `/metrics`, `/health`, and `/ready` from a single hand-rolled
    // dispatcher on the same port.
    let handle = PrometheusBuilder::new()
        .set_buckets(LATENCY_BUCKETS_MS)?
        .install_recorder()?;
    START_TIME.set(Instant::now()).ok();
    describe_all();
    emit_build_info();

    // Spawn the admin-port HTTP listener on a bare `std::thread` so it owns
    // its own tokio runtime — same rationale as retention/metrics tests:
    // callers of `init` from a `#[tokio::test]` context tear down their
    // runtime with the test, which would kill an inherited-runtime listener.
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("metrics admin-port runtime build failed: {e:?}");
                return;
            }
        };
        rt.block_on(async move {
            if let Err(e) = serve_admin(addr, handle).await {
                log::error!("metrics admin-port listener exited: {e:?}");
            }
        });
    });

    Ok(())
}

/// Accept loop for the admin HTTP endpoint. Routes:
///
/// - `GET /metrics` → 200, `text/plain; version=0.0.4`, body from the Prometheus handle.
/// - `GET /health`  → 200, `text/plain`, body `ok`.
/// - `GET /ready`   → 200 `ok` if [`is_ready`], else 503 `not ready`.
/// - anything else  → 404.
///
/// Deliberately hand-rolled instead of pulling in `hyper` or `axum` — we
/// serve three static routes with fixed responses; the entire request-parse
/// path fits inline.
async fn serve_admin(
    addr: std::net::SocketAddr,
    handle: metrics_exporter_prometheus::PrometheusHandle,
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
            if let Err(e) = handle_admin_conn(sock, handle).await {
                log::debug!("admin conn error: {e:?}");
            }
        });
    }
}

async fn handle_admin_conn(
    mut sock: tokio::net::TcpStream,
    handle: metrics_exporter_prometheus::PrometheusHandle,
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
        ("GET", "/metrics") => {
            let body = handle.render();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        }
        ("GET", "/health") => {
            let body = "ok";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        }
        ("GET", "/ready") => {
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

/// Emit `RUNTIME_BUILD_INFO` once at boot: a constant-1 gauge carrying the
/// broker's version and (optionally, compile-time-injected) git SHA as
/// labels. Scraping this metric family tells you which build is running.
fn emit_build_info() {
    let version = env!("CARGO_PKG_VERSION");
    let git_sha = option_env!("KAFKRS_GIT_SHA").unwrap_or("unknown");
    metrics::gauge!(
        RUNTIME_BUILD_INFO,
        LABEL_VERSION => version,
        LABEL_GIT_SHA => git_sha
    )
    .set(1.0);
}

/// Background task: every 10s, set `RUNTIME_UPTIME_SECONDS` to the elapsed
/// time since `init` recorded `START_TIME`. Must be spawned by the caller
/// on a live tokio runtime after `init` returns — see [`init`]'s doc comment.
pub async fn uptime_updater() {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        ticker.tick().await;
        if let Some(start) = START_TIME.get() {
            let uptime = start.elapsed().as_secs_f64();
            metrics::gauge!(RUNTIME_UPTIME_SECONDS).set(uptime);
        }
    }
}

/// Register HELP text and units for every metric the broker emits.
/// Called once from `init` after the recorder is installed.
/// Every metric constant listed above must have an entry here.
pub(crate) fn describe_all() {
    // Produce
    metrics::describe_counter!(PRODUCE_RECORDS, "Records accepted by produce");
    metrics::describe_counter!(
        PRODUCE_BYTES,
        metrics::Unit::Bytes,
        "Bytes accepted by produce (key + value)"
    );
    metrics::describe_histogram!(
        PRODUCE_LATENCY_MS,
        metrics::Unit::Milliseconds,
        "Server-side produce handling time"
    );
    metrics::describe_counter!(PRODUCE_ERRORS, "Produce responses with an error");

    // Fetch
    metrics::describe_counter!(FETCH_REQUESTS, "Fetch RPCs served");
    metrics::describe_counter!(FETCH_RECORDS, "Records returned by fetch");
    metrics::describe_counter!(FETCH_BYTES, metrics::Unit::Bytes, "Bytes returned by fetch");
    metrics::describe_histogram!(
        FETCH_LATENCY_MS,
        metrics::Unit::Milliseconds,
        "Server-side fetch handling time (excludes long-poll wait)"
    );
    metrics::describe_histogram!(
        FETCH_LONG_POLL_WAIT_MS,
        metrics::Unit::Milliseconds,
        "Time spent waiting for tail data on a long-poll fetch"
    );
    metrics::describe_counter!(
        FETCH_SOURCE,
        "Where fetched data came from (memory/wal/object_store)"
    );
    metrics::describe_counter!(FETCH_ERRORS, "Fetch responses with an error");

    // Uploader
    metrics::describe_counter!(
        UPLOADER_SEGMENTS_UPLOADED,
        "Segments successfully uploaded to object store"
    );
    metrics::describe_counter!(
        UPLOADER_BYTES_UPLOADED,
        metrics::Unit::Bytes,
        "Parquet bytes uploaded"
    );
    metrics::describe_histogram!(
        UPLOADER_UPLOAD_LATENCY_MS,
        metrics::Unit::Milliseconds,
        "Time from SealedBatch received to manifest updated"
    );
    metrics::describe_counter!(
        UPLOADER_UPLOAD_RETRIES,
        "Times an upload hit the retry loop"
    );

    // Retention
    metrics::describe_counter!(RETENTION_SEGMENTS_EVICTED, "Segments deleted by retention");
    metrics::describe_counter!(
        RETENTION_BYTES_EVICTED,
        metrics::Unit::Bytes,
        "Sum of byte_size of evicted segments"
    );
    metrics::describe_counter!(
        RETENTION_PASSES,
        "Retention passes run (labelled by trigger)"
    );
    metrics::describe_counter!(
        RETENTION_DELETE_FAILURES,
        "Object-store DELETE calls that failed (orphans accepted)"
    );
    metrics::describe_counter!(
        RETENTION_SWEEP_KICKS_SENT,
        "Sweeper kicks enqueued (broker-wide)"
    );
    metrics::describe_counter!(
        RETENTION_SWEEP_KICKS_DROPPED,
        "Sweeper kicks dropped due to backpressure (broker-wide)"
    );

    // Partition state
    metrics::describe_gauge!(PARTITION_COUNT, "Number of active partitions on the broker");
    metrics::describe_gauge!(
        PARTITION_RECORDS_IN_MEMORY,
        "Records currently held in a partition's active batch (per topic)"
    );
    metrics::describe_gauge!(
        PARTITION_BYTES_IN_MEMORY,
        metrics::Unit::Bytes,
        "Bytes currently held in a partition's active batch (per topic)"
    );
    metrics::describe_gauge!(
        PARTITION_SEGMENTS_UPLOADED,
        "Number of segments in the manifest per topic"
    );
    metrics::describe_gauge!(PARTITION_HWM_OFFSET, "High-water-mark offset per topic");
    metrics::describe_gauge!(
        PARTITION_WAL_FILES,
        "WAL files present on disk per topic (pre-upload)"
    );

    // Runtime
    metrics::describe_gauge!(
        RUNTIME_UPTIME_SECONDS,
        metrics::Unit::Seconds,
        "Broker process uptime in seconds"
    );
    metrics::describe_gauge!(
        RUNTIME_BUILD_INFO,
        "Static broker identity; value is always 1"
    );

    // Wire
    metrics::describe_gauge!(WIRE_CONNECTIONS_ACTIVE, "Currently open TCP connections");
    metrics::describe_counter!(
        WIRE_CONNECTIONS_ACCEPTED,
        "Cumulative TCP connections accepted"
    );
    metrics::describe_counter!(
        WIRE_RPC_REQUESTS,
        "RPCs served, by RPC kind and result code"
    );

    // Deletion sweep
    metrics::describe_gauge!(
        DELETE_PENDING_TOPICS,
        "Currently in-flight deletion sweeps (broker-wide)"
    );
    metrics::describe_counter!(
        DELETE_SEGMENTS_REMOVED,
        "Segments deleted by deletion sweeps"
    );
    metrics::describe_counter!(
        DELETE_BYTES_REMOVED,
        metrics::Unit::Bytes,
        "Cumulative bytes reclaimed by deletion sweeps"
    );
    metrics::describe_histogram!(
        DELETE_DURATION_MS,
        metrics::Unit::Milliseconds,
        "Per-sweep wall-clock duration"
    );
    metrics::describe_counter!(
        DELETE_ERRORS,
        "Object-store DELETE calls that failed during sweep"
    );
}

/// Return a label set for a hot-path metric. Always includes `topic`;
/// includes `partition` only when `metrics_high_cardinality` was true
/// at `init` time.
pub fn partition_label(topic: &str, partition: u32) -> Vec<(&'static str, String)> {
    let mut v = vec![(LABEL_TOPIC, topic.to_string())];
    if HIGH_CARDINALITY.load(Ordering::Relaxed) {
        v.push((LABEL_PARTITION, partition.to_string()));
    }
    v
}

/// Same as [`partition_label`], but with additional labels appended (e.g.
/// `error_code`, `source`, `trigger`). Used at call sites where the metric
/// already carries another label alongside `topic`/`partition`.
pub fn partition_label_with(
    topic: &str,
    partition: u32,
    extras: &[(&'static str, String)],
) -> Vec<(&'static str, String)> {
    let mut v = partition_label(topic, partition);
    v.extend_from_slice(extras);
    v
}

/// Full list of every metric name constant. Used by tests to verify
/// uniqueness and to spot-check that describes stay in sync with call
/// sites when new metrics are added.
#[cfg(test)]
pub(crate) const ALL_METRIC_NAMES: &[&str] = &[
    PRODUCE_RECORDS,
    PRODUCE_BYTES,
    PRODUCE_LATENCY_MS,
    PRODUCE_ERRORS,
    FETCH_REQUESTS,
    FETCH_RECORDS,
    FETCH_BYTES,
    FETCH_LATENCY_MS,
    FETCH_LONG_POLL_WAIT_MS,
    FETCH_SOURCE,
    FETCH_ERRORS,
    UPLOADER_SEGMENTS_UPLOADED,
    UPLOADER_BYTES_UPLOADED,
    UPLOADER_UPLOAD_LATENCY_MS,
    UPLOADER_UPLOAD_RETRIES,
    RETENTION_SEGMENTS_EVICTED,
    RETENTION_BYTES_EVICTED,
    RETENTION_PASSES,
    RETENTION_DELETE_FAILURES,
    RETENTION_SWEEP_KICKS_SENT,
    RETENTION_SWEEP_KICKS_DROPPED,
    PARTITION_COUNT,
    PARTITION_RECORDS_IN_MEMORY,
    PARTITION_BYTES_IN_MEMORY,
    PARTITION_SEGMENTS_UPLOADED,
    PARTITION_HWM_OFFSET,
    PARTITION_WAL_FILES,
    RUNTIME_UPTIME_SECONDS,
    RUNTIME_BUILD_INFO,
    WIRE_CONNECTIONS_ACTIVE,
    WIRE_CONNECTIONS_ACCEPTED,
    WIRE_RPC_REQUESTS,
    DELETE_PENDING_TOPICS,
    DELETE_SEGMENTS_REMOVED,
    DELETE_BYTES_REMOVED,
    DELETE_DURATION_MS,
    DELETE_ERRORS,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that toggle the process-global `HIGH_CARDINALITY`
    /// flag. Without this, `cargo test`'s default parallel execution races
    /// two tests' `store(true)` / `store(false)` calls against each other's
    /// assertions.
    static HIGH_CARDINALITY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn latency_buckets_are_monotonically_increasing() {
        for w in LATENCY_BUCKETS_MS.windows(2) {
            assert!(w[0] < w[1], "buckets not monotonic: {w:?}");
        }
    }

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
    fn init_with_no_metrics_port_is_noop() {
        let ports = PortsConfig {
            wire: vec![5432],
            metrics: None,
        };
        assert!(init(&ports, false).is_ok());
    }

    #[test]
    fn partition_label_respects_flag() {
        let _guard = HIGH_CARDINALITY_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        HIGH_CARDINALITY.store(false, Ordering::Relaxed);
        let labels = partition_label("orders", 3);
        assert_eq!(labels, vec![(LABEL_TOPIC, "orders".to_string())]);

        HIGH_CARDINALITY.store(true, Ordering::Relaxed);
        let labels = partition_label("orders", 3);
        assert_eq!(
            labels,
            vec![
                (LABEL_TOPIC, "orders".to_string()),
                (LABEL_PARTITION, "3".to_string()),
            ]
        );
        HIGH_CARDINALITY.store(false, Ordering::Relaxed);
    }

    #[test]
    fn partition_label_with_appends_extras_and_respects_flag() {
        let _guard = HIGH_CARDINALITY_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        HIGH_CARDINALITY.store(false, Ordering::Relaxed);
        let labels = partition_label_with("orders", 3, &[(LABEL_ERROR_CODE, "7".to_string())]);
        assert_eq!(
            labels,
            vec![
                (LABEL_TOPIC, "orders".to_string()),
                (LABEL_ERROR_CODE, "7".to_string()),
            ]
        );

        HIGH_CARDINALITY.store(true, Ordering::Relaxed);
        let labels = partition_label_with("orders", 3, &[(LABEL_ERROR_CODE, "7".to_string())]);
        assert_eq!(
            labels,
            vec![
                (LABEL_TOPIC, "orders".to_string()),
                (LABEL_PARTITION, "3".to_string()),
                (LABEL_ERROR_CODE, "7".to_string()),
            ]
        );
        HIGH_CARDINALITY.store(false, Ordering::Relaxed);
    }

    #[test]
    fn all_metric_names_are_unique() {
        assert_eq!(ALL_METRIC_NAMES.len(), 37, "expected 37 metric names");
        let mut sorted: Vec<&str> = ALL_METRIC_NAMES.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 37, "metric names must be unique");
    }

    #[test]
    fn all_metric_names_have_dot_form() {
        // Names use dots in code; Prometheus exposition converts to underscores.
        // A metric name without a dot is either miscategorised or an internal bug.
        for name in ALL_METRIC_NAMES {
            assert!(
                name.contains('.'),
                "metric constant {name} does not use dotted namespace"
            );
        }
    }
}
