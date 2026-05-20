use anyhow::Result;
use bytes::Bytes;
use kafkrs_models::config::ObjectStoreConfig;
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;
use std::ops::Range;
use std::sync::Arc;

/// Constructs the configured object store. `filesystem` is rooted at
/// `<data_dir>/object_store` for local testing; `s3` targets the configured
/// bucket/endpoint with credentials sourced from the environment / IAM.
pub fn build_store(cfg: &ObjectStoreConfig, data_dir: &str) -> Result<Arc<dyn ObjectStore>> {
    match cfg.backend.as_str() {
        "filesystem" => {
            let root: std::path::PathBuf = std::path::Path::new(data_dir).join("object_store");
            std::fs::create_dir_all(&root)?;
            Ok(Arc::new(
                object_store::local::LocalFileSystem::new_with_prefix(root)?,
            ))
        }
        "s3" => {
            let mut b: object_store::aws::AmazonS3Builder =
                object_store::aws::AmazonS3Builder::from_env()
                    .with_bucket_name(&cfg.bucket)
                    .with_region(&cfg.region);
            if !cfg.endpoint.is_empty() {
                b = b.with_endpoint(&cfg.endpoint).with_allow_http(true);
            }
            Ok(Arc::new(b.build()?))
        }
        other => anyhow::bail!("unknown object_store backend: {other}"),
    }
}

/// Deterministic object key for a sealed segment (spec §"Object key layout").
/// `prefix` is the configured `object_store.prefix` (may be empty).
pub fn segment_key(prefix: &str, topic: &str, partition: u32, base_offset: i64) -> ObjPath {
    join(
        prefix,
        topic,
        partition,
        &format!("segment-{:020}.parquet", base_offset),
    )
}

pub fn manifest_key(prefix: &str, topic: &str, partition: u32) -> ObjPath {
    join(prefix, topic, partition, "manifest.json")
}

fn join(prefix: &str, topic: &str, partition: u32, leaf: &str) -> ObjPath {
    let mut s: String = String::new();
    if !prefix.is_empty() {
        s.push_str(prefix.trim_end_matches('/'));
        s.push('/');
    }
    s.push_str(&format!("{topic}/partition={partition}/{leaf}"));
    ObjPath::from(s)
}

pub async fn put(store: &Arc<dyn ObjectStore>, key: &ObjPath, bytes: Bytes) -> Result<()> {
    store.put(key, bytes.into()).await?;
    Ok(())
}

pub async fn get(store: &Arc<dyn ObjectStore>, key: &ObjPath) -> Result<Bytes> {
    Ok(store.get(key).await?.bytes().await?)
}

pub async fn get_range(
    store: &Arc<dyn ObjectStore>,
    key: &ObjPath,
    range: Range<usize>,
) -> Result<Bytes> {
    Ok(store.get_range(key, range).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_hive_partitioned_and_zero_padded() {
        let k = segment_key("", "orders", 3, 100);
        assert_eq!(
            k.to_string(),
            "orders/partition=3/segment-00000000000000000100.parquet"
        );
        let k2 = segment_key("env/v1", "orders", 0, 0);
        assert_eq!(
            k2.to_string(),
            "env/v1/orders/partition=0/segment-00000000000000000000.parquet"
        );
        assert_eq!(
            manifest_key("", "orders", 3).to_string(),
            "orders/partition=3/manifest.json"
        );
    }

    #[tokio::test]
    async fn filesystem_put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ObjectStoreConfig {
            backend: "filesystem".into(),
            bucket: "b".into(),
            prefix: "".into(),
            endpoint: "".into(),
            region: "us-east-1".into(),
        };
        let store = build_store(&cfg, dir.path().to_str().unwrap()).unwrap();
        let key = manifest_key("", "t", 0);
        put(&store, &key, Bytes::from_static(b"hello"))
            .await
            .unwrap();
        let got = get(&store, &key).await.unwrap();
        assert_eq!(&got[..], b"hello");
        let r = get_range(&store, &key, 1..3).await.unwrap();
        assert_eq!(&r[..], b"el");
    }
}
