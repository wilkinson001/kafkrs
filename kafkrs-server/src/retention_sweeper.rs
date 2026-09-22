//! Broker-wide RetentionSweeper. Ticks on a configurable interval; for each
//! known partition, enqueues an UploaderMsg::RetentionKick so idle partitions
//! (those not currently receiving writes) still evict expired segments.

use crate::metrics::{RETENTION_SWEEP_KICKS_DROPPED, RETENTION_SWEEP_KICKS_SENT};
use crate::uploader::UploaderMsg;
use crate::wire::PartitionHandle;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};

pub struct RetentionSweeper {
    partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
    sweep_interval: Duration,
}

impl RetentionSweeper {
    pub fn new(
        partitions: Arc<RwLock<HashMap<(String, u32), PartitionHandle>>>,
        sweep_interval: Duration,
    ) -> RetentionSweeper {
        RetentionSweeper {
            partitions,
            sweep_interval,
        }
    }

    pub async fn run(self) {
        let mut ticker = interval(self.sweep_interval);
        // Skip the immediate first tick: interval() fires immediately on the
        // first .tick() call, which would race with startup. Wait one interval
        // before the first sweep.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let snapshot: Vec<mpsc::Sender<UploaderMsg>> = {
                let guard = self.partitions.read().await;
                guard.values().map(|h| h.uploader_tx.clone()).collect()
            };
            for tx in snapshot {
                // Best-effort; drop the kick if the Uploader is busy.
                // Retention is idempotent so lost kicks are harmless.
                match tx.try_send(UploaderMsg::RetentionKick) {
                    Ok(_) => {
                        metrics::counter!(RETENTION_SWEEP_KICKS_SENT).increment(1);
                    }
                    Err(_) => {
                        metrics::counter!(RETENTION_SWEEP_KICKS_DROPPED).increment(1);
                    }
                }
            }
        }
    }
}
