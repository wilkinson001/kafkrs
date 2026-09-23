use clap::Parser;
use log::{error, info};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::RwLock;
use tokio::time::Duration;

use kafkrs_server::broker_identity::resolve_identity;
use kafkrs_server::config;
use kafkrs_server::object_store::build_store;
use kafkrs_server::retention_sweeper::RetentionSweeper;
use kafkrs_server::startup::spawn_partition;
use kafkrs_server::topic_registry::{RegistryMsg, TopicRegistry};
use kafkrs_server::wire::dispatch::PartitionSpawnLocks;
use kafkrs_server::wire::{accept_loop, PartitionHandle, SharedState};

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

    // Broker identity — fail-fast if cluster_id missing. Panics with a clear
    // error naming the field.
    let wire_port_for_advertise = *cfg
        .ports
        .wire
        .first()
        .expect("at least one wire port must be configured (ports.wire = [...])");
    let identity = resolve_identity(
        &cfg.broker,
        &cfg.address,
        wire_port_for_advertise,
        std::path::Path::new(&cfg.data_dir),
    )
    .unwrap_or_else(|e| panic!("broker identity resolution failed: {e}"));

    kafkrs_server::metrics::init(&cfg.ports, cfg.broker.metrics_high_cardinality)
        .expect("metrics init");
    tokio::spawn(kafkrs_server::metrics::uptime_updater());

    let store: Arc<dyn ::object_store::ObjectStore> =
        build_store(&cfg.object_store, &cfg.data_dir).expect("object store");
    let prefix: String = cfg.object_store.prefix.clone();

    // Topic registry actor.
    let (reg_tx, reg_rx) = tokio::sync::mpsc::channel::<RegistryMsg>(64);
    let registry: TopicRegistry = TopicRegistry::load(
        cfg.data_dir.clone(),
        cfg.broker.disk_type.clone(),
        store.clone(),
        prefix.clone(),
        reg_rx,
    )
    .expect("load topic registry");

    // Snapshot existing topics before moving the actor into its spawn.
    let known = registry.snapshot();

    let partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>> =
        Arc::new(RwLock::new(HashMap::new()));

    let spawn_locks: PartitionSpawnLocks = Arc::new(StdMutex::new(HashMap::new()));

    // Bring up each known partition independently (spec risk: startup must not
    // serialize on the slowest manifest GET — each task is independent).
    for (topic, uuid, pcount, rtc) in known {
        for p in 0..pcount {
            spawn_partition(
                &cfg.data_dir,
                &topic,
                uuid.clone(),
                p,
                rtc,
                store.clone(),
                prefix.clone(),
                partitions.clone(),
                spawn_locks.clone(),
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
        data_dir: cfg.data_dir.clone(),
        disk_type: cfg.broker.disk_type.clone(),
        spawn_locks: spawn_locks.clone(),
        identity: identity.clone(),
    };

    let sweep_interval =
        Duration::from_millis(cfg.broker.retention_sweep_interval_ms.unwrap_or(60_000));
    tokio::spawn(RetentionSweeper::new(partitions.clone(), sweep_interval).run());

    // Resume any in-flight deletion sweeps left over from a prior broker run.
    let pending = kafkrs_server::pending_deletes::load_all(&cfg.data_dir)
        .await
        .expect("load pending deletes");
    for record in pending {
        metrics::gauge!(kafkrs_server::metrics::DELETE_PENDING_TOPICS).increment(1.0);
        let store = store.clone();
        let prefix = prefix.clone();
        let data_dir = cfg.data_dir.clone();
        tokio::spawn(async move {
            if let Err(e) =
                kafkrs_server::deletion::sweep_deletion(record, store, prefix, data_dir).await
            {
                log::warn!("startup-resumed sweep_deletion failed: {e:?}");
            }
        });
    }

    for port in cfg.ports.wire.clone() {
        let addr: String = format!("{}:{}", cfg.address, port);
        let listener: TcpListener = TcpListener::bind(&addr).await.expect("bind");
        info!("Listening on {addr}");
        let st: SharedState = state.clone();
        tokio::spawn(accept_loop(listener, st));
    }

    // Wire listeners are bound and startup work is complete — flip the
    // readiness flag so `/ready` returns 200 to load balancers and K8s
    // readiness probes.
    kafkrs_server::metrics::set_ready(true);

    match signal::ctrl_c().await {
        Ok(()) => info!("Shutdown signal received. Goodbye"),
        Err(e) => error!("signal error: {e}"),
    }
}
