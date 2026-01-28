//! Agent Client 连接逻辑
//!
//! 跨平台实现连接或启动 Agent 的逻辑
//! - Unix: Unix Domain Socket
//! - Windows: Named Pipe

use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    Name,
};
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::sync::mpsc;
use tokio::time::sleep;

/// Client 配置
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// 数据目录（默认 ~/.vimo）
    pub data_dir: PathBuf,
    /// 组件名称
    pub component: String,
    /// 组件版本
    pub version: String,
    /// 连接重试次数
    pub connect_retries: u32,
    /// 重试间隔（毫秒）
    pub retry_interval_ms: u64,
    /// Agent 二进制路径覆盖（优先于默认路径）
    pub agent_binary_override: Option<PathBuf>,
    /// Agent 源目录（用于首次部署，如 plugin bundle 的 Lib 目录）
    pub agent_source_dir: Option<PathBuf>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        let data_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".vimo");

        Self {
            data_dir,
            component: "unknown".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            connect_retries: 3,
            retry_interval_ms: 500,
            agent_binary_override: None,
            agent_source_dir: None,
        }
    }
}

impl ClientConfig {
    /// 创建新的配置
    pub fn new(component: &str) -> Self {
        Self {
            component: component.to_string(),
            ..Default::default()
        }
    }

    /// 设置 Agent 二进制路径
    pub fn with_agent_binary(mut self, path: PathBuf) -> Self {
        self.agent_binary_override = Some(path);
        self
    }

    /// 设置 Agent 源目录（用于首次部署）
    pub fn with_agent_source_dir(mut self, path: PathBuf) -> Self {
        self.agent_source_dir = Some(path);
        self
    }

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
        "vimo-agent"
            .to_ns_name::<GenericNamespaced>()
            .unwrap()
            .to_owned()
    }

    /// PID 文件路径
    pub fn pid_path(&self) -> PathBuf {
        self.data_dir.join("agent.pid")
    }

    /// Agent 二进制默认路径
    pub fn default_agent_binary_path(&self) -> PathBuf {
        self.data_dir.join("bin").join("vimo-agent")
    }

    /// 查找 Agent 二进制（如果需要，自动部署到 ~/.vimo/bin/）
    ///
    /// 查找顺序：
    /// 1. agent_binary_override（配置覆盖）
    /// 2. VIMO_AGENT_PATH 环境变量
    /// 3. ~/.vimo/bin/vimo-agent（默认安装路径）
    /// 4. 源路径（Cargo target / App bundle）→ 自动部署到 ~/.vimo/bin/
    pub fn find_agent_binary(&self) -> Option<PathBuf> {
        // 1. 配置覆盖
        if let Some(ref path) = self.agent_binary_override {
            if path.exists() {
                return Some(path.clone());
            }
        }

        // 2. 环境变量
        if let Ok(path) = std::env::var("VIMO_AGENT_PATH") {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }

        // 3. 默认安装路径
        let default_path = self.default_agent_binary_path();
        if default_path.exists() {
            return Some(default_path);
        }

        // 4. 查找源路径并自动部署
        if let Some(source_path) = self.find_agent_source() {
            if let Err(e) = self.deploy_agent(&source_path) {
                tracing::warn!("Failed to auto-deploy Agent: {}", e);
                // 部署失败时直接使用源路径
                return Some(source_path);
            }
            return Some(default_path);
        }

        None
    }

    /// 查找 Agent 源二进制（用于自动部署）
    fn find_agent_source(&self) -> Option<PathBuf> {
        // 1. 优先从配置的 agent_source_dir 查找（plugin bundle 等场景）
        if let Some(ref source_dir) = self.agent_source_dir {
            let path = source_dir.join("vimo-agent");
            if path.exists() {
                tracing::debug!("Found Agent in agent_source_dir: {:?}", path);
                return Some(path);
            }
        }

        // 2. Cargo target 目录（开发阶段）
        for profile in ["release", "debug"] {
            // 相对于当前目录查找
            let cargo_path = PathBuf::from(format!("target/{}/vimo-agent", profile));
            if cargo_path.exists() {
                return Some(cargo_path);
            }

            // 相对于 workspace root 查找
            if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
                let workspace_path = PathBuf::from(&manifest_dir)
                    .parent()
                    .map(|p| p.join(format!("target/{}/vimo-agent", profile)));
                if let Some(path) = workspace_path {
                    if path.exists() {
                        return Some(path);
                    }
                }
            }
        }

        // 3. App bundle（生产环境）
        // ETerm.app/Contents/MacOS/vimo-agent
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(macos_dir) = exe_path.parent() {
                let bundle_agent = macos_dir.join("vimo-agent");
                if bundle_agent.exists() {
                    return Some(bundle_agent);
                }
            }
        }

        None
    }

    /// 部署 Agent 到 ~/.vimo/bin/
    fn deploy_agent(&self, source: &PathBuf) -> std::io::Result<()> {
        let install_dir = self.data_dir.join("bin");
        let install_path = install_dir.join("vimo-agent");

        // 创建目录
        fs::create_dir_all(&install_dir)?;

        // 复制二进制
        fs::copy(source, &install_path)?;

        // 设置可执行权限
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&install_path, fs::Permissions::from_mode(0o755))?;
        }

        tracing::info!("✅ Agent deployed: {:?} -> {:?}", source, install_path);
        Ok(())
    }
}

