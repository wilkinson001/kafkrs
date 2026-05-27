//! Partition startup logic shared between initial boot and auto-create paths.

use crate::partition_writer::PartitionWriter;
use crate::recovery::recover_partition;
use crate::uploader::{Uploader, UploaderMsg};
use crate::wire::dispatch::PartitionSpawnLocks;
use crate::wire::PartitionHandle;
use kafkrs_models::topic::ResolvedTopicConfig;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};

pub async fn spawn_partition(
    data_dir: &str,
    topic: &str,
    partition: u32,
    cfg: ResolvedTopicConfig,
    store: Arc<dyn object_store::ObjectStore>,
    prefix: String,
    partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
    spawn_locks: PartitionSpawnLocks,
) {
    let _ = &spawn_locks; // unused in Task 2; Task 3 adds the idempotency guard.
    let rec = recover_partition(data_dir, topic, partition, &store, &prefix)
        .await
        .expect("recover partition");

    let (utx, urx): (mpsc::Sender<UploaderMsg>, mpsc::Receiver<UploaderMsg>) =
        mpsc::channel::<UploaderMsg>(64);
    let (dtx, mut drx): (
        mpsc::Sender<crate::uploader::SegmentDurable>,
        mpsc::Receiver<crate::uploader::SegmentDurable>,
    ) = mpsc::channel(64);
    tokio::spawn(
        Uploader::new(
            store.clone(),
            prefix.clone(),
            topic.to_string(),
            partition,
            urx,
            dtx,
        )
        .run(),
    );

    let (pw_tx, pw_rx): (
        mpsc::Sender<crate::partition_writer::PwMsg>,
        mpsc::Receiver<crate::partition_writer::PwMsg>,
    ) = mpsc::channel(256);
    let (tail, _): (broadcast::Sender<i64>, broadcast::Receiver<i64>) = broadcast::channel(1024);

    // Re-queue orphan sealed segments for upload.
    for (base, records) in rec.orphan_segments {
        let last = records.last().unwrap();
        let _ = utx
            .send(UploaderMsg::Upload(crate::uploader::SealedBatch {
                base_offset: base,
                last_offset: last.offset,
                base_timestamp_ns: records.first().unwrap().timestamp_ns,
                last_timestamp_ns: last.timestamp_ns,
                records,
            }))
            .await;
    }

    let pw = PartitionWriter::new(
        data_dir.to_string(),
        topic.to_string(),
        partition,
        cfg,
        rec.next_offset,
        rec.active_records,
        pw_rx,
        utx,
        tail.clone(),
    )
    .expect("partition writer");

    let pw_tx_for_durable: mpsc::Sender<crate::partition_writer::PwMsg> = pw_tx.clone();
    tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let _ = pw_tx_for_durable
                .send(crate::partition_writer::PwMsg::SegmentDurable(d))
                .await;
        }
    });

    tokio::spawn(pw.run());
    partitions.write().await.insert(
        (topic.to_string(), partition),
        PartitionHandle { pw_tx, tail, cfg },
    );
}
