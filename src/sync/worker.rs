//! Sync Worker - WebSocket streaming + HTTP fallback
//!
//! 优先 WebSocket 流式推送，失败后自动 fallback 到 HTTP 轮询。
//! 支持 pause/resume 控制。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, Notify};
use tracing::{error, info, warn};

use super::{FilterConfig, SyncClient, SyncConfig, SyncDb, SyncStream};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(90);
const RECONNECT_BASE: Duration = Duration::from_secs(1);
const RECONNECT_CAP: Duration = Duration::from_secs(60);

pub struct SyncWorker {
    trigger: Arc<Notify>,
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
            return Ok(Self {
                trigger: Arc::new(Notify::new()),
                shutdown_tx: None,
                paused,
            });
        }

        // Restore pause state
        if let Ok(Some(v)) = sync_db.get_state("sync_paused") {
            if v == "true" {
                paused.store(true, Ordering::Relaxed);
                info!("sync worker restored paused state");
            }
        }

        let client = SyncClient::new(config.clone(), filter_config, sync_db.clone(), db_dir)?;
        let http_interval = Duration::from_secs(config.interval_seconds);
        let trigger = Arc::new(Notify::new());
        let trigger_clone = trigger.clone();
        let paused_clone = paused.clone();
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            info!("sync worker started");

            let mut reconnect_delay = RECONNECT_BASE;

            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }

                if paused_clone.load(Ordering::Relaxed) {
                    tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
                    }
                }

                // Try WebSocket streaming mode
                match SyncStream::connect(&config.server).await {
                    Ok(mut stream) => {
                        let device_id = sync_db
                            .get_or_init_device_id()
                            .unwrap_or_else(|_| "unknown".to_string());

                        match stream.authenticate(&device_id, &config.api_key).await {
                            Ok(()) => {
                                reconnect_delay = RECONNECT_BASE;
                                info!("ws streaming mode active");

                                let exit = run_streaming_loop(
                                    &mut stream,
                                    &client,
                                    &trigger_clone,
                                    &paused_clone,
                                    &mut shutdown_rx,
                                )
                                .await;

                                stream.close().await;

                                if exit {
                                    break;
                                }

                                warn!("ws disconnected, will reconnect");
                            }
                            Err(e) => {
                                warn!("ws auth failed: {e}, falling back to HTTP");
                                stream.close().await;
                            }
                        }
                    }
                    Err(e) => {
                        info!("ws connect failed ({e}), using HTTP fallback");
                    }
                }

                // HTTP fallback: run one poll cycle, then wait before retrying ws
                if !paused_clone.load(Ordering::Relaxed) {
                    run_http_fallback(
                        &client,
                        &trigger_clone,
                        &paused_clone,
                        &mut shutdown_rx,
                        http_interval,
                        reconnect_delay,
                    )
                    .await;
                }

                reconnect_delay = (reconnect_delay * 2).min(RECONNECT_CAP);
            }

            info!("sync worker exited");
        });

        Ok(Self {
            trigger,
            shutdown_tx: Some(shutdown_tx),
            paused,
        })
    }

    pub fn trigger(&self) {
        self.trigger.notify_one();
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
        self.trigger.notify_one();
        info!("sync resumed");
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

/// Main streaming loop. Returns true if shutdown requested.
async fn run_streaming_loop(
    stream: &mut SyncStream,
    client: &SyncClient,
    trigger: &Notify,
    paused: &AtomicBool,
    shutdown_rx: &mut mpsc::Receiver<()>,
) -> bool {
    let mut heartbeat_interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat_interval.tick().await; // skip first immediate tick

    loop {
        if paused.load(Ordering::Relaxed) {
            return false;
        }

        tokio::select! {
            _ = shutdown_rx.recv() => return true,

            _ = trigger.notified() => {
                if let Err(e) = client.sync_once_ws(stream).await {
                    error!("ws sync failed: {e}");
                    return false; // reconnect
                }
            }

            _ = heartbeat_interval.tick() => {
                let ts = client.query_last_db_updated_at();
                if let Err(e) = stream.heartbeat(ts).await {
                    error!("ws heartbeat failed: {e}");
                    return false; // reconnect
                }
            }
        }
    }
}

/// HTTP fallback: one sync cycle + wait for reconnect_delay before retrying ws.
async fn run_http_fallback(
    client: &SyncClient,
    trigger: &Notify,
    paused: &AtomicBool,
    shutdown_rx: &mut mpsc::Receiver<()>,
    interval: Duration,
    reconnect_wait: Duration,
) {
    // Do one immediate HTTP sync
    match client.sync_once().await {
        Ok(stats) => {
            if stats.sessions_synced > 0 || stats.sessions_failed > 0 {
                info!(
                    "http sync: {} synced, {} failed",
                    stats.sessions_synced, stats.sessions_failed
                );
            }
        }
        Err(e) => error!("http sync failed: {e}"),
    }

    // Wait before attempting ws reconnection, but remain responsive to triggers/shutdown
    let deadline = tokio::time::Instant::now() + reconnect_wait;
    loop {
        if paused.load(Ordering::Relaxed) {
            return;
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }

        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = trigger.notified() => {
                match client.sync_once().await {
                    Ok(stats) => {
                        if stats.sessions_synced > 0 || stats.sessions_failed > 0 {
                            info!(
                                "http sync triggered: {} synced, {} failed",
                                stats.sessions_synced, stats.sessions_failed
                            );
                        }
                    }
                    Err(e) => error!("http sync triggered failed: {e}"),
                }
            }
            _ = tokio::time::sleep(remaining.min(interval)) => {
                match client.sync_once().await {
                    Ok(stats) => {
                        if stats.sessions_synced > 0 || stats.sessions_failed > 0 {
                            info!(
                                "http sync periodic: {} synced, {} failed",
                                stats.sessions_synced, stats.sessions_failed
                            );
                        }
                    }
                    Err(e) => error!("http sync periodic failed: {e}"),
                }
            }
        }
    }
}
