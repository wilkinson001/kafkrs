//! Retention policy evaluation. Pure function: given a manifest, resolved
//! per-topic config, and wall-clock now, return the segments to evict.

use kafkrs_models::manifest::{Manifest, SegmentEntry};
use kafkrs_models::topic::ResolvedTopicConfig;

/// Compute the set of segments to evict. Pure. Never returns the last
/// (tail) segment even if it would be eligible.
pub fn evaluate_eviction(
    manifest: &Manifest,
    cfg: &ResolvedTopicConfig,
    now_ns: i64,
) -> Vec<SegmentEntry> {
    if manifest.segments.len() <= 1 {
        return Vec::new();
    }

    // Time-based eligibility: last_timestamp_ns older than the threshold.
    let time_cutoff_ns: Option<i64> = if cfg.retention_ms < 0 {
        None
    } else {
        Some(now_ns - cfg.retention_ms.saturating_mul(1_000_000))
    };

    // Size-based eligibility: evict oldest first until total size <= cap.
    let total_bytes: u64 = manifest.segments.iter().map(|s| s.byte_size).sum();
    let mut over_size_by: i64 = if cfg.retention_bytes < 0 {
        0
    } else {
        (total_bytes as i64) - cfg.retention_bytes
    };

    let mut evict = Vec::new();
    // Segments are stored in base_offset order (Uploader sorts on insert).
    // Iterate all but the last — never evict the tail segment.
    for seg in &manifest.segments[..manifest.segments.len() - 1] {
        let time_says_evict = time_cutoff_ns.is_some_and(|cutoff| seg.last_timestamp_ns < cutoff);
        let size_says_evict = over_size_by > 0;
        if time_says_evict || size_says_evict {
            over_size_by -= seg.byte_size as i64;
            evict.push(seg.clone());
        } else {
            break;
        }
    }
    evict
}

#[cfg(test)]
mod tests {
    use super::*;
    use kafkrs_models::config::DiskType;
    use kafkrs_models::topic::TopicConfigOverrides;

    fn seg(base: i64, last: i64, last_ts_ns: i64, byte_size: u64) -> SegmentEntry {
        SegmentEntry {
            base_offset: base,
            last_offset: last,
            base_timestamp_ns: last_ts_ns - 1_000_000, // 1ms span
            last_timestamp_ns: last_ts_ns,
            record_count: (last - base + 1) as u64,
            byte_size,
            object_key: format!("segment-{:020}.parquet", base),
        }
    }

    fn cfg(retention_ms: i64, retention_bytes: i64) -> ResolvedTopicConfig {
        let o = TopicConfigOverrides {
            retention_ms: Some(retention_ms),
            retention_bytes: Some(retention_bytes),
            ..Default::default()
        };
        ResolvedTopicConfig::resolve(&o, DiskType::Nvme)
    }

    #[test]
    fn no_eviction_when_all_infinite() {
        let mut m = Manifest::empty("t", 0);
        m.segments = vec![
            seg(0, 99, 1_000_000_000, 100),
            seg(100, 199, 2_000_000_000, 100),
        ];
        let out = evaluate_eviction(&m, &cfg(-1, -1), 10_000_000_000);
        assert!(out.is_empty());
    }

    #[test]
    fn no_eviction_when_manifest_has_one_segment() {
        let mut m = Manifest::empty("t", 0);
        m.segments = vec![seg(0, 99, 1_000_000_000, 100)];
        // Everything says evict, but the tail is never eligible.
        let out = evaluate_eviction(&m, &cfg(1, 1), 10_000_000_000);
        assert!(out.is_empty());
    }

    #[test]
    fn time_based_evicts_expired_segments() {
        let now_ns = 100_000_000_000; // 100s in ns
        let mut m = Manifest::empty("t", 0);
        m.segments = vec![
            seg(0, 99, 8_000_000_000, 100),
            seg(100, 199, 96_000_000_000, 100),
            seg(200, 299, 99_000_000_000, 100),
        ];
        // retention_ms = 10_000 → cutoff at now - 10s = 90s.
        // Segment 0 (last=8s)  → older than cutoff → evict.
        // Segment 1 (last=96s) → newer than cutoff → keep.
        // Segment 2 (last=99s) → tail, never evicted regardless.
        let out = evaluate_eviction(&m, &cfg(10_000, -1), now_ns);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].base_offset, 0);
    }

    #[test]
    fn size_based_evicts_oldest_first() {
        let mut m = Manifest::empty("t", 0);
        m.segments = vec![
            seg(0, 99, 1_000_000_000, 100),
            seg(100, 199, 2_000_000_000, 100),
            seg(200, 299, 3_000_000_000, 100),
        ];
        // total = 300 bytes; cap = 250 → over by 50 → evict oldest 100-byte segment.
        let out = evaluate_eviction(&m, &cfg(-1, 250), 10_000_000_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].base_offset, 0);
    }

    #[test]
    fn either_dimension_triggers_eviction() {
        let now_ns = 100_000_000_000;
        let mut m = Manifest::empty("t", 0);
        // Two 100-byte segments; oldest is age-expired, newest is not,
        // and total size is under cap.
        m.segments = vec![
            seg(0, 99, 1_000_000_000, 100),     // 1s → expired
            seg(100, 199, 99_000_000_000, 100), // 99s → tail
        ];
        // Time cutoff at now - 10s = 90s; segment 0 evicted, segment 1 is tail.
        let out = evaluate_eviction(&m, &cfg(10_000, 1_000_000), now_ns);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].base_offset, 0);
    }

    #[test]
    fn tail_segment_never_evicted() {
        let now_ns = 100_000_000_000;
        let mut m = Manifest::empty("t", 0);
        m.segments = vec![
            seg(0, 99, 1_000_000_000, 100_000),
            seg(100, 199, 2_000_000_000, 100_000), // also old + big → tail, kept.
        ];
        // Both dimensions say evict everything.
        let out = evaluate_eviction(&m, &cfg(10, 1), now_ns);
        // Only segment 0 should evict; segment 1 is the tail.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].base_offset, 0);
    }
}
