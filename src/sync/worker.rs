//! Sync Worker - 事件驱动同步
//!
//! - trigger_session(id): 有数据变更时，只同步该 session
//! - 定时全量对齐: 每 interval_seconds 扫一次兜底补漏

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{error, info, warn};

use super::{FilterConfig, SyncClient, SyncConfig, SyncDb};

const FAILURE_COOLDOWN: Duration = Duration::from_secs(60);

pub struct SyncWorker {
    trigger_tx: mpsc::UnboundedSender<String>,
    shutdown_tx: Option<mpsc::Sender<()>>,
    paused: Arc<AtomicBool>,
}

impl SyncWorker {
    pub fn start(
        config: SyncConfig,
        filter_config: FilterConfig,
        sync_db: Arc<SyncDb>,
        db_dir: &Path,
    ) -> Result<Self> {
        let paused = Arc::new(AtomicBool::new(false));

        if !config.enabled {
            info!("sync disabled, worker not started");
            let (trigger_tx, _) = mpsc::unbounded_channel();
            return Ok(Self {
                trigger_tx,
                shutdown_tx: None,
                paused,
            });
        }

        if let Ok(Some(v)) = sync_db.get_state("sync_paused") {
            if v == "true" {
                paused.store(true, Ordering::Relaxed);
                info!("sync worker restored paused state");
            }
        }

        let client = SyncClient::new(config.clone(), filter_config, sync_db.clone(), db_dir)?;
        let interval = Duration::from_secs(config.interval_seconds);
        let (trigger_tx, trigger_rx) = mpsc::unbounded_channel::<String>();
        let paused_clone = paused.clone();
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            info!("sync worker started");

            if sync_db.get_state("api_key").ok().flatten().is_none()
                && !config.master_key.is_empty()
                && !config.name.is_empty()
            {
                info!("no api_key found, registering as '{}'", config.name);
                match client.register().await {
                    Ok(_) => {}
                    Err(e) => error!("self-registration failed: {e}"),
                }
            }

            run_event_loop(&client, trigger_rx, &paused_clone, shutdown_rx, interval).await;

            info!("sync worker exited");
        });

        Ok(Self {
            trigger_tx,
            shutdown_tx: Some(shutdown_tx),
            paused,
        })
    }

    pub fn trigger_session(&self, session_id: &str) {
        let _ = self.trigger_tx.send(session_id.to_string());
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
    }

    pub fn is_running(&self) -> bool {
        self.shutdown_tx.is_some()
    }

    pub fn pause(&self, sync_db: &SyncDb) {
        self.paused.store(true, Ordering::Relaxed);
        let _ = sync_db.set_state("sync_paused", "true");
        info!("sync paused");
    }

    pub fn resume(&self, sync_db: &SyncDb) {
        self.paused.store(false, Ordering::Relaxed);
        let _ = sync_db.set_state("sync_paused", "false");
        info!("sync resumed");
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

async fn run_event_loop(
    client: &SyncClient,
    mut trigger_rx: mpsc::UnboundedReceiver<String>,
    paused: &AtomicBool,
    mut shutdown_rx: mpsc::Receiver<()>,
    interval: Duration,
) {
    let mut periodic = tokio::time::interval(interval);
    periodic.tick().await;
    let mut failed: HashMap<String, Instant> = HashMap::new();

    loop {
        if paused.load(Ordering::Relaxed) {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
            }
        }

        tokio::select! {
            _ = shutdown_rx.recv() => break,

            Some(session_id) = trigger_rx.recv() => {
                if paused.load(Ordering::Relaxed) { continue; }

                let mut ids = HashSet::new();
                ids.insert(session_id);
                while let Ok(id) = trigger_rx.try_recv() {
                    ids.insert(id);
                }

                let now = Instant::now();
                for id in &ids {
                    if let Some(fail_at) = failed.get(id.as_str()) {
                        if now.duration_since(*fail_at) < FAILURE_COOLDOWN {
                            continue;
                        }
                    }

                    match client.sync_single_session(id).await {
                        Ok(stats) => {
                            failed.remove(id.as_str());
                            if stats.messages_pushed > 0 {
                                info!(
                                    "sync {}: {} pushed",
                                    &id[..8.min(id.len())],
                                    stats.messages_pushed
                                );
                            }
                        }
                        Err(e) => {
                            failed.insert(id.clone(), now);
                            error!("sync {} failed: {:#}", &id[..8.min(id.len())], e);
                        }
                    }
                }
            }

            _ = periodic.tick() => {
                if paused.load(Ordering::Relaxed) { continue; }
                failed.clear();
                match client.sync_once().await {
                    Ok(stats) => {
                        if stats.sessions_synced > 0 || stats.sessions_failed > 0 {
                            info!(
                                "periodic sync: {} synced, {} pushed, {} failed",
                                stats.sessions_synced, stats.messages_pushed, stats.sessions_failed
                            );
                        }
                    }
                    Err(e) => error!("periodic sync failed: {e}"),
                }
            }
        }
    }
}
