use crate::object_store::{manifest_key, put};
use anyhow::Result;
use kafkrs_models::config::DiskType;
use kafkrs_models::manifest::Manifest;
use kafkrs_models::topic::{
    ResolvedTopicConfig, TopicConfigOverrides, TopicEntry, TopicRegistryFile,
};
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub enum RegistryMsg {
    Create {
        name: String,
        partition_count: u32,
        overrides: TopicConfigOverrides,
        reply: oneshot::Sender<Result<(), RegistryError>>,
    },
    Describe {
        name: String,
        reply: oneshot::Sender<Option<TopicEntry>>,
    },
    List {
        reply: oneshot::Sender<Vec<String>>,
    },
    /// Ensure a topic exists (auto-create). No-op if present.
    EnsureExists {
        name: String,
        partition_count: u32,
        reply: oneshot::Sender<Result<(), RegistryError>>,
    },
}

#[derive(Debug, PartialEq)]
pub enum RegistryError {
    AlreadyExists,
    Io(String),
}

pub struct TopicRegistry {
    data_dir: String,
    disk: DiskType,
    store: Arc<dyn ObjectStore>,
    prefix: String,
    topics: HashMap<String, TopicEntry>,
    rx: mpsc::Receiver<RegistryMsg>,
}

fn registry_path(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("topics.json")
}

impl TopicRegistry {
    /// Loads `topics.json` (or starts empty) and returns the actor.
    pub fn load(
        data_dir: String,
        disk: DiskType,
        store: Arc<dyn ObjectStore>,
        prefix: String,
        rx: mpsc::Receiver<RegistryMsg>,
    ) -> Result<TopicRegistry> {
        let path: PathBuf = registry_path(&data_dir);
        let file: TopicRegistryFile = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path)?)?
        } else {
            TopicRegistryFile::default()
        };
        let topics: HashMap<String, TopicEntry> = file
            .topics
            .into_iter()
            .map(|t| (t.name.clone(), t))
            .collect();
        Ok(TopicRegistry {
            data_dir,
            disk,
            store,
            prefix,
            topics,
            rx,
        })
    }

    pub fn resolved(&self, name: &str) -> Option<ResolvedTopicConfig> {
        self.topics
            .get(name)
            .map(|t| ResolvedTopicConfig::resolve(&t.config, self.disk.clone()))
    }

    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                RegistryMsg::Create {
                    name,
                    partition_count,
                    overrides,
                    reply,
                } => {
                    let _ = reply.send(self.create(&name, partition_count, overrides).await);
                }
                RegistryMsg::EnsureExists {
                    name,
                    partition_count,
                    reply,
                } => {
                    let r: Result<(), RegistryError> = if self.topics.contains_key(&name) {
                        Err(RegistryError::AlreadyExists)
                    } else {
                        self.create(&name, partition_count, TopicConfigOverrides::default())
                            .await
                    };
                    let _ = reply.send(r);
                }
                RegistryMsg::Describe { name, reply } => {
                    let _ = reply.send(self.topics.get(&name).cloned());
                }
                RegistryMsg::List { reply } => {
                    let _ = reply.send(self.topics.keys().cloned().collect());
                }
            }
        }
    }

    async fn create(
        &mut self,
        name: &str,
        partition_count: u32,
        overrides: TopicConfigOverrides,
    ) -> Result<(), RegistryError> {
        if self.topics.contains_key(name) {
            return Err(RegistryError::AlreadyExists);
        }
        let entry: TopicEntry = TopicEntry {
            name: name.to_string(),
            partition_count,
            created_at_ns: now_ns(),
            config: overrides,
        };
        // Step 1: atomic rewrite of topics.json (tmp + fsync + rename).
        let mut next: TopicRegistryFile = TopicRegistryFile {
            topics: self.topics.values().cloned().collect(),
        };
        next.topics.push(entry.clone());
        atomic_write_registry(&self.data_dir, &next)
            .map_err(|e| RegistryError::Io(e.to_string()))?;

        // Step 2: WAL directories per partition.
        for p in 0..partition_count {
            let dir: PathBuf = Path::new(&self.data_dir)
                .join("wal")
                .join(name)
                .join(p.to_string());
            std::fs::create_dir_all(&dir).map_err(|e| RegistryError::Io(e.to_string()))?;
        }
        // Step 3: empty manifest per partition.
        for p in 0..partition_count {
            let key: ObjPath = manifest_key(&self.prefix, name, p);
            let body: Vec<u8> = serde_json::to_vec(&Manifest::empty(name, p))
                .map_err(|e| RegistryError::Io(e.to_string()))?;
            put(&self.store, &key, bytes::Bytes::from(body))
                .await
                .map_err(|e| RegistryError::Io(e.to_string()))?;
        }
        self.topics.insert(name.to_string(), entry);
        Ok(())
    }
}

