use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub address: String,
    pub ports: PortsConfig,
    pub data_dir: String,
    #[serde(default)]
    pub broker: BrokerConfig,
    pub object_store: ObjectStoreConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PortsConfig {
    pub wire: Vec<u16>,
    /// Prometheus scrape endpoint (`GET /metrics`). Opt-in.
    #[serde(default)]
    pub metrics: Option<u16>,
    /// Liveness + readiness endpoints (`GET /health` + `GET /ready`). Opt-in.
    /// Independent from `metrics` so operators can run K8s probes without
    /// exposing Prometheus scrape traffic. If set to the same port value as
    /// `metrics`, one listener serves the merged route set.
    #[serde(default)]
    pub health: Option<u16>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BrokerConfig {
    #[serde(default)]
    pub disk_type: DiskType,
    #[serde(default)]
    pub auto_create_topics: bool,
    #[serde(default = "default_partition_count")]
    pub default_partition_count: u32,
    #[serde(default)]
    pub retention_sweep_interval_ms: Option<u64>,
    #[serde(default)]
    pub metrics_high_cardinality: bool,
    /// Cluster identifier. **Required** — broker refuses to start if unset.
    /// Load-bearing safety machinery: clients cache this to detect misconfiguration.
    #[serde(default)]
    pub cluster_id: Option<String>,
    /// Broker identifier. Optional. If unset, resolved from `data_dir/broker_id`
    /// on restart, or auto-generated (`brk-<8hex>`) and persisted on first boot.
    #[serde(default)]
    pub id: Option<String>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        BrokerConfig {
            disk_type: DiskType::default(),
            auto_create_topics: false,
            default_partition_count: default_partition_count(),
            retention_sweep_interval_ms: None,
            metrics_high_cardinality: false,
            cluster_id: None,
            id: None,
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
data_dir = "./data"

[ports]
wire = [5432]

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
        assert_eq!(cfg.ports.wire, vec![5432]);
        assert_eq!(cfg.ports.metrics, None);
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
data_dir = "./data"

[ports]
wire = [5432]

[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.broker.auto_create_topics);
        assert_eq!(cfg.broker.default_partition_count, 1);
        assert_eq!(cfg.broker.disk_type, DiskType::Nvme);
        assert_eq!(cfg.broker.retention_sweep_interval_ms, None);
        assert_eq!(cfg.object_store.region, "us-east-1");
    }

    #[test]
    fn retention_sweep_interval_ms_parses_when_set() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[broker]
retention_sweep_interval_ms = 30000
[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.broker.retention_sweep_interval_ms, Some(30_000));
    }

    #[test]
    fn ports_metrics_parses_when_set() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]
metrics = 9464

[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.ports.wire, vec![5432]);
        assert_eq!(cfg.ports.metrics, Some(9464));
    }

    #[test]
    fn ports_health_parses_when_set() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]
health = 9465

[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.ports.wire, vec![5432]);
        assert_eq!(cfg.ports.metrics, None);
        assert_eq!(cfg.ports.health, Some(9465));
    }

    #[test]
    fn ports_metrics_and_health_can_coexist() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]
metrics = 9464
health = 9465

[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.ports.metrics, Some(9464));
        assert_eq!(cfg.ports.health, Some(9465));
    }

    #[test]
    fn ports_health_defaults_none() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.ports.health, None);
    }

    #[test]
    fn metrics_high_cardinality_defaults_false() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.broker.metrics_high_cardinality);
    }

    #[test]
    fn metrics_high_cardinality_parses_when_set() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[broker]
metrics_high_cardinality = true

[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.broker.metrics_high_cardinality);
    }

    #[test]
    fn old_top_level_ports_fails_helpfully() {
        let toml = r#"
address = "127.0.0.1"
ports = [5432]
data_dir = "./data"

[object_store]
backend = "filesystem"
bucket = "b"
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err().to_string();
        assert!(
            err.contains("ports"),
            "error should mention `ports` field, got: {err}"
        );
    }

    #[test]
    fn broker_cluster_id_and_id_parse_when_present() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[broker]
cluster_id = "prod-east"
id = "broker-1"

[object_store]
backend = "filesystem"
bucket = "b"
prefix = ""
endpoint = ""
region = "us-east-1"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.broker.cluster_id.as_deref(), Some("prod-east"));
        assert_eq!(cfg.broker.id.as_deref(), Some("broker-1"));
    }

    #[test]
    fn broker_cluster_id_only_parses_id_defaults_none() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[broker]
cluster_id = "staging-us-west"

[object_store]
backend = "filesystem"
bucket = "b"
prefix = ""
endpoint = ""
region = "us-east-1"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.broker.cluster_id.as_deref(), Some("staging-us-west"));
        assert_eq!(cfg.broker.id, None);
    }

    #[test]
    fn broker_defaults_leave_cluster_id_and_id_none() {
        let toml = r#"
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]

[object_store]
backend = "filesystem"
bucket = "b"
prefix = ""
endpoint = ""
region = "us-east-1"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.broker.cluster_id, None);
        assert_eq!(cfg.broker.id, None);
    }
}
