//! Label-set helpers for hot-path metrics.

use super::names::{LABEL_PARTITION, LABEL_TOPIC};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once at `init`; read from `partition_label`.
static HIGH_CARDINALITY: AtomicBool = AtomicBool::new(false);

/// Flip the high-cardinality flag. Called once from `mod.rs::init`.
pub(super) fn set_high_cardinality(v: bool) {
    HIGH_CARDINALITY.store(v, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::super::names::LABEL_ERROR_CODE;
    use super::*;

    /// Serializes tests that toggle the process-global `HIGH_CARDINALITY`
    /// flag. Without this, `cargo test`'s default parallel execution races
    /// two tests' `store(true)` / `store(false)` calls against each other's
    /// assertions.
    static HIGH_CARDINALITY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
}
