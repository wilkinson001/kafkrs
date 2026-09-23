use kafkrs_models::config::BrokerConfig;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct BrokerIdentity {
    pub broker_id: Arc<str>,
    pub cluster_id: Arc<str>,
    pub advertised_host: Arc<str>,
    pub advertised_port: u16,
}

#[derive(Debug)]
pub enum IdentityError {
    MissingClusterId,
    IoError(String),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::MissingClusterId => write!(
                f,
                "broker.cluster_id must be set in config.toml (it's the human-readable \
                 cluster identifier clients use to detect misconfiguration)"
            ),
            IdentityError::IoError(msg) => write!(f, "identity IO error: {msg}"),
        }
    }
}

impl std::error::Error for IdentityError {}

pub fn resolve_identity(
    cfg: &BrokerConfig,
    address: &str,
    wire_port: u16,
    data_dir: &Path,
) -> Result<BrokerIdentity, IdentityError> {
    let cluster_id = cfg
        .cluster_id
        .clone()
        .ok_or(IdentityError::MissingClusterId)?;
    let broker_id = resolve_broker_id(&cfg.id, data_dir)?;
    Ok(BrokerIdentity {
        broker_id: Arc::from(broker_id),
        cluster_id: Arc::from(cluster_id),
        advertised_host: Arc::from(address.to_string()),
        advertised_port: wire_port,
    })
}

fn resolve_broker_id(cfg_id: &Option<String>, data_dir: &Path) -> Result<String, IdentityError> {
    if let Some(id) = cfg_id.as_ref() {
        return Ok(id.clone());
    }
    let path = data_dir.join("broker_id");
    if path.exists() {
        return std::fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .map_err(|e| IdentityError::IoError(format!("read {}: {e}", path.display())));
    }
    let id = generate_broker_id();
    // Ensure data_dir exists — fresh install / container start where the operator
    // hasn't pre-created the directory needs auto-creation. std::fs::write does
    // NOT auto-create parents.
    std::fs::create_dir_all(data_dir).map_err(|e| {
        IdentityError::IoError(format!("create data_dir {}: {e}", data_dir.display()))
    })?;
    std::fs::write(&path, &id)
        .map_err(|e| IdentityError::IoError(format!("write {}: {e}", path.display())))?;
    Ok(id)
}

/// Generate a fresh broker identifier of the form `brk-<8 lowercase hex chars>`.
/// Uses UUIDv7's tail (fully random per RFC 9562) to avoid pulling in a
/// dedicated random crate; the 32 bits of entropy are more than enough for
/// realistic single-cluster broker counts.
pub fn generate_broker_id() -> String {
    let uuid = uuid::Uuid::now_v7();
    let bytes = uuid.as_bytes();
    format!(
        "brk-{:02x}{:02x}{:02x}{:02x}",
        bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

#[cfg(test)]
mod tests {
    use super::{generate_broker_id, resolve_identity, IdentityError};
    use kafkrs_models::config::BrokerConfig;
    use tempfile::tempdir;

    fn cfg_with(cluster_id: Option<&str>, id: Option<&str>) -> BrokerConfig {
        BrokerConfig {
            cluster_id: cluster_id.map(String::from),
            id: id.map(String::from),
            ..Default::default()
        }
    }

    fn is_valid_broker_id(id: &str) -> bool {
        id.starts_with("brk-")
            && id.len() == 12
            && id[4..]
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }

    #[test]
    fn resolve_uses_config_id_when_set() {
        let dir = tempdir().unwrap();
        let cfg = cfg_with(Some("prod-east"), Some("broker-1"));
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        assert_eq!(&*ident.broker_id, "broker-1");
        assert_eq!(&*ident.cluster_id, "prod-east");
        assert_eq!(&*ident.advertised_host, "127.0.0.1");
        assert_eq!(ident.advertised_port, 5432);
        // No disk file created because config wins.
        assert!(!dir.path().join("broker_id").exists());
    }

    #[test]
    fn resolve_reads_persisted_file_when_config_id_unset() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("broker_id"), "brk-deadbeef").unwrap();
        let cfg = cfg_with(Some("prod-east"), None);
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        assert_eq!(&*ident.broker_id, "brk-deadbeef");
    }

    #[test]
    fn resolve_reads_persisted_file_trimming_trailing_newline() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("broker_id"), "brk-cafefeed\n").unwrap();
        let cfg = cfg_with(Some("prod-east"), None);
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        assert_eq!(&*ident.broker_id, "brk-cafefeed");
    }

    #[test]
    fn resolve_generates_and_persists_on_first_boot() {
        let dir = tempdir().unwrap();
        let cfg = cfg_with(Some("prod-east"), None);
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        // Generated ID matches format.
        assert!(
            is_valid_broker_id(&ident.broker_id),
            "broker_id {:?} does not match brk-<8hex>",
            ident.broker_id
        );
        // File persisted with the same value.
        let persisted = std::fs::read_to_string(dir.path().join("broker_id")).unwrap();
        assert_eq!(persisted.trim(), &*ident.broker_id);
    }

    #[test]
    fn resolve_config_id_wins_over_disk_file() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("broker_id"), "brk-fromdisk").unwrap();
        let file_before = std::fs::read(dir.path().join("broker_id")).unwrap();
        let cfg = cfg_with(Some("prod-east"), Some("broker-config"));
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()).unwrap();
        assert_eq!(&*ident.broker_id, "broker-config");
        // Disk file untouched.
        let file_after = std::fs::read(dir.path().join("broker_id")).unwrap();
        assert_eq!(file_before, file_after);
    }

    #[test]
    fn resolve_creates_data_dir_when_missing() {
        // Point at a subdir that doesn't exist yet — simulates fresh
        // install / container start where the operator hasn't pre-created
        // ./data. `std::fs::write` doesn't auto-create parents, so
        // `resolve_identity` must do so itself.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nonexistent_subdir");
        assert!(!missing.exists(), "test setup: subdir should not exist");
        let cfg = cfg_with(Some("prod-east"), None);
        let ident = resolve_identity(&cfg, "127.0.0.1", 5432, &missing).unwrap();
        assert!(is_valid_broker_id(&ident.broker_id));
        assert!(missing.exists(), "data_dir should have been created");
        assert!(
            missing.join("broker_id").exists(),
            "broker_id file should exist"
        );
    }

    #[test]
    fn resolve_fails_when_cluster_id_missing() {
        let dir = tempdir().unwrap();
        let cfg = cfg_with(None, Some("broker-1"));
        match resolve_identity(&cfg, "127.0.0.1", 5432, dir.path()) {
            Err(IdentityError::MissingClusterId) => {}
            other => panic!("expected MissingClusterId, got {other:?}"),
        }
    }

    #[test]
    fn generate_broker_id_matches_brk_prefix_format() {
        let id = generate_broker_id();
        assert!(
            is_valid_broker_id(&id),
            "{:?} does not match brk-<8hex>",
            id
        );
    }

    #[test]
    fn generate_broker_id_produces_different_values() {
        let a = generate_broker_id();
        let b = generate_broker_id();
        // With 32 bits of entropy the collision probability is ~2^-32. Passing
        // twice guards against a stub implementation.
        assert_ne!(a, b, "generate_broker_id produced duplicate value");
    }
}
