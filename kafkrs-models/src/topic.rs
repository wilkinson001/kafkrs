use crate::config::{DiskType, GroupCommitProfile};
use serde::{Deserialize, Serialize};

/// Broker-level defaults for per-topic overridable settings (spec §"Per-topic
/// overridable broker defaults"). Per-topic config overrides take precedence;
/// otherwise these apply.
pub const DEFAULT_SEGMENT_SIZE_BYTES: u64 = 128 * 1024 * 1024; // 128 MiB
pub const DEFAULT_SEGMENT_SEAL_TIME_MS: u64 = 60_000; // 60 s
pub const DEFAULT_MAX_KEY_SIZE_BYTES: u32 = 1024; // 1 KiB
pub const DEFAULT_MAX_VALUE_SIZE_BYTES: u32 = 1024 * 1024; // 1 MiB
pub const DEFAULT_MAX_FETCH_WAIT_MS: u64 = 60_000; // 60 s

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct TopicConfigOverrides {
    pub segment_size_bytes: Option<u64>,
    pub segment_seal_time_ms: Option<u64>,
    pub max_key_size_bytes: Option<u32>,
    pub max_value_size_bytes: Option<u32>,
    pub group_commit_time_ms: Option<u64>,
    pub group_commit_size_bytes: Option<usize>,
    pub group_commit_record_count: Option<usize>,
    pub max_fetch_wait_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TopicEntry {
    pub name: String,
    pub partition_count: u32,
    pub created_at_ns: i64,
    #[serde(default)]
    pub config: TopicConfigOverrides,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TopicRegistryFile {
    #[serde(default)]
    pub topics: Vec<TopicEntry>,
}

/// Effective config for a partition writer/uploader after merging per-topic
/// overrides over broker-level defaults (spec §"Per-topic overridable broker
/// defaults").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedTopicConfig {
    pub segment_size_bytes: u64,
    pub segment_seal_time_ms: u64,
    pub max_key_size_bytes: u32,
    pub max_value_size_bytes: u32,
    pub group_commit_time_ms: u64,
    pub group_commit_size_bytes: usize,
    pub group_commit_record_count: usize,
    pub max_fetch_wait_ms: u64,
}

impl ResolvedTopicConfig {
    pub fn resolve(o: &TopicConfigOverrides, disk: DiskType) -> ResolvedTopicConfig {
        let p: GroupCommitProfile = disk.group_commit_profile();
        ResolvedTopicConfig {
            segment_size_bytes: o.segment_size_bytes.unwrap_or(DEFAULT_SEGMENT_SIZE_BYTES),
            segment_seal_time_ms: o
                .segment_seal_time_ms
                .unwrap_or(DEFAULT_SEGMENT_SEAL_TIME_MS),
            max_key_size_bytes: o.max_key_size_bytes.unwrap_or(DEFAULT_MAX_KEY_SIZE_BYTES),
            max_value_size_bytes: o
                .max_value_size_bytes
                .unwrap_or(DEFAULT_MAX_VALUE_SIZE_BYTES),
            group_commit_time_ms: o.group_commit_time_ms.unwrap_or(p.time_ms),
            group_commit_size_bytes: o.group_commit_size_bytes.unwrap_or(p.size_bytes),
            group_commit_record_count: o.group_commit_record_count.unwrap_or(p.record_count),
            max_fetch_wait_ms: o.max_fetch_wait_ms.unwrap_or(DEFAULT_MAX_FETCH_WAIT_MS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DiskType;

    #[test]
    fn resolved_defaults_when_no_overrides() {
        let r = ResolvedTopicConfig::resolve(&TopicConfigOverrides::default(), DiskType::Nvme);
        assert_eq!(r.segment_size_bytes, 128 * 1024 * 1024);
        assert_eq!(r.segment_seal_time_ms, 60_000);
        assert_eq!(r.max_key_size_bytes, 1024);
        assert_eq!(r.max_value_size_bytes, 1024 * 1024);
        assert_eq!(r.group_commit_time_ms, 5); // nvme profile
        assert_eq!(r.group_commit_record_count, 256);
        assert_eq!(r.max_fetch_wait_ms, 60_000);
    }

    #[test]
    fn max_fetch_wait_ms_override_wins() {
        let o = TopicConfigOverrides {
            max_fetch_wait_ms: Some(200),
            ..Default::default()
        };
        let r = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
        assert_eq!(r.max_fetch_wait_ms, 200);
    }

    #[test]
    fn per_topic_override_wins() {
        let o = TopicConfigOverrides {
            segment_seal_time_ms: Some(5_000),
            ..Default::default()
        };
        let r = ResolvedTopicConfig::resolve(&o, DiskType::Ssd);
        assert_eq!(r.segment_seal_time_ms, 5_000);
        assert_eq!(r.group_commit_time_ms, 15); // ssd profile, not overridden
    }

    #[test]
    fn registry_file_roundtrips() {
        let mut f = TopicRegistryFile::default();
        f.topics.push(TopicEntry {
            name: "orders".into(),
            partition_count: 3,
            created_at_ns: 1,
            config: TopicConfigOverrides::default(),
        });
        let j = serde_json::to_string(&f).unwrap();
        let back: TopicRegistryFile = serde_json::from_str(&j).unwrap();
        assert_eq!(back.topics[0].name, "orders");
        assert_eq!(back.topics[0].partition_count, 3);
    }
}