/// Agent Client
pub struct AgentClient {
    #[allow(dead_code)]
    config: ClientConfig,
    /// 写入端（跨平台 IPC stream）
    writer: WriteHalf<Stream>,
    /// Response 接收通道（用于 request/response 模式）
    response_rx: mpsc::Receiver<String>,
    /// Push 事件接收通道（用于订阅推送）
    push_rx: mpsc::Receiver<String>,
}

impl AgentClient {
    /// 发送请求并等待响应
    pub async fn request(&mut self, request: &crate::protocol::Request) -> Result<crate::protocol::Response> {
        // 序列化请求
        let request_json = serde_json::to_string(request)?;
        let request_line = format!("{}\n", request_json);

        // 发送请求
        self.writer.write_all(request_line.as_bytes()).await?;

        // 从 response_rx 读取响应（与 push_rx 分离，避免竞争）
        let response_line = self.response_rx.recv().await
            .ok_or_else(|| anyhow::anyhow!("Connection closed"))?;

        // 解析响应
        let response: crate::protocol::Response = serde_json::from_str(&response_line)?;
        Ok(response)
    }

    /// 订阅事件
    pub async fn subscribe(&mut self, events: Vec<crate::protocol::EventType>) -> Result<()> {
        let request = crate::protocol::Request::Subscribe { events };
        let response = self.request(&request).await?;

        match response {
            crate::protocol::Response::Ok => Ok(()),
            crate::protocol::Response::Error { code, message } => {
                Err(anyhow::anyhow!("Subscribe failed: {} (code={})", message, code))
            }
            _ => Err(anyhow::anyhow!("Unexpected response")),
        }
    }

    /// 通知文件变化
    pub async fn notify_file_change(&mut self, path: PathBuf) -> Result<()> {
        let request = crate::protocol::Request::NotifyFileChange { path };
        let response = self.request(&request).await?;

        match response {
            crate::protocol::Response::Ok => Ok(()),
            crate::protocol::Response::Error { code, message } => {
                Err(anyhow::anyhow!("NotifyFileChange failed: {} (code={})", message, code))
            }
            _ => Err(anyhow::anyhow!("Unexpected response")),
        }
    }

    /// 写入 Approve 结果
    pub async fn write_approve_result(
        &mut self,
        tool_call_id: String,
        status: crate::protocol::ApprovalStatus,
        resolved_at: i64,
    ) -> Result<()> {
        let request = crate::protocol::Request::WriteApproveResult {
            tool_call_id,
            status,
            resolved_at,
        };
        let response = self.request(&request).await?;

        match response {
            crate::protocol::Response::Ok => Ok(()),
            crate::protocol::Response::Error { code, message } => {
                Err(anyhow::anyhow!("WriteApproveResult failed: {} (code={})", message, code))
            }
            _ => Err(anyhow::anyhow!("Unexpected response")),
        }
    }

    /// 接收推送事件
    pub async fn recv_push(&mut self) -> Option<crate::protocol::Push> {
        let line = self.push_rx.recv().await?;
        serde_json::from_str(&line).ok()
    }

    /// 获取推送接收器（用于 select!）
    pub fn push_receiver(&mut self) -> &mut mpsc::Receiver<String> {
        &mut self.push_rx
    }
}

/// 连接或启动 Agent
///
/// 连接流程：
/// 1. 尝试连接 socket（重试 3 次，间隔 500ms）
/// 2. 连接失败 → 检查残留状态
/// 3. 清理残留 → 启动 Agent
/// 4. 等待 Agent ready → 连接
pub async fn connect_or_start_agent(config: ClientConfig) -> Result<AgentClient> {
    // 1. 尝试连接（重试）
    for attempt in 1..=config.connect_retries {
        match Stream::connect(config.socket_name()).await {
            Ok(stream) => {
                tracing::debug!("Connected to Agent successfully (attempt={})", attempt);
                return finish_connect(config, stream).await;
            }
            Err(e) => {
                tracing::debug!("Failed to connect to Agent (attempt={}): {}", attempt, e);
                if attempt < config.connect_retries {
                    sleep(Duration::from_millis(config.retry_interval_ms)).await;
                }
            }
        }
    }

    // 2. 检查残留状态
    if is_agent_stuck(&config) {
        tracing::warn!("Agent appears stuck, cleaning up stale state...");
        cleanup_stale(&config)?;
    }

    // 3. 启动 Agent
    start_agent(&config)?;

    // 4. 等待 Agent ready 并连接
    for attempt in 1..=10 {
        sleep(Duration::from_millis(200)).await;

        if let Ok(stream) = Stream::connect(config.socket_name()).await {
            tracing::info!("Agent started successfully, connected");
            return finish_connect(config, stream).await;
        }

        tracing::debug!("Waiting for Agent to be ready (attempt={})", attempt);
    }

    Err(anyhow::anyhow!("Timeout starting Agent"))
}

