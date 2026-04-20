//! Sync Worker - 后台定时同步任务
//!
//! 事件驱动 + 兜底定时：
//! - 外部可通过 `trigger()` 手动触发（如 vimo-agent collect 完成后）
//! - 兜底每 interval_seconds 秒检查一次

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, Notify};
use tracing::{error, info};

use super::{FilterConfig, SyncClient, SyncConfig, SyncDb};

pub struct SyncWorker {
    trigger: Arc<Notify>,
    shutdown_tx: Option<mpsc::Sender<()>>,
}

impl SyncWorker {
    pub fn start(config: SyncConfig, filter_config: FilterConfig, db_dir: &Path) -> Result<Self> {
        if !config.enabled {
            info!("sync 未启用，worker 不启动");
            return Ok(Self {
                trigger: Arc::new(Notify::new()),
                shutdown_tx: None,
            });
        }

        let sync_db = Arc::new(SyncDb::open(db_dir)?);
        let client = SyncClient::new(config.clone(), filter_config, sync_db, db_dir)?;
        let interval = std::time::Duration::from_secs(config.interval_seconds);
        let trigger = Arc::new(Notify::new());
        let trigger_clone = trigger.clone();
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            info!("sync worker 已启动，interval={}s", interval.as_secs());

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        info!("sync worker 收到 shutdown 信号");
                        break;
                    }
                    _ = trigger_clone.notified() => {
                        match client.sync_once().await {
                            Ok(stats) => {
                                if stats.sessions_synced > 0 || stats.sessions_failed > 0 {
                                    info!("sync triggered: {} synced, {} failed", stats.sessions_synced, stats.sessions_failed);
                                }
                            }
                            Err(e) => error!("sync triggered 失败: {e}"),
                        }
                    }
                    _ = tokio::time::sleep(interval) => {
                        match client.sync_once().await {
                            Ok(stats) => {
                                if stats.sessions_synced > 0 || stats.sessions_failed > 0 {
                                    info!("sync periodic: {} synced, {} failed", stats.sessions_synced, stats.sessions_failed);
                                }
                            }
                            Err(e) => error!("sync periodic 失败: {e}"),
                        }
                    }
                }
            }

            info!("sync worker 已退出");
        });

        Ok(Self {
            trigger,
            shutdown_tx: Some(shutdown_tx),
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
}
