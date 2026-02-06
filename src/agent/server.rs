//! Agent 服务器
//!
//! 跨平台 IPC 服务，处理客户端连接和请求
//! - Unix: Unix Domain Socket
//! - Windows: Named Pipe

use std::fs;
use std::path::PathBuf;
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
    #[allow(dead_code)] // 预留，未来扩展功能使用
    db: Arc<SessionDB>,
    connections: Arc<ConnectionManager>,
    watcher: Arc<FileWatcher>,
    handler: Arc<Handler>,
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

        // 创建处理器
        let handler = Arc::new(Handler::new(db.clone(), connections.clone(), watcher.clone()));

        Ok(Self {
            config,
            db,
            connections,
            watcher,
            handler,
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

        // 接受连接
        loop {
            // 只有当 shutdown 信号发出 且 没有活跃连接 时才退出
            // 这样新连接进来后可以取消退出
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
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Received interrupt signal, preparing to exit...");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // 定期回到循环顶部检查 shutdown 条件
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
