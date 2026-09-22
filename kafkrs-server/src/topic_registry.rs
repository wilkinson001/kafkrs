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
use uuid::Uuid;

pub enum RegistryMsg {
    Create {
        name: String,
        partition_count: u32,
        overrides: TopicConfigOverrides,
        reply: oneshot::Sender<Result<String, RegistryError>>,
    },
    Describe {
        name: String,
        reply: oneshot::Sender<Option<TopicEntry>>,
    },
    List {
        reply: oneshot::Sender<Vec<String>>,
    },
    /// Ensure a topic exists (auto-create semantics). Returns `Ok(uuid)` with
    /// the assigned UUID if this call created it; `Err(AlreadyExists)` if it
    /// already existed.
    EnsureExists {
        name: String,
        partition_count: u32,
        reply: oneshot::Sender<Result<String, RegistryError>>,
    },
    /// Remove a topic's registry entry and persist `topics.json`. This is
    /// only the registry's slice of DeleteTopic: actor shutdown, WAL
    /// removal, manifest snapshot, and sweep spawn are orchestrated by
    /// `wire/dispatch.rs::handle_delete_topic`, which has access to
    /// `SharedState`. The registry stays focused on its file-of-truth role.
    /// `delete_data` is carried through for uniformity but unused here — the
    /// dispatch handler decides sweep vs skip based on it.
    Delete {
        name: String,
        delete_data: bool,
        reply: oneshot::Sender<Result<(), RegistryError>>,
    },
    /// Merge a partial-patch of `TopicConfigOverrides` onto the topic's
    /// current config, validate the merged result, and persist to
    /// `topics.json` atomically. Returns the merged overrides so the
    /// caller can resolve them + push `UpdateConfig` messages to running
    /// actors. Persist-first: `topics.json` is fsync'd before the reply
    /// fires (spec invariant 1).
    Alter {
        name: String,
        patch: TopicConfigOverrides,
        reply: oneshot::Sender<Result<TopicConfigOverrides, RegistryError>>,
    },
}

