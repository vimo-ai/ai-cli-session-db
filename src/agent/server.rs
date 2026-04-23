//! Agent 服务器
//!
//! 跨平台 IPC 服务，处理客户端连接和请求
//! - Unix: Unix Domain Socket
//! - Windows: Named Pipe

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    ListenerOptions, Name,
};
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::interval;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::broadcaster::ConnectionManager;
use super::handler::Handler;
use super::watcher::FileWatcher;
use crate::protocol::{Request, Response};
use crate::sync::SyncWorker;
use crate::{DbConfig, SessionDB};

/// Agent 配置
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 数据目录（默认 ~/.vimo）
    pub data_dir: PathBuf,
    /// 空闲超时（秒）
    pub idle_timeout_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        let data_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".vimo");

        Self {
            data_dir,
            idle_timeout_secs: 30,
        }
    }
}

impl AgentConfig {
    /// Socket 路径 (Unix only, for cleanup)
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("agent.sock")
    }

    /// 获取跨平台 socket name
    #[cfg(unix)]
    pub fn socket_name(&self) -> Name<'static> {
        use interprocess::local_socket::ToFsName;
        self.socket_path()
            .to_fs_name::<GenericFilePath>()
            .unwrap()
            .to_owned()
    }

    #[cfg(windows)]
    pub fn socket_name(&self) -> Name<'static> {
        use interprocess::local_socket::ToNsName;
        // Windows Named Pipe: 用固定名称，避免路径问题
        "vimo-agent"
            .to_ns_name::<GenericNamespaced>()
            .unwrap()
            .to_owned()
    }

    /// PID 文件路径
    pub fn pid_path(&self) -> PathBuf {
        self.data_dir.join("agent.pid")
    }

    /// 数据库路径
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("db").join("ai-cli-session.db")
    }
}

/// Agent 服务
pub struct Agent {
    config: AgentConfig,
    #[allow(dead_code)]
    db: Arc<SessionDB>,
    connections: Arc<ConnectionManager>,
    watcher: Arc<FileWatcher>,
    handler: Arc<Handler>,
    #[allow(dead_code)]
    sync_worker: Arc<SyncWorker>,
    shutdown: Arc<AtomicBool>,
}

