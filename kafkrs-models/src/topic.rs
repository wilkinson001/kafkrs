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
pub const DEFAULT_RETENTION_MS: i64 = 7 * 24 * 3600 * 1000; // 7 days
pub const DEFAULT_RETENTION_BYTES: i64 = -1; // no size cap

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
    pub retention_ms: Option<i64>,
    pub retention_bytes: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TopicEntry {
    pub name: String,
    pub uuid: String,
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
    pub retention_ms: i64,
    pub retention_bytes: i64,
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
            retention_ms: o.retention_ms.unwrap_or(DEFAULT_RETENTION_MS),
            retention_bytes: o.retention_bytes.unwrap_or(DEFAULT_RETENTION_BYTES),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValidationError {
    FieldOutOfRange {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
}

impl std::fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigValidationError::FieldOutOfRange {
                field,
                value,
                reason,
            } => write!(f, "field `{field}` value `{value}` out of range: {reason}"),
        }
    }
}

impl std::error::Error for ConfigValidationError {}

impl TopicConfigOverrides {
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        if let Some(v) = self.segment_size_bytes {
            if v < 1 {
                return Err(ConfigValidationError::FieldOutOfRange {
                    field: "segment_size_bytes",
                    value: v.to_string(),
                    reason: "must be >= 1 (0 would seal every record then re-seal instantly)",
                });
            }
        }
        if let Some(v) = self.segment_seal_time_ms {
            if v < 1 {
                return Err(ConfigValidationError::FieldOutOfRange {
                    field: "segment_seal_time_ms",
                    value: v.to_string(),
                    reason: "must be >= 1 (0 defeats seal-by-time)",
                });
            }
        }
        if let Some(v) = self.retention_ms {
            if v < -1 {
                return Err(ConfigValidationError::FieldOutOfRange {
                    field: "retention_ms",
                    value: v.to_string(),
                    reason: "must be >= -1 (-1 = never; other negatives are meaningless)",
                });
            }
        }
        if let Some(v) = self.retention_bytes {
            if v < -1 {
                return Err(ConfigValidationError::FieldOutOfRange {
                    field: "retention_bytes",
                    value: v.to_string(),
                    reason: "must be >= -1 (-1 = no cap; other negatives are meaningless)",
                });
            }
        }
        Ok(())
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
        assert_eq!(r.retention_ms, 7 * 24 * 3600 * 1000);
        assert_eq!(r.retention_bytes, -1);
    }

    #[test]
    fn retention_overrides_win() {
        let o = TopicConfigOverrides {
            retention_ms: Some(-1), // opt out
            retention_bytes: Some(1_000_000_000),
            ..Default::default()
        };
        let r = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
        assert_eq!(r.retention_ms, -1);
        assert_eq!(r.retention_bytes, 1_000_000_000);
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
            uuid: "01936a80-1234-7890-abcd-ef1234567890".into(),
            partition_count: 3,
            created_at_ns: 1,
            config: TopicConfigOverrides::default(),
        });
        let j = serde_json::to_string(&f).unwrap();
        let back: TopicRegistryFile = serde_json::from_str(&j).unwrap();
        assert_eq!(back.topics[0].name, "orders");
        assert_eq!(back.topics[0].partition_count, 3);
    }

    #[test]
    fn topic_entry_roundtrips_with_uuid() {
        let e = TopicEntry {
            name: "orders".into(),
            uuid: "01936a80-1234-7890-abcd-ef1234567890".into(),
            partition_count: 4,
            created_at_ns: 1_700_000_000_000_000_000,
            config: TopicConfigOverrides::default(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: TopicEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.uuid, e.uuid);
        assert_eq!(back.name, e.name);
        assert_eq!(back.partition_count, 4);
    }

    #[test]
    fn topic_entry_without_uuid_fails_to_deserialize() {
        // Simulates a 0.5.0 topics.json entry (no uuid field).
        let legacy = r#"{"name":"orders","partition_count":1,"created_at_ns":0}"#;
        let err = serde_json::from_str::<TopicEntry>(legacy).unwrap_err();
        assert!(
            err.to_string().contains("uuid"),
            "expected error mentioning `uuid` field, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_defaults() {
        assert!(TopicConfigOverrides::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_segment_size_zero() {
        let o = TopicConfigOverrides {
            segment_size_bytes: Some(0),
            ..Default::default()
        };
        match o.validate() {
            Err(ConfigValidationError::FieldOutOfRange { field, .. }) => {
                assert_eq!(field, "segment_size_bytes");
            }
            other => panic!("expected FieldOutOfRange for segment_size_bytes, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_segment_seal_time_zero() {
        let o = TopicConfigOverrides {
            segment_seal_time_ms: Some(0),
            ..Default::default()
        };
        match o.validate() {
            Err(ConfigValidationError::FieldOutOfRange { field, .. }) => {
                assert_eq!(field, "segment_seal_time_ms");
            }
            other => panic!("expected FieldOutOfRange for segment_seal_time_ms, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_retention_ms_below_minus_one() {
        let o = TopicConfigOverrides {
            retention_ms: Some(-2),
            ..Default::default()
        };
        match o.validate() {
            Err(ConfigValidationError::FieldOutOfRange { field, .. }) => {
                assert_eq!(field, "retention_ms");
            }
            other => panic!("expected FieldOutOfRange for retention_ms, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_retention_bytes_below_minus_one() {
        let o = TopicConfigOverrides {
            retention_bytes: Some(-2),
            ..Default::default()
        };
        match o.validate() {
            Err(ConfigValidationError::FieldOutOfRange { field, .. }) => {
                assert_eq!(field, "retention_bytes");
            }
            other => panic!("expected FieldOutOfRange for retention_bytes, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_valid_full_config() {
        let o = TopicConfigOverrides {
            segment_size_bytes: Some(1024),
            segment_seal_time_ms: Some(1000),
            max_key_size_bytes: Some(512),
            max_value_size_bytes: Some(65_536),
            group_commit_time_ms: Some(10),
            group_commit_size_bytes: Some(1024),
            group_commit_record_count: Some(64),
            max_fetch_wait_ms: Some(100),
            retention_ms: Some(60_000),
            retention_bytes: Some(1_000_000_000),
        };
        assert!(o.validate().is_ok());
    }

    #[test]
    fn validate_accepts_retention_ms_negative_one_sentinel() {
        let o = TopicConfigOverrides {
            retention_ms: Some(-1),
            ..Default::default()
        };
        assert!(o.validate().is_ok());
    }

    #[test]
    fn validate_accepts_retention_bytes_negative_one_sentinel() {
        let o = TopicConfigOverrides {
            retention_bytes: Some(-1),
            ..Default::default()
        };
        assert!(o.validate().is_ok());
    }
}