#[derive(Debug, PartialEq)]
pub enum RegistryError {
    AlreadyExists,
    Io(String),
    UnknownTopic,
    InvalidConfig(String),
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
                    let r: Result<String, RegistryError> = if self.topics.contains_key(&name) {
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
                RegistryMsg::Delete {
                    name,
                    delete_data: _,
                    reply,
                } => {
                    let _ = reply.send(self.delete(&name).await);
                }
                RegistryMsg::Alter { name, patch, reply } => {
                    let _ = reply.send(self.alter(&name, patch).await);
                }
            }
        }
    }

    async fn delete(&mut self, name: &str) -> Result<(), RegistryError> {
        let entry = match self.topics.remove(name) {
            None => return Err(RegistryError::UnknownTopic),
            Some(e) => e,
        };
        let next: TopicRegistryFile = TopicRegistryFile {
            topics: self.topics.values().cloned().collect(),
        };
        match atomic_write_registry(&self.data_dir, &next) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Persistence failed — roll back the in-memory removal so the
                // registry stays consistent with topics.json on disk.
                self.topics.insert(name.to_string(), entry);
                Err(RegistryError::Io(e.to_string()))
            }
        }
    }

    async fn create(
        &mut self,
        name: &str,
        partition_count: u32,
        overrides: TopicConfigOverrides,
    ) -> Result<String, RegistryError> {
        if let Err(e) = overrides.validate() {
            return Err(RegistryError::InvalidConfig(e.to_string()));
        }
        if self.topics.contains_key(name) {
            return Err(RegistryError::AlreadyExists);
        }
        let uuid = Uuid::now_v7().hyphenated().to_string();
        let entry: TopicEntry = TopicEntry {
            name: name.to_string(),
            uuid,
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
            let key: ObjPath = manifest_key(&self.prefix, name, &entry.uuid, p);
            let body: Vec<u8> = serde_json::to_vec(&Manifest::empty(name, p))
                .map_err(|e| RegistryError::Io(e.to_string()))?;
            put(&self.store, &key, bytes::Bytes::from(body))
                .await
                .map_err(|e| RegistryError::Io(e.to_string()))?;
        }
        let uuid: String = entry.uuid.clone();
        self.topics.insert(name.to_string(), entry);
        Ok(uuid)
    }

    async fn alter(
        &mut self,
        name: &str,
        patch: TopicConfigOverrides,
    ) -> Result<TopicConfigOverrides, RegistryError> {
        let Some(entry) = self.topics.get(name).cloned() else {
            return Err(RegistryError::UnknownTopic);
        };

        // Merge patch onto current config.
        let mut merged = entry.config.clone();
        if let Some(v) = patch.segment_size_bytes {
            merged.segment_size_bytes = Some(v);
        }
        if let Some(v) = patch.segment_seal_time_ms {
            merged.segment_seal_time_ms = Some(v);
        }
        if let Some(v) = patch.max_key_size_bytes {
            merged.max_key_size_bytes = Some(v);
        }
        if let Some(v) = patch.max_value_size_bytes {
            merged.max_value_size_bytes = Some(v);
        }
        if let Some(v) = patch.group_commit_time_ms {
            merged.group_commit_time_ms = Some(v);
        }
        if let Some(v) = patch.group_commit_size_bytes {
            merged.group_commit_size_bytes = Some(v);
        }
        if let Some(v) = patch.group_commit_record_count {
            merged.group_commit_record_count = Some(v);
        }
        if let Some(v) = patch.max_fetch_wait_ms {
            merged.max_fetch_wait_ms = Some(v);
        }
        if let Some(v) = patch.retention_ms {
            merged.retention_ms = Some(v);
        }
        if let Some(v) = patch.retention_bytes {
            merged.retention_bytes = Some(v);
        }

        // Validate BEFORE touching in-memory state.
        if let Err(e) = merged.validate() {
            return Err(RegistryError::InvalidConfig(e.to_string()));
        }

        // Swap in-memory config, then persist. On IO failure, roll back so
        // topics.json and in-memory state stay consistent.
        let prev_config = entry.config.clone();
        let mut new_entry = entry;
        new_entry.config = merged.clone();
        self.topics.insert(name.to_string(), new_entry);

        let next = TopicRegistryFile {
            topics: self.topics.values().cloned().collect(),
        };
        if let Err(e) = atomic_write_registry(&self.data_dir, &next) {
            // Roll back the in-memory swap.
            if let Some(t) = self.topics.get_mut(name) {
                t.config = prev_config;
            }
            return Err(RegistryError::Io(e.to_string()));
        }

        Ok(merged)
    }
}

