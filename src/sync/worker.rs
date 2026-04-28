//! Sync Worker - WebSocket streaming + HTTP fallback, event-driven
//!
//! - 优先 WebSocket 流式推送（长连接 + heartbeat）
//! - WS 不可用时自动 fallback 到 HTTP 轮询
//! - trigger_session(id): 事件驱动，只同步变化的 session
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

use super::{FilterConfig, SyncClient, SyncConfig, SyncDb, SyncStream};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(90);
const RECONNECT_BASE: Duration = Duration::from_secs(1);
const RECONNECT_CAP: Duration = Duration::from_secs(60);
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

        let ca_pem: Option<Vec<u8>> = config.ca_cert.as_ref().and_then(|p| {
            let expanded = shellexpand::tilde(p);
            std::fs::read(expanded.as_ref()).ok()
        });

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

            run_main_loop(
                &config,
                &client,
                ca_pem.as_deref(),
                trigger_rx,
                &paused_clone,
                shutdown_rx,
                interval,
            )
            .await;

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

async fn run_main_loop(
    config: &SyncConfig,
    client: &SyncClient,
    ca_pem: Option<&[u8]>,
    mut trigger_rx: mpsc::UnboundedReceiver<String>,
    paused: &AtomicBool,
    mut shutdown_rx: mpsc::Receiver<()>,
    interval: Duration,
) {
    let mut reconnect_delay = RECONNECT_BASE;

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        if paused.load(Ordering::Relaxed) {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
            }
        }

        let api_key = match client.effective_api_key() {
            Ok(k) => k,
            Err(e) => {
                warn!("no api_key, HTTP-only: {e}");
                run_http_event_loop(client, &mut trigger_rx, paused, &mut shutdown_rx, interval)
                    .await;
                break;
            }
        };

        match SyncStream::connect(&config.server, ca_pem).await {
            Ok(mut stream) => {
                let device_id = client
                    .sync_db()
                    .get_or_init_device_id()
                    .unwrap_or_else(|_| "unknown".to_string());

                match stream.authenticate(&device_id, &api_key).await {
                    Ok(()) => {
                        reconnect_delay = RECONNECT_BASE;
                        info!("ws streaming mode active");

                        let exit = run_ws_event_loop(
                            &mut stream,
                            client,
                            &mut trigger_rx,
                            paused,
                            &mut shutdown_rx,
                            interval,
                        )
                        .await;

                        stream.close().await;

                        if exit {
                            break;
                        }

                        warn!("ws disconnected, will reconnect in {}s", reconnect_delay.as_secs());
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

        // HTTP fallback for one interval, then retry WS
        let fallback_exit = run_http_fallback_once(
            client,
            &mut trigger_rx,
            paused,
            &mut shutdown_rx,
            interval,
            reconnect_delay,
        )
        .await;

        if fallback_exit {
            break;
        }

        reconnect_delay = (reconnect_delay * 2).min(RECONNECT_CAP);
    }
}

/// WS event loop: trigger_session → sync_session_ws, periodic → sync_once_ws
async fn run_ws_event_loop(
    stream: &mut SyncStream,
    client: &SyncClient,
    trigger_rx: &mut mpsc::UnboundedReceiver<String>,
    paused: &AtomicBool,
    shutdown_rx: &mut mpsc::Receiver<()>,
    interval: Duration,
) -> bool {
    let mut periodic = tokio::time::interval(interval);
    periodic.tick().await;
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await;

    loop {
        if paused.load(Ordering::Relaxed) {
            return false;
        }

        tokio::select! {
            _ = shutdown_rx.recv() => return true,

            Some(session_id) = trigger_rx.recv() => {
                if paused.load(Ordering::Relaxed) { continue; }

                let mut ids = HashSet::new();
                ids.insert(session_id);
                while let Ok(id) = trigger_rx.try_recv() {
                    ids.insert(id);
                }

                for id in &ids {
                    if let Err(e) = client.sync_single_session_ws(stream, id).await {
                        error!("ws sync {} failed: {:#}", &id[..8.min(id.len())], e);
                        return false;
                    }
                }
            }

            _ = periodic.tick() => {
                if paused.load(Ordering::Relaxed) { continue; }
                if let Err(e) = client.sync_once_ws(stream).await {
                    error!("ws periodic sync failed: {e}");
                    return false;
                }
            }

            _ = heartbeat.tick() => {
                let ts = client.query_last_db_updated_at();
                if let Err(e) = stream.heartbeat(ts).await {
                    error!("ws heartbeat failed: {e}");
                    return false;
                }
            }
        }
    }
}

/// HTTP event loop (permanent, when WS is not available at all)
async fn run_http_event_loop(
    client: &SyncClient,
    trigger_rx: &mut mpsc::UnboundedReceiver<String>,
    paused: &AtomicBool,
    shutdown_rx: &mut mpsc::Receiver<()>,
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

/// HTTP fallback: run event loop for reconnect_delay duration, then return to retry WS
async fn run_http_fallback_once(
    client: &SyncClient,
    trigger_rx: &mut mpsc::UnboundedReceiver<String>,
    paused: &AtomicBool,
    shutdown_rx: &mut mpsc::Receiver<()>,
    interval: Duration,
    reconnect_wait: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + reconnect_wait;
    let mut periodic = tokio::time::interval(interval);
    periodic.tick().await;

    // Immediate sync on entering fallback
    if !paused.load(Ordering::Relaxed) {
        match client.sync_once().await {
            Ok(stats) => {
                if stats.sessions_synced > 0 {
                    info!("http fallback: {} synced, {} pushed", stats.sessions_synced, stats.messages_pushed);
                }
            }
            Err(e) => error!("http fallback sync failed: {e}"),
        }
    }

    loop {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        if paused.load(Ordering::Relaxed) {
            return false;
        }

        let remaining = deadline - tokio::time::Instant::now();

        tokio::select! {
            _ = shutdown_rx.recv() => return true,

            Some(session_id) = trigger_rx.recv() => {
                let mut ids = HashSet::new();
                ids.insert(session_id);
                while let Ok(id) = trigger_rx.try_recv() {
                    ids.insert(id);
                }

                for id in &ids {
                    match client.sync_single_session(&id).await {
                        Ok(stats) => {
                            if stats.messages_pushed > 0 {
                                info!("http fallback sync {}: {} pushed", &id[..8.min(id.len())], stats.messages_pushed);
                            }
                        }
                        Err(e) => error!("http fallback sync {} failed: {:#}", &id[..8.min(id.len())], e),
                    }
                }
            }

            _ = tokio::time::sleep(remaining.min(interval)) => {
                if !paused.load(Ordering::Relaxed) {
                    match client.sync_once().await {
                        Ok(stats) => {
                            if stats.sessions_synced > 0 {
                                info!("http fallback periodic: {} synced", stats.sessions_synced);
                            }
                        }
                        Err(e) => error!("http fallback periodic failed: {e}"),
                    }
                }
            }
        }
    }
}
