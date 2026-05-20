use clap::Parser;
use log::{error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::{broadcast, mpsc, RwLock};

use kafkrs_models::topic::ResolvedTopicConfig;

use kafkrs_server::config;
use kafkrs_server::listener::{Listener, PartitionHandle, SharedState};
use kafkrs_server::object_store::build_store;
use kafkrs_server::partition_writer::PartitionWriter;
use kafkrs_server::recovery::recover_partition;
use kafkrs_server::topic_registry::{RegistryMsg, TopicRegistry};
use kafkrs_server::uploader::{Uploader, UploaderMsg};

#[derive(Parser)]
struct Cli {
    config_path: Option<String>,
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let args: Cli = Cli::parse();
    let config_path: String = args
        .config_path
        .unwrap_or_else(|| "./config.toml".to_string());
    let cfg: kafkrs_models::config::Config = config::load_config(config_path);

    let store: Arc<dyn ::object_store::ObjectStore> =
        build_store(&cfg.object_store, &cfg.data_dir).expect("object store");
    let prefix: String = cfg.object_store.prefix.clone();

    // Topic registry actor.
    let (reg_tx, reg_rx): (mpsc::Sender<RegistryMsg>, mpsc::Receiver<RegistryMsg>) =
        mpsc::channel(64);
    let registry: TopicRegistry = TopicRegistry::load(
        cfg.data_dir.clone(),
        cfg.broker.disk_type.clone(),
        store.clone(),
        prefix.clone(),
        reg_rx,
    )
    .expect("load topic registry");

    // Snapshot existing topics before moving the actor into its spawn.
    let known: Vec<(String, u32, ResolvedTopicConfig)> = registry.snapshot();

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Bring up each known partition independently (spec risk: startup must not
    // serialize on the slowest manifest GET — each task is independent).
    for (topic, pcount, rtc) in known {
        for p in 0..pcount {
            spawn_partition(
                &cfg.data_dir,
                &topic,
                p,
                rtc,
                store.clone(),
                prefix.clone(),
                partitions.clone(),
            )
            .await;
        }
    }

    tokio::spawn(registry.run());

    let state: SharedState = SharedState {
        partitions: partitions.clone(),
        registry: reg_tx.clone(),
        store: store.clone(),
        prefix: prefix.clone(),
        auto_create: cfg.broker.auto_create_topics,
        default_partition_count: cfg.broker.default_partition_count,
    };

    for port in cfg.ports.clone() {
        let addr: String = format!("{}:{}", cfg.address, port);
        let listener: TcpListener = TcpListener::bind(&addr).await.expect("bind");
        info!("Listening on {addr}");
        let st: SharedState = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => {
                        let st2: SharedState = st.clone();
                        tokio::spawn(async move { Listener::new(socket, st2).process().await });
                    }
                    Err(e) => error!("accept error: {e}"),
                }
            }
        });
    }

    match signal::ctrl_c().await {
        Ok(()) => info!("Shutdown signal received. Goodbye"),
        Err(e) => error!("signal error: {e}"),
    }
}

async fn spawn_partition(
    data_dir: &str,
    topic: &str,
    partition: u32,
    cfg: ResolvedTopicConfig,
    store: Arc<dyn ::object_store::ObjectStore>,
    prefix: String,
    partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
) {
    let rec: kafkrs_server::recovery::PartitionRecovery =
        recover_partition(data_dir, topic, partition, &store, &prefix)
            .await
            .expect("recover partition");

    let (utx, urx): (mpsc::Sender<UploaderMsg>, mpsc::Receiver<UploaderMsg>) =
        mpsc::channel::<UploaderMsg>(64);
    let (dtx, mut drx): (
        mpsc::Sender<kafkrs_server::uploader::SegmentDurable>,
        mpsc::Receiver<kafkrs_server::uploader::SegmentDurable>,
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
        mpsc::Sender<kafkrs_server::partition_writer::PwMsg>,
        mpsc::Receiver<kafkrs_server::partition_writer::PwMsg>,
    ) = mpsc::channel(256);
    let (tail, _): (broadcast::Sender<i64>, broadcast::Receiver<i64>) = broadcast::channel(1024);

    // Re-queue orphan sealed segments for upload.
    for (base, records) in rec.orphan_segments {
        let last: &kafkrs_models::record::Record = records.last().unwrap();
        let _ = utx
            .send(UploaderMsg::Upload(kafkrs_server::uploader::SealedBatch {
                base_offset: base,
                last_offset: last.offset,
                base_timestamp_ns: records.first().unwrap().timestamp_ns,
                last_timestamp_ns: last.timestamp_ns,
                records,
            }))
            .await;
    }

    let pw: PartitionWriter = PartitionWriter::new(
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

    let pw_tx_for_durable: mpsc::Sender<kafkrs_server::partition_writer::PwMsg> = pw_tx.clone();
    tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let _ = pw_tx_for_durable
                .send(kafkrs_server::partition_writer::PwMsg::SegmentDurable(d))
                .await;
        }
    });

    tokio::spawn(pw.run());
    partitions.write().await.insert(
        (topic.to_string(), partition),
        PartitionHandle { pw_tx, tail },
    );
}