impl TopicRegistry {
    pub fn snapshot(&self) -> Vec<(String, String, u32, ResolvedTopicConfig)> {
        self.topics
            .values()
            .map(|t| {
                (
                    t.name.clone(),
                    t.uuid.clone(),
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
        let uuid = rr.await.unwrap().unwrap();
        assert_eq!(
            Uuid::parse_str(&uuid).unwrap().get_version(),
            Some(uuid::Version::SortRand)
        );

        // topics.json persisted
        assert!(registry_path(&dd).exists());
        // WAL dirs exist
        assert!(Path::new(&dd).join("wal/orders/0").exists());
        assert!(Path::new(&dd).join("wal/orders/1").exists());
        // empty manifests exist
        let raw =
            crate::object_store::get(&store(dir.path()), &manifest_key("", "orders", &uuid, 1))
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

    #[tokio::test]
    async fn create_topic_assigns_uniquely_and_persists_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let registry =
            TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
                .unwrap();
        tokio::spawn(registry.run());

        let (r1_tx, r1_rx) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "orders".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides::default(),
            reply: r1_tx,
        })
        .await
        .unwrap();
        r1_rx.await.unwrap().unwrap();

        // Read topics.json off disk and verify uuid is present + parseable.
        let raw = std::fs::read_to_string(Path::new(&dd).join("topics.json")).unwrap();
        let parsed: TopicRegistryFile = serde_json::from_str(&raw).unwrap();
        let entry = &parsed.topics[0];
        assert_eq!(entry.name, "orders");
        let parsed_uuid = Uuid::parse_str(&entry.uuid).expect("valid UUID");
        assert_eq!(parsed_uuid.get_version(), Some(uuid::Version::SortRand)); // v7
    }

    #[tokio::test]
    async fn create_rejects_invalid_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd, DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r, rr) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "bad".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides {
                segment_size_bytes: Some(0),
                ..Default::default()
            },
            reply: r,
        })
        .await
        .unwrap();
        match rr.await.unwrap() {
            Err(RegistryError::InvalidConfig(msg)) => {
                assert!(msg.contains("segment_size_bytes"), "got: {msg}");
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn alter_unknown_topic_returns_unknown_topic() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd, DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r, rr) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "nope".into(),
            patch: TopicConfigOverrides::default(),
            reply: r,
        })
        .await
        .unwrap();
        assert_eq!(rr.await.unwrap().unwrap_err(), RegistryError::UnknownTopic);
    }

    #[tokio::test]
    async fn alter_invalid_patch_returns_invalid_config_and_leaves_state_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        // Create with valid config.
        let (r1, rr1) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "t".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides::default(),
            reply: r1,
        })
        .await
        .unwrap();
        rr1.await.unwrap().unwrap();

        // Alter with an invalid patch.
        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "t".into(),
            patch: TopicConfigOverrides {
                segment_size_bytes: Some(0),
                ..Default::default()
            },
            reply: r2,
        })
        .await
        .unwrap();
        match rr2.await.unwrap() {
            Err(RegistryError::InvalidConfig(msg)) => {
                assert!(msg.contains("segment_size_bytes"), "got: {msg}");
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }

        // Re-read topics.json and confirm on-disk config is untouched.
        let raw = std::fs::read_to_string(std::path::Path::new(&dd).join("topics.json")).unwrap();
        let parsed: TopicRegistryFile = serde_json::from_str(&raw).unwrap();
        let entry = parsed.topics.iter().find(|t| t.name == "t").unwrap();
        assert_eq!(entry.config.segment_size_bytes, None);
    }

    #[tokio::test]
    async fn alter_valid_patch_persists_and_returns_merged_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r1, rr1) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "t".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides::default(),
            reply: r1,
        })
        .await
        .unwrap();
        rr1.await.unwrap().unwrap();

        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "t".into(),
            patch: TopicConfigOverrides {
                retention_ms: Some(60_000),
                ..Default::default()
            },
            reply: r2,
        })
        .await
        .unwrap();
        let merged = rr2.await.unwrap().unwrap();
        assert_eq!(merged.retention_ms, Some(60_000));
        assert_eq!(merged.segment_size_bytes, None);

        let raw = std::fs::read_to_string(std::path::Path::new(&dd).join("topics.json")).unwrap();
        let parsed: TopicRegistryFile = serde_json::from_str(&raw).unwrap();
        let entry = parsed.topics.iter().find(|t| t.name == "t").unwrap();
        assert_eq!(entry.config.retention_ms, Some(60_000));
    }

    #[tokio::test]
    async fn alter_second_patch_composes_on_first() {
        let dir = tempfile::tempdir().unwrap();
        let dd = dir.path().to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(4);
        let reg = TopicRegistry::load(dd.clone(), DiskType::Nvme, store(dir.path()), "".into(), rx)
            .unwrap();
        tokio::spawn(reg.run());

        let (r1, rr1) = oneshot::channel();
        tx.send(RegistryMsg::Create {
            name: "t".into(),
            partition_count: 1,
            overrides: TopicConfigOverrides::default(),
            reply: r1,
        })
        .await
        .unwrap();
        rr1.await.unwrap().unwrap();

        // First patch: only retention_ms.
        let (r2, rr2) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "t".into(),
            patch: TopicConfigOverrides {
                retention_ms: Some(1000),
                ..Default::default()
            },
            reply: r2,
        })
        .await
        .unwrap();
        rr2.await.unwrap().unwrap();

        // Second patch: only max_fetch_wait_ms.
        let (r3, rr3) = oneshot::channel();
        tx.send(RegistryMsg::Alter {
            name: "t".into(),
            patch: TopicConfigOverrides {
                max_fetch_wait_ms: Some(200),
                ..Default::default()
            },
            reply: r3,
        })
        .await
        .unwrap();
        let merged = rr3.await.unwrap().unwrap();

        // Both fields are present in the merged result.
        assert_eq!(merged.retention_ms, Some(1000));
        assert_eq!(merged.max_fetch_wait_ms, Some(200));
    }
}
