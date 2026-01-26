//! Agent 服务器
//!
//! Unix Socket 服务，处理客户端连接和请求

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::time::interval;

use super::broadcaster::Broadcaster;
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
    /// Socket 路径
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("agent.sock")
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
    broadcaster: Arc<Broadcaster>,
    watcher: Arc<FileWatcher>,
    handler: Arc<Handler>,
    shutdown: Arc<AtomicBool>,
}

impl Agent {
    /// 创建 Agent
    pub fn new(config: AgentConfig) -> Result<Self> {
        // 确保数据目录存在
        fs::create_dir_all(&config.data_dir)
            .context("创建数据目录失败")?;
        fs::create_dir_all(config.data_dir.join("db"))
            .context("创建数据库目录失败")?;

        // 连接数据库
        let db_config = DbConfig::local(config.db_path().to_str().unwrap());
        let db = Arc::new(SessionDB::connect(db_config)?);

        // 创建广播器
        let broadcaster = Broadcaster::new();

        // 创建文件监听器
        let watcher = FileWatcher::new(db.clone(), broadcaster.clone());

        // 创建处理器
        let handler = Arc::new(Handler::new(db.clone(), broadcaster.clone(), watcher.clone()));

        Ok(Self {
            config,
            db,
            broadcaster,
            watcher,
            handler,
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 运行 Agent
    pub async fn run(self: Arc<Self>) -> Result<()> {
        // 写入 PID 文件
        self.write_pid_file()?;

        // 清理旧的 socket 文件
        let socket_path = self.config.socket_path();
        if socket_path.exists() {
            fs::remove_file(&socket_path)?;
        }

        // 创建 Unix Socket 监听器
        let listener = UnixListener::bind(&socket_path)
            .context("绑定 socket 失败")?;

        // 设置 socket 权限为 0600
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

        tracing::info!("🚀 Agent 启动: {:?}", socket_path);

        // 启动时执行全量扫描（mtime 剪枝会跳过未变化的文件）
        {
            let db = self.db.clone();
            tokio::task::spawn_blocking(move || {
                let collector = crate::Collector::new(&*db);
                match collector.collect_all() {
                    Ok(result) => {
                        if result.messages_inserted > 0 {
                            tracing::info!(
                                "📊 启动扫描完成: {} 个会话, {} 条新消息",
                                result.sessions_scanned,
                                result.messages_inserted
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!("启动扫描失败: {}", e);
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
            if self.shutdown.load(Ordering::Relaxed) && !self.broadcaster.has_connections() {
                break;
            }

            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let agent = self.clone();
                            tokio::spawn(async move {
                                if let Err(e) = agent.handle_connection(stream).await {
                                    tracing::error!("处理连接失败: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("接受连接失败: {}", e);
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("收到中断信号，准备退出...");
                    break;
                }
            }
        }

        self.cleanup();
        Ok(())
    }

    /// 处理单个连接
    async fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // 创建消息发送通道
        let (tx, mut rx) = mpsc::channel::<String>(100);

        // 注册连接
        let conn_id = self.broadcaster.register(tx);
        tracing::debug!("📥 新连接: conn_id={}", conn_id);

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
                            tracing::warn!("解析请求失败: {}", e);
                            let response = Response::Error {
                                code: 400,
                                message: format!("Invalid JSON: {}", e),
                            };
                            let resp_json = serde_json::to_string(&response)?;
                            self.broadcaster.try_send_to(conn_id, format!("{}\n", resp_json));
                            continue;
                        }
                    };

                    // 处理请求
                    let response = self.handler.handle(conn_id, request).await;
                    let resp_json = serde_json::to_string(&response)?;

                    // 发送响应
                    if !self.broadcaster.send_to(conn_id, format!("{}\n", resp_json)).await {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("读取失败: {}", e);
                    break;
                }
            }
        }

        // 清理
        self.broadcaster.unregister(conn_id);
        write_handle.abort();
        tracing::debug!("📤 连接关闭: conn_id={}", conn_id);

        Ok(())
    }

    /// 空闲检测
    async fn idle_checker(&self) {
        let mut check_interval = interval(Duration::from_secs(5));
        let mut idle_count = 0u64;
        let idle_threshold = self.config.idle_timeout_secs / 5;

        loop {
            check_interval.tick().await;

            if self.broadcaster.has_connections() {
                // 有连接时重置状态
                idle_count = 0;
                // 如果之前设置了 shutdown，现在取消它
                if self.shutdown.load(Ordering::Relaxed) {
                    tracing::info!("🔄 有新连接，取消退出");
                    self.shutdown.store(false, Ordering::Relaxed);
                }
            } else {
                // 没有连接时累计空闲时间
                idle_count += 1;
                if idle_count >= idle_threshold && !self.shutdown.load(Ordering::Relaxed) {
                    tracing::info!(
                        "⏰ 空闲超时 ({}s)，准备退出...",
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
        fs::set_permissions(&pid_path, fs::Permissions::from_mode(0o600))?;
        tracing::debug!("📝 写入 PID 文件: {} (pid={})", pid_path.display(), pid);
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

        tracing::info!("🧹 Agent 清理完成");
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

    let pid: i32 = match pid_str.trim().parse() {
        Ok(p) => p,
        Err(_) => return false,
    };

    // 检查进程是否存在
    unsafe {
        libc::kill(pid, 0) == 0
    }
}

/// 清理残留的 Agent 状态
pub fn cleanup_stale_agent(config: &AgentConfig) -> Result<()> {
    let socket_path = config.socket_path();
    let pid_path = config.pid_path();

    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
        tracing::debug!("🧹 删除残留 socket: {:?}", socket_path);
    }

    if pid_path.exists() {
        fs::remove_file(&pid_path)?;
        tracing::debug!("🧹 删除残留 PID 文件: {:?}", pid_path);
    }

    Ok(())
}
