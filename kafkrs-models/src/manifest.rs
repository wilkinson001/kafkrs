use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SegmentEntry {
    pub base_offset: i64,
    pub last_offset: i64,
    pub base_timestamp_ns: i64,
    pub last_timestamp_ns: i64,
    pub record_count: u64,
    pub byte_size: u64,
    /// Relative key within the partition directory, e.g.
    /// `segment-00000000000000000000.parquet`.
    pub object_key: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Manifest {
    pub topic: String,
    pub partition: u32,
    pub format_version: u32,
    pub segments: Vec<SegmentEntry>,
}

impl Manifest {
    pub fn empty(topic: &str, partition: u32) -> Manifest {
        Manifest {
            topic: topic.to_string(),
            partition,
            format_version: 1,
            segments: Vec::new(),
        }
    }

    /// Binary-search the (offset-sorted, non-overlapping) segment list for the
    /// segment whose [base_offset, last_offset] range contains `offset`.
    pub fn segment_for_offset(&self, offset: i64) -> Option<&SegmentEntry> {
        let idx: usize = self.segments.partition_point(|s| s.last_offset < offset);
        self.segments
            .get(idx)
            .filter(|s| offset >= s.base_offset && offset <= s.last_offset)
    }

    pub fn last_uploaded_offset(&self) -> Option<i64> {
        self.segments.last().map(|s| s.last_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(base: i64, last: i64) -> SegmentEntry {
        SegmentEntry {
            base_offset: base,
            last_offset: last,
            base_timestamp_ns: base * 1000,
            last_timestamp_ns: last * 1000,
            record_count: (last - base + 1) as u64,
            byte_size: 123,
            object_key: format!("segment-{:020}.parquet", base),
        }
    }

    #[test]
    fn empty_manifest_serializes_with_empty_segments() {
        let m = Manifest::empty("orders", 3);
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"segments\":[]"));
        assert!(!j.contains("next_offset"));
        let back: Manifest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.segments.len(), 0);
        assert_eq!(back.format_version, 1);
    }

    #[test]
    fn segment_for_offset_binary_search() {
        let mut m = Manifest::empty("o", 0);
        m.segments = vec![seg(0, 99), seg(100, 199), seg(200, 299)];
        assert_eq!(m.segment_for_offset(0).unwrap().base_offset, 0);
        assert_eq!(m.segment_for_offset(150).unwrap().base_offset, 100);
        assert_eq!(m.segment_for_offset(299).unwrap().base_offset, 200);
        assert!(m.segment_for_offset(300).is_none());
        assert!(m.segment_for_offset(-1).is_none());
    }

    #[test]
    fn covers_offset_reports_highest_uploaded() {
        let mut m = Manifest::empty("o", 0);
        assert_eq!(m.last_uploaded_offset(), None);
        m.segments = vec![seg(0, 99), seg(100, 199)];
        assert_eq!(m.last_uploaded_offset(), Some(199));
    }
}
