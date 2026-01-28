//! 文件监听器
//!
//! 监听 AI CLI 会话文件变化，触发 Collection

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use tokio::sync::mpsc;

use super::broadcaster::Broadcaster;
use crate::protocol::Event;
use crate::{all_watch_configs, Collector, SessionDB};

/// 文件监听器
pub struct FileWatcher {
    /// 数据库连接
    db: Arc<SessionDB>,
    /// 广播器
    broadcaster: Arc<Broadcaster>,
    /// 支持的文件扩展名
    supported_extensions: HashSet<String>,
}

impl FileWatcher {
    /// 创建文件监听器
    pub fn new(db: Arc<SessionDB>, broadcaster: Arc<Broadcaster>) -> Arc<Self> {
        // 从适配器收集所有支持的扩展名
        let supported_extensions: HashSet<String> = all_watch_configs()
            .iter()
            .flat_map(|c| c.extensions.iter().map(|e| e.to_string()))
            .collect();

        Arc::new(Self {
            db,
            broadcaster,
            supported_extensions,
        })
    }

    /// 启动文件监听
    pub async fn start(self: Arc<Self>) -> Result<()> {
        let (tx, mut rx) = mpsc::channel::<PathBuf>(100);

        // 创建 debouncer（2秒防抖）
        let tx_clone = tx.clone();
        let mut debouncer = new_debouncer(Duration::from_secs(2), move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
            if let Ok(events) = res {
                for event in events {
                    if event.kind == DebouncedEventKind::Any {
                        let _ = tx_clone.blocking_send(event.path);
                    }
                }
            }
        })?;

        // 使用适配器自注册的监听配置
        let watch_configs = all_watch_configs();

        for config in &watch_configs {
            let recursive_mode = if config.recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };

            match debouncer.watcher().watch(&config.path, recursive_mode) {
                Ok(_) => {
                    tracing::info!(
                        "👁️ Watching {} directory: {:?} (extensions: {:?})",
                        config.name,
                        config.path,
                        config.extensions
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "⚠️ Failed to watch {} directory {:?}: {}",
                        config.name,
                        config.path,
                        e
                    );
                }
            }
        }

        if watch_configs.is_empty() {
            tracing::warn!("⚠️ No valid watch directories found");
        }

        tracing::info!(
            "🔄 File watcher service started ({} directories)",
            watch_configs.len()
        );

        // 处理文件变化事件
        let watcher = self.clone();
        tokio::spawn(async move {
            // 保持 debouncer 存活
            let _debouncer = debouncer;

            while let Some(path) = rx.recv().await {
                watcher.handle_file_change(&path).await;
            }
        });

        Ok(())
    }

    /// 处理文件变化
    async fn handle_file_change(&self, path: &Path) {
        // 检查扩展名
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => return,
        };

        if !self.supported_extensions.contains(ext) {
            return;
        }

        tracing::debug!("📝 File change detected: {:?}", path);

        if let Err(e) = self.trigger_collect(path).await {
            tracing::error!("Failed to process file change {:?}: {}", path.file_name(), e);
        }
    }

    /// 触发 Collection（供外部调用，如 Kit 通知）
    pub async fn trigger_collect(&self, path: &Path) -> Result<()> {
        let path_str = path.to_str().ok_or_else(|| {
            anyhow::anyhow!("Cannot convert path: {:?}", path)
        })?.to_string();

        let path_clone = path.to_path_buf();

        // 使用 spawn_blocking 避免阻塞 tokio runtime
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || {
            let collector = Collector::new(&db);
            collector.collect_by_path(&path_str)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {}", e))??;

        if result.messages_inserted > 0 {
            tracing::debug!(
                "📝 Collection complete: {:?} → {} new messages",
                path_clone.file_name().unwrap_or_default(),
                result.messages_inserted
            );

            // 提取 session_id
            let session_id = path_clone
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_default();

            // 广播 NewMessages 事件
            self.broadcaster.broadcast(Event::NewMessages {
                session_id,
                path: path_clone,
                count: result.messages_inserted,
                message_ids: result.new_message_ids,
            });
        }

        Ok(())
    }
}