/// 完成连接（握手 + 启动读取任务）
async fn finish_connect(config: ClientConfig, stream: Stream) -> Result<AgentClient> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    // 发送握手
    let handshake = crate::protocol::Request::Handshake {
        component: config.component.clone(),
        version: config.version.clone(),
    };
    let handshake_json = serde_json::to_string(&handshake)?;
    writer.write_all(format!("{}\n", handshake_json).as_bytes()).await?;

    // 读取握手响应
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response: crate::protocol::Response = serde_json::from_str(&line)?;
    match response {
        crate::protocol::Response::HandshakeOk { agent_version } => {
            tracing::info!("Handshake successful: agent_version={}", agent_version);
        }
        crate::protocol::Response::Error { code, message } => {
            return Err(anyhow::anyhow!("Handshake failed: {} (code={})", message, code));
        }
        _ => {
            return Err(anyhow::anyhow!("Unexpected handshake response"));
        }
    }

    // 创建分离的通道：Response 和 Push 各自独立
    let (response_tx, response_rx) = mpsc::channel(100);
    let (push_tx, push_rx) = mpsc::channel(100);

    // 启动读取任务，根据消息类型路由到正确的通道
    tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // 连接关闭
                Ok(_) => {
                    let trimmed = line.trim().to_string();

                    // 尝试解析为 Response
                    if let Ok(_) = serde_json::from_str::<crate::protocol::Response>(&trimmed) {
                        // 是 Response，发送到 response 通道
                        if response_tx.send(trimmed).await.is_err() {
                            break;
                        }
                    } else {
                        // 不是 Response，当作 Push 处理
                        if push_tx.send(trimmed).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(AgentClient {
        config,
        writer,
        response_rx,
        push_rx,
    })
}

/// 检查 Agent 是否卡死
fn is_agent_stuck(config: &ClientConfig) -> bool {
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
    let process_alive = is_process_alive(pid);

    // 如果进程存在但 socket 连接失败，认为是卡死
    // 注意：Windows 上 Named Pipe 没有文件实体，这个检查只在 Unix 上有意义
    #[cfg(unix)]
    let socket_exists = config.socket_path().exists();
    #[cfg(windows)]
    let socket_exists = true; // Windows 上假设存在

    process_alive && !socket_exists
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

/// 清理残留状态
fn cleanup_stale(config: &ClientConfig) -> Result<()> {
    let pid_path = config.pid_path();

    // 尝试杀死旧进程（跨平台）
    if pid_path.exists() {
        if let Ok(pid_str) = fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                kill_process(pid);
                tracing::debug!("Killed stale Agent process: pid={}", pid);
            }
        }
    }

    // 清理 socket 文件 (Unix only)
    #[cfg(unix)]
    {
        let socket_path = config.socket_path();
        if socket_path.exists() {
            fs::remove_file(&socket_path)?;
        }
    }

    if pid_path.exists() {
        fs::remove_file(&pid_path)?;
    }

    Ok(())
}

/// 跨平台杀进程
#[cfg(unix)]
fn kill_process(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    // Windows: 使用 taskkill 命令
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// 启动 Agent
fn start_agent(config: &ClientConfig) -> Result<()> {
    // 尝试查找或下载 Agent
    let agent_path = match config.find_agent_binary() {
        Some(path) => path,
        None => {
            tracing::info!("vimo-agent not found locally, attempting to download from GitHub Release...");

            match download_agent_from_github(config) {
                Ok(path) => {
                    tracing::info!("✅ vimo-agent downloaded successfully");
                    path
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Agent binary not found and auto-download failed: {}\n\
                         \n\
                         Tried paths:\n\
                         - Config override: {:?}\n\
                         - Environment variable VIMO_AGENT_PATH: {:?}\n\
                         - Default path: {:?}\n\
                         - Cargo target directory\n\
                         - GitHub Release auto-download\n\
                         \n\
                         Please set VIMO_AGENT_PATH environment variable or run `cargo build -p ai-cli-session-db --features agent --bin vimo-agent`",
                        e,
                        config.agent_binary_override,
                        std::env::var("VIMO_AGENT_PATH").ok(),
                        config.default_agent_binary_path()
                    ));
                }
            }
        }
    };

    tracing::info!("Starting Agent: {:?}", agent_path);

    // 打开日志文件（追加模式）
    let log_path = config.data_dir.join("agent.log");
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("Failed to open agent log file")?;

    Command::new(&agent_path)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .context("Failed to start Agent")?;

    Ok(())
}