impl TopicRegistry {
    pub fn snapshot(&self) -> Vec<(String, u32, ResolvedTopicConfig)> {
        self.topics
            .values()
            .map(|t| {
                (
                    t.name.clone(),
                    t.partition_count,
                    ResolvedTopicConfig::resolve(&t.config, self.disk.clone()),
                )
            })
            .collect()
    }
}

fn atomic_write_registry(data_dir: &str, file: &TopicRegistryFile) -> std::io::Result<()> {
    let path: PathBuf = registry_path(data_dir);
    let tmp: PathBuf = path.with_extension("json.tmp");
    let body: Vec<u8> = serde_json::to_vec_pretty(file)?;
    {
        let mut f: std::fs::File = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn now_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::build_store;
    use kafkrs_models::config::ObjectStoreConfig;

    fn store(dir: &Path) -> Arc<dyn ObjectStore> {
        build_store(
            &ObjectStoreConfig {
                backend: "filesystem".into(),
                bucket: "b".into(),
                prefix: "".into(),
                endpoint: "".into(),
                region: "us-east-1".into(),
            },
            dir.to_str().unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn create_is_atomic_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(8);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r, rr) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "orders".into(),
            partition_count: 2,
            overrides: TopicConfigOverrides::default(),
            reply: r,
        })
        .await
        .unwrap();
        rr.await.unwrap().unwrap();

        // topics.json persisted
        assert!(registry_path(&dd).exists());
        // WAL dirs exist
        assert!(Path::new(&dd).join("wal/orders/0").exists());
        assert!(Path::new(&dd).join("wal/orders/1").exists());
        // empty manifests exist
        let raw = crate::object_store::get(&store(dir.path()), &manifest_key("", "orders", 1))
            .await
            .unwrap();
        let m: Manifest = serde_json::from_slice(&raw).unwrap();
        assert_eq!(m.segments.len(), 0);

        // duplicate create rejected
        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "orders".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides::default(),
            reply: r2,
        })
        .await
        .unwrap();
        assert_eq!(
            rr2.await.unwrap().unwrap_err(),
            RegistryError::AlreadyExists
        );
    }

    #[tokio::test]
    async fn ensure_exists_returns_already_exists_for_existing_topic() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(8);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        // First EnsureExists creates the topic.
        let (r1, rr1) = oneshot::channel();
        tx.send(RegistryMsg::EnsureExists {
            name: "foo".into(),
            partition_count: 1,
            reply: r1,
        })
        .await
        .unwrap();
        assert!(rr1.await.unwrap().is_ok());

        // Second EnsureExists for the same topic must return Err(AlreadyExists),
        // matching Create's semantic. This is what handle_produce's auto-create
        // branch relies on to avoid re-spawning partition workers.
        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::EnsureExists {
            name: "foo".into(),
            partition_count: 1,
            reply: r2,
        })
        .await
        .unwrap();
        assert_eq!(
            rr2.await.unwrap().unwrap_err(),
            RegistryError::AlreadyExists,
        );
    }

    #[tokio::test]
    async fn reload_recovers_topics() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        {
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(
                TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
                    .unwrap()
                    .run(),
            );
            let (r, rr) = oneshot::channel();
            tx.send(RegistryMsg::Create {
                name: "t".into(),
                partition_count: 1,
                overrides: TopicConfigOverrides::default(),
                reply: r,
            })
            .await
            .unwrap();
            rr.await.unwrap().unwrap();
        }
        let (_tx, rx) = mpsc::channel(1);
        let reg2 =
            TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
                .unwrap();
        assert!(reg2.resolved("t").is_some());
    }
}
