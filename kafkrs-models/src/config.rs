use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub address: String,
    pub ports: Vec<u16>,
    pub data_dir: String,
    #[serde(default)]
    pub broker: BrokerConfig,
    pub object_store: ObjectStoreConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BrokerConfig {
    #[serde(default)]
    pub disk_type: DiskType,
    #[serde(default)]
    pub auto_create_topics: bool,
    #[serde(default = "default_partition_count")]
    pub default_partition_count: u32,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        BrokerConfig {
            disk_type: DiskType::default(),
            auto_create_topics: false,
            default_partition_count: default_partition_count(),
        }
    }
}

fn default_partition_count() -> u32 {
    1
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiskType {
    #[default]
    Nvme,
    Ssd,
    Rotational,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupCommitProfile {
    pub time_ms: u64,
    pub size_bytes: usize,
    pub record_count: usize,
}

impl DiskType {
    pub fn group_commit_profile(&self) -> GroupCommitProfile {
        match self {
            DiskType::Nvme => GroupCommitProfile {
                time_ms: 5,
                size_bytes: 64 * 1024,
                record_count: 256,
            },
            DiskType::Ssd => GroupCommitProfile {
                time_ms: 15,
                size_bytes: 256 * 1024,
                record_count: 1024,
            },
            DiskType::Rotational => GroupCommitProfile {
                time_ms: 50,
                size_bytes: 1024 * 1024,
                record_count: 4096,
            },
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct ObjectStoreConfig {
    pub backend: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_region")]
    pub region: String,
}

fn default_region() -> String {
    "us-east-1".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config_and_applies_disk_profile() {
        let toml = r#"
address = "127.0.0.1"
ports = [5432]
data_dir = "./data"

[broker]
disk_type = "nvme"
auto_create_topics = false
default_partition_count = 1

[object_store]
backend = "filesystem"
bucket = "kafkrs-data"
prefix = ""
endpoint = ""
region = "us-east-1"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.ports, vec![5432]);
        assert_eq!(cfg.data_dir, "./data");
        assert_eq!(cfg.broker.disk_type, DiskType::Nvme);
        assert!(!cfg.broker.auto_create_topics);
        assert_eq!(cfg.broker.default_partition_count, 1);
        assert_eq!(cfg.object_store.backend, "filesystem");

        let p = cfg.broker.disk_type.group_commit_profile();
        assert_eq!(p.time_ms, 5);
        assert_eq!(p.size_bytes, 64 * 1024);
        assert_eq!(p.record_count, 256);
    }

    #[test]
    fn rotational_profile_values() {
        let p = DiskType::Rotational.group_commit_profile();
        assert_eq!(
            (p.time_ms, p.size_bytes, p.record_count),
            (50, 1024 * 1024, 4096)
        );
    }

    #[test]
    fn defaults_apply_when_optional_sections_absent() {
        let toml = r#"
address = "127.0.0.1"
ports = [5432]
data_dir = "./data"
[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.broker.auto_create_topics);
        assert_eq!(cfg.broker.default_partition_count, 1);
        assert_eq!(cfg.broker.disk_type, DiskType::Nvme);
        assert_eq!(cfg.object_store.region, "us-east-1");
    }
}