/// 从 GitHub Release 下载 vimo-agent
fn download_agent_from_github(config: &ClientConfig) -> Result<PathBuf> {
    // 检测平台（确保当前平台受支持）
    let _platform = detect_platform()?;

    // 构建下载 URL
    // TODO: 目前写死版本号，后续改成动态获取或从 memex 统一下载
    let url = "https://github.com/vimo-ai/ai-cli-session-db/releases/download/v0.0.1-beta.5/vimo-agent";

    tracing::info!("Download URL: {}", url);

    // 下载文件
    let url_owned = url.to_string();
    let response = std::thread::spawn(move || {
        // 使用 std 的网络库进行简单的 HTTP GET
        download_file_simple(&url_owned)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Download thread panicked"))??;

    // 确保目标目录存在
    let install_dir = config.data_dir.join("bin");
    fs::create_dir_all(&install_dir)
        .context("Failed to create ~/.vimo/bin directory")?;

    // 写入文件
    let install_path = install_dir.join("vimo-agent");
    fs::write(&install_path, response)
        .context("Failed to write vimo-agent file")?;

    // 设置可执行权限 (Unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&install_path, fs::Permissions::from_mode(0o755))
            .context("Failed to set executable permission")?;
    }

    tracing::info!("vimo-agent downloaded to: {:?}", install_path);

    Ok(install_path)
}

/// 检测当前平台
fn detect_platform() -> Result<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let platform = match (os, arch) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "windows-x64",
        _ => {
            return Err(anyhow::anyhow!(
                "Unsupported platform: os={}, arch={}",
                os, arch
            ));
        }
    };

    Ok(platform.to_string())
}

/// 简单的 HTTP GET 下载（使用 std 网络库）
fn download_file_simple(url: &str) -> Result<Vec<u8>> {
    use std::io::Read;

    // 解析 URL
    let url_parsed = url.strip_prefix("https://")
        .ok_or_else(|| anyhow::anyhow!("Only HTTPS is supported"))?;

    let (host, path) = url_parsed.split_once('/')
        .ok_or_else(|| anyhow::anyhow!("Invalid URL format"))?;

    // 使用 TLS 连接
    let stream = std::net::TcpStream::connect(format!("{}:443", host))
        .context("TCP connection failed")?;

    let connector = native_tls::TlsConnector::new()
        .context("Failed to create TLS connector")?;

    let mut stream = connector.connect(host, stream)
        .context("TLS handshake failed")?;

    // 发送 HTTP 请求
    let request = format!(
        "GET /{} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: vimo-agent-downloader/1.0\r\n\
         Connection: close\r\n\
         \r\n",
        path, host
    );

    use std::io::Write;
    stream.write_all(request.as_bytes())
        .context("Failed to send HTTP request")?;

    // 读取响应
    let mut response = Vec::new();
    stream.read_to_end(&mut response)
        .context("Failed to read HTTP response")?;

    // 解析 HTTP 响应（简单处理）
    let response_str = String::from_utf8_lossy(&response);

    // 检查状态码
    if !response_str.starts_with("HTTP/1.1 200")
        && !response_str.starts_with("HTTP/1.0 200")
        && !response_str.starts_with("HTTP/2 200") {
        // 处理重定向 - 跟随 Location 头
        if response_str.starts_with("HTTP/1.1 302")
            || response_str.starts_with("HTTP/1.1 301")
            || response_str.starts_with("HTTP/1.1 307")
            || response_str.starts_with("HTTP/1.1 308") {
            // 提取 Location 头
            for line in response_str.lines() {
                if line.to_lowercase().starts_with("location:") {
                    let redirect_url = line.split_once(':')
                        .map(|(_, v)| v.trim())
                        .ok_or_else(|| anyhow::anyhow!("Cannot parse Location header"))?;
                    tracing::info!("Following redirect: {}", redirect_url);
                    return download_file_simple(redirect_url);
                }
            }
            return Err(anyhow::anyhow!(
                "Download failed: Received redirect but Location header not found"
            ));
        }

        return Err(anyhow::anyhow!(
            "Download failed: Unexpected HTTP status\nResponse headers: {}",
            response_str.lines().take(10).collect::<Vec<_>>().join("\n")
        ));
    }

    // 分离 header 和 body
    let body_start = response.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("HTTP body not found"))?
        + 4;

    Ok(response[body_start..].to_vec())
}
