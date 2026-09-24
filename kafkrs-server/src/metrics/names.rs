//! Metric name and label key constants (37 metrics total).
//!
//! Every metric name and every label key used across the broker is defined
//! as a `pub const` here. Call sites reference the constants, never string
//! literals — this keeps the metrics catalogue in one place and gives us
//! compile-time typo detection.

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

    #[test]
    fn latency_buckets_are_monotonically_increasing() {
        for w in LATENCY_BUCKETS_MS.windows(2) {
            assert!(w[0] < w[1], "buckets not monotonic: {w:?}");
        }
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
