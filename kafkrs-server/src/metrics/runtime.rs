//! Process start time, build-info emission, and the uptime background task.

use super::names::{LABEL_GIT_SHA, LABEL_VERSION, RUNTIME_BUILD_INFO, RUNTIME_UPTIME_SECONDS};
use std::sync::OnceLock;
use std::time::Instant;

/// Process start time, set once at `init`. Read by `uptime_updater`.
static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Record the process start time and emit the one-shot build-info gauge.
/// Called once from `mod.rs::init` after the recorder is installed.
pub(super) fn install() {
    START_TIME.set(Instant::now()).ok();
    emit_build_info();
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
/// on a live tokio runtime after `init` returns — see [`super::init`]'s doc
/// comment.
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
