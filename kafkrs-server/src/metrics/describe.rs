//! Registers HELP text and units for every metric the broker emits.

use super::names::*;

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