impl Agent {
    /// 创建 Agent
    pub fn new(config: AgentConfig) -> Result<Self> {
        // 确保数据目录存在
        fs::create_dir_all(&config.data_dir)
            .context("Failed to create data directory")?;
        fs::create_dir_all(config.data_dir.join("db"))
            .context("Failed to create database directory")?;

        // 连接数据库
        let db_config = DbConfig::local(config.db_path().to_str().unwrap());
        let db = Arc::new(SessionDB::connect(db_config)?);

        // 创建连接管理器
        let connections = ConnectionManager::new();

        // 创建文件监听器
        let watcher = FileWatcher::new(db.clone());

        // 加载 sync 配置并启动 worker
        let (sync_worker, sync_db) = {
            let sync_config = Self::load_sync_config(&config.data_dir);
            let filter_config = Self::load_filter_config(&config.data_dir);
            let db_dir = config.data_dir.join("db");
            let sync_db = Arc::new(crate::sync::SyncDb::open(&db_dir)?);
            let worker = Arc::new(SyncWorker::start(sync_config, filter_config, sync_db.clone(), &db_dir)?);
            (worker, sync_db)
        };

        // 创建处理器
        let handler = Arc::new(Handler::new(db.clone(), connections.clone(), watcher.clone(), sync_worker.clone(), sync_db));

        Ok(Self {
            config,
            db,
            connections,
            watcher,
            handler,
            sync_worker,
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 运行 Agent
    pub async fn run(self: Arc<Self>) -> Result<()> {
        // 写入 PID 文件
        self.write_pid_file()?;

        // 清理旧的 socket 文件 (Unix only)
        #[cfg(unix)]
        {
            let socket_path = self.config.socket_path();
            if socket_path.exists() {
                fs::remove_file(&socket_path)?;
            }
        }

        // 创建跨平台 IPC 监听器
        let listener = ListenerOptions::new()
            .name(self.config.socket_name())
            .create_tokio()
            .context("Failed to bind socket")?;

        // 设置 socket 权限为 0600 (Unix only)
        #[cfg(unix)]
        fs::set_permissions(
            self.config.socket_path(),
            fs::Permissions::from_mode(0o600),
        )?;

        tracing::info!("🚀 Agent started: {:?}", self.config.socket_path());

        // 启动时执行全量扫描（mtime 剪枝会跳过未变化的文件）
        {
            let db = self.db.clone();
            tokio::task::spawn_blocking(move || {
                let collector = crate::Collector::new(&db);
                match collector.collect_all() {
                    Ok(result) => {
                        if result.messages_inserted > 0 {
                            tracing::info!(
                                "📊 Startup scan complete: {} sessions, {} new messages",
                                result.sessions_scanned,
                                result.messages_inserted
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!("Startup scan failed: {}", e);
                    }
                }
            })
            .await
            .ok();
        }

        // 启动文件监听
        self.watcher.clone().start().await?;

        // 启动空闲检测
        let agent_for_idle = self.clone();
        tokio::spawn(async move {
            agent_for_idle.idle_checker().await;
        });

        // 终止信号：SIGTERM (Unix) / ctrl_c 统一为一个 future
        let shutdown_signal = async {
            #[cfg(unix)]
            {
                let mut sigterm = tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                ).expect("Failed to register SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("Received SIGINT, shutting down...");
                    }
                    _ = sigterm.recv() => {
                        tracing::info!("Received SIGTERM, shutting down gracefully...");
                    }
                }
            }
            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await.ok();
                tracing::info!("Received interrupt signal, shutting down...");
            }
        };
        tokio::pin!(shutdown_signal);

        // 接受连接
        loop {
            if self.shutdown.load(Ordering::Relaxed) && !self.connections.has_connections() {
                break;
            }

            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok(stream) => {
                            let agent = self.clone();
                            tokio::spawn(async move {
                                if let Err(e) = agent.handle_connection(stream).await {
                                    tracing::error!("Failed to handle connection: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to accept connection: {}", e);
                        }
                    }
                }
                _ = &mut shutdown_signal => {
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    continue;
                }
            }
        }

        self.cleanup();
        Ok(())
    }

    /// 处理单个连接
    async fn handle_connection(&self, stream: Stream) -> Result<()> {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);

        // 创建消息发送通道
        let (tx, mut rx) = mpsc::channel::<String>(100);

        // 注册连接
        let conn_id = self.connections.register(tx);
        tracing::debug!("📥 New connection: conn_id={}", conn_id);

        // 启动发送任务
        let write_handle = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if writer.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        // 读取请求
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    // 连接关闭
                    break;
                }
                Ok(_) => {
                    // 解析请求
                    let request: Request = match serde_json::from_str(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!("Failed to parse request: {}", e);
                            let response = Response::Error {
                                code: 400,
                                message: format!("Invalid JSON: {}", e),
                            };
                            let resp_json = serde_json::to_string(&response)?;
                            self.connections.try_send_to(conn_id, format!("{}\n", resp_json));
                            continue;
                        }
                    };

                    // 处理请求
                    let response = self.handler.handle(conn_id, request).await;
                    let resp_json = serde_json::to_string(&response)?;

                    // 发送响应
                    if !self.connections.send_to(conn_id, format!("{}\n", resp_json)).await {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("Read failed: {}", e);
                    break;
                }
            }
        }

        // 清理
        self.connections.unregister(conn_id);
        write_handle.abort();
        tracing::debug!("📤 Connection closed: conn_id={}", conn_id);

        Ok(())
    }

    /// 空闲检测
    async fn idle_checker(&self) {
        let mut check_interval = interval(Duration::from_secs(5));
        let mut idle_count = 0u64;
        let idle_threshold = self.config.idle_timeout_secs / 5;

        loop {
            check_interval.tick().await;

            if self.connections.has_connections() {
                // 有连接时重置状态
                idle_count = 0;
                // 如果之前设置了 shutdown，现在取消它
                if self.shutdown.load(Ordering::Relaxed) {
                    tracing::info!("🔄 New connection detected, canceling exit");
                    self.shutdown.store(false, Ordering::Relaxed);
                }
            } else {
                // 没有连接时累计空闲时间
                idle_count += 1;
                if idle_count >= idle_threshold && !self.shutdown.load(Ordering::Relaxed) {
                    tracing::info!(
                        "⏰ Idle timeout ({}s), preparing to exit...",
                        self.config.idle_timeout_secs
                    );
                    self.shutdown.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    /// 写入 PID 文件
    fn write_pid_file(&self) -> Result<()> {
        let pid = std::process::id();
        let pid_path = self.config.pid_path();
        fs::write(&pid_path, pid.to_string())?;
        #[cfg(unix)]
        fs::set_permissions(&pid_path, fs::Permissions::from_mode(0o600))?;
        tracing::debug!("📝 Writing PID file: {} (pid={})", pid_path.display(), pid);
        Ok(())
    }

    fn load_sync_config(data_dir: &Path) -> crate::sync::SyncConfig {
        let path = data_dir.join("memex/config.json");
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(sync) = json.get("sync") {
                    if let Ok(config) = serde_json::from_value(sync.clone()) {
                        return config;
                    }
                }
            }
        }
        crate::sync::SyncConfig::default()
    }

    fn load_filter_config(data_dir: &Path) -> crate::sync::FilterConfig {
        let path = data_dir.join("memex/config.json");
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(filter) = json.get("filter") {
                    if let Ok(config) = serde_json::from_value(filter.clone()) {
                        return config;
                    }
                }
            }
        }
        crate::sync::FilterConfig::default()
    }

    /// 清理资源
    fn cleanup(&self) {
        // 删除 socket 文件
        let socket_path = self.config.socket_path();
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }

        // 删除 PID 文件
        let pid_path = self.config.pid_path();
        if pid_path.exists() {
            let _ = fs::remove_file(&pid_path);
        }

        tracing::info!("🧹 Agent cleanup complete");
    }
}

/// 检查 Agent 是否正在运行
pub fn is_agent_running(config: &AgentConfig) -> bool {
    let pid_path = config.pid_path();
    if !pid_path.exists() {
        return false;
    }

    // 读取 PID
    let pid_str = match fs::read_to_string(&pid_path) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let pid: u32 = match pid_str.trim().parse() {
        Ok(p) => p,
        Err(_) => return false,
    };

    // 检查进程是否存在（跨平台）
    is_process_alive(pid)
}

/// 跨平台进程存活检测
fn is_process_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::new(),
    );
    sys.process(Pid::from_u32(pid)).is_some()
}

/// 清理残留的 Agent 状态
pub fn cleanup_stale_agent(config: &AgentConfig) -> Result<()> {
    let socket_path = config.socket_path();
    let pid_path = config.pid_path();

    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
        tracing::debug!("🧹 Removed stale socket: {:?}", socket_path);
    }

    if pid_path.exists() {
        fs::remove_file(&pid_path)?;
        tracing::debug!("🧹 Removed stale PID file: {:?}", pid_path);
    }

    Ok(())
}
