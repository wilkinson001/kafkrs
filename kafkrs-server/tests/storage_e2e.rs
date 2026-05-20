use bytes::Bytes;
use kafkrs_models::config::{DiskType, ObjectStoreConfig};
use kafkrs_models::manifest::Manifest;
use kafkrs_models::record::Record;
use kafkrs_models::topic::{ResolvedTopicConfig, TopicConfigOverrides};
use kafkrs_server::object_store::{build_store, manifest_key, put};
use kafkrs_server::partition_writer::{IncomingRecord, PartitionWriter, PwMsg};
use kafkrs_server::uploader::{Uploader, UploaderMsg};
use tokio::sync::{broadcast, mpsc, oneshot};

async fn setup(dd: &str, seal_bytes: u64) -> (mpsc::Sender<PwMsg>, broadcast::Sender<i64>) {
    let store = build_store(
        &ObjectStoreConfig {
            backend: "filesystem".into(),
            bucket: "b".into(),
            prefix: "".into(),
            endpoint: "".into(),
            region: "us-east-1".into(),
        },
        dd,
    )
    .unwrap();
    put(
        &store,
        &manifest_key("", "t", 0),
        Bytes::from(serde_json::to_vec(&Manifest::empty("t", 0)).unwrap()),
    )
    .await
    .unwrap();

    let (utx, urx) = mpsc::channel(64);
    let (dtx, mut drx) = mpsc::channel(64);
    tokio::spawn(Uploader::new(store, "".into(), "t".into(), 0, urx, dtx).run());

    let (pw_tx, pw_rx) = mpsc::channel(256);
    let (tail, _) = broadcast::channel(1024);
    let pw_tx_d = pw_tx.clone();
    tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let _ = pw_tx_d.send(PwMsg::SegmentDurable(d)).await;
        }
    });

    let mut o = TopicConfigOverrides::default();
    o.segment_size_bytes = Some(seal_bytes);
    o.group_commit_record_count = Some(1);
    let cfg = ResolvedTopicConfig::resolve(&o, DiskType::Nvme);
    let pw = PartitionWriter::new(
        dd.into(),
        "t".into(),
        0,
        cfg,
        0,
        vec![],
        pw_rx,
        utx,
        tail.clone(),
    )
    .unwrap();
    tokio::spawn(pw.run());
    (pw_tx, tail)
}

async fn produce(tx: &mpsc::Sender<PwMsg>, v: Vec<u8>) -> i64 {
    let (a, ar) = oneshot::channel();
    tx.send(PwMsg::Produce {
        records: vec![IncomingRecord {
            schema_id: 0,
            key: vec![],
            value: v,
            timestamp_ns: 0,
        }],
        ack: a,
    })
    .await
    .unwrap();
    ar.await.unwrap()
}

#[tokio::test]
async fn produce_seals_uploads_and_is_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap().to_string();
    let (tx, _tail) = setup(&dd, 4).await; // tiny seal threshold

    // produce enough to force at least one seal + upload
    for i in 0..10u8 {
        let hwm = produce(&tx, vec![i; 8]).await;
        assert_eq!(hwm, i as i64);
    }
    // allow the uploader to drain
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // recovery sees uploaded segments + remaining active WAL
    let store = build_store(
        &ObjectStoreConfig {
            backend: "filesystem".into(),
            bucket: "b".into(),
            prefix: "".into(),
            endpoint: "".into(),
            region: "us-east-1".into(),
        },
        &dd,
    )
    .unwrap();
    let r = kafkrs_server::recovery::recover_partition(&dd, "t", 0, &store, "")
        .await
        .unwrap();
    // next_offset must cover everything produced (10 records → offsets 0..=9)
    assert!(r.next_offset >= 10, "next_offset = {}", r.next_offset);
}
