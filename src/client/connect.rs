//! Agent Client 连接逻辑
//!
//! 实现连接或启动 Agent 的逻辑

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
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

    /// Socket 路径
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("agent.sock")
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
                tracing::warn!("自动部署 Agent 失败: {}", e);
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
                tracing::debug!("从 agent_source_dir 找到 Agent: {:?}", path);
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

        tracing::info!("✅ Agent 已部署: {:?} -> {:?}", source, install_path);
        Ok(())
    }
}

/// Agent Client
pub struct AgentClient {
    #[allow(dead_code)]
    config: ClientConfig,
    /// 写入端
    writer: OwnedWriteHalf,
    /// 推送事件接收通道
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

        // 读取响应（从 push_rx 中读取，因为响应也通过这个通道）
        // 注意：这里简化处理，实际应该区分响应和推送
        let response_line = self.push_rx.recv().await
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
    let socket_path = config.socket_path();

    // 1. 尝试连接（重试）
    for attempt in 1..=config.connect_retries {
        match UnixStream::connect(&socket_path).await {
            Ok(stream) => {
                tracing::debug!("连接 Agent 成功 (attempt={})", attempt);
                return finish_connect(config, stream).await;
            }
            Err(e) => {
                tracing::debug!("连接 Agent 失败 (attempt={}): {}", attempt, e);
                if attempt < config.connect_retries {
                    sleep(Duration::from_millis(config.retry_interval_ms)).await;
                }
            }
        }
    }

    // 2. 检查残留状态
    if is_agent_stuck(&config) {
        tracing::warn!("检测到 Agent 卡死，清理残留状态...");
        cleanup_stale(&config)?;
    }

    // 3. 启动 Agent
    start_agent(&config)?;

    // 4. 等待 Agent ready 并连接
    for attempt in 1..=10 {
        sleep(Duration::from_millis(200)).await;

        if let Ok(stream) = UnixStream::connect(&socket_path).await {
            tracing::info!("Agent 启动成功，已连接");
            return finish_connect(config, stream).await;
        }

        tracing::debug!("等待 Agent ready (attempt={})", attempt);
    }

    Err(anyhow::anyhow!("启动 Agent 超时"))
}

/// 完成连接（握手 + 启动读取任务）
async fn finish_connect(config: ClientConfig, stream: UnixStream) -> Result<AgentClient> {
    let (reader, mut writer) = stream.into_split();
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
            tracing::info!("握手成功: agent_version={}", agent_version);
        }
        crate::protocol::Response::Error { code, message } => {
            return Err(anyhow::anyhow!("握手失败: {} (code={})", message, code));
        }
        _ => {
            return Err(anyhow::anyhow!("握手响应异常"));
        }
    }

    // 创建推送通道
    let (push_tx, push_rx) = mpsc::channel(100);

    // 启动读取任务
    tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // 连接关闭
                Ok(_) => {
                    if push_tx.send(line.trim().to_string()).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(AgentClient {
        config,
        writer,
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

    let pid: i32 = match pid_str.trim().parse() {
        Ok(p) => p,
        Err(_) => return false,
    };

    // 检查进程是否存在
    let process_alive = unsafe { libc::kill(pid, 0) == 0 };

    // 如果进程存在但 socket 连接失败，认为是卡死
    process_alive && !config.socket_path().exists()
}

/// 清理残留状态
fn cleanup_stale(config: &ClientConfig) -> Result<()> {
    let socket_path = config.socket_path();
    let pid_path = config.pid_path();

    // 尝试杀死旧进程
    if pid_path.exists() {
        if let Ok(pid_str) = fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                tracing::debug!("杀死残留 Agent 进程: pid={}", pid);
            }
        }
    }

    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
    }

    if pid_path.exists() {
        fs::remove_file(&pid_path)?;
    }

    Ok(())
}

/// 启动 Agent
fn start_agent(config: &ClientConfig) -> Result<()> {
    // 尝试查找或下载 Agent
    let agent_path = match config.find_agent_binary() {
        Some(path) => path,
        None => {
            tracing::info!("本地未找到 vimo-agent，尝试从 GitHub Release 下载...");

            match download_agent_from_github(config) {
                Ok(path) => {
                    tracing::info!("✅ vimo-agent 下载成功");
                    path
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "找不到 Agent 二进制，且自动下载失败: {}\n\
                         \n\
                         尝试过的路径：\n\
                         - 配置覆盖: {:?}\n\
                         - 环境变量 VIMO_AGENT_PATH: {:?}\n\
                         - 默认路径: {:?}\n\
                         - Cargo target 目录\n\
                         - GitHub Release 自动下载\n\
                         \n\
                         请手动设置 VIMO_AGENT_PATH 环境变量，或运行 `cargo build -p ai-cli-session-db --features agent --bin vimo-agent`",
                        e,
                        config.agent_binary_override,
                        std::env::var("VIMO_AGENT_PATH").ok(),
                        config.default_agent_binary_path()
                    ));
                }
            }
        }
    };

    tracing::info!("启动 Agent: {:?}", agent_path);

    Command::new(&agent_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("启动 Agent 失败")?;

    Ok(())
}

/// 从 GitHub Release 下载 vimo-agent
fn download_agent_from_github(config: &ClientConfig) -> Result<PathBuf> {
    // 检测平台（确保当前平台受支持）
    let _platform = detect_platform()?;

    // 构建下载 URL
    // TODO: 目前写死版本号，后续改成动态获取或从 memex 统一下载
    let url = "https://github.com/vimo-ai/ai-cli-session-db/releases/download/v0.0.1-beta.5/vimo-agent";

    tracing::info!("下载 URL: {}", url);

    // 下载文件
    let url_owned = url.to_string();
    let response = std::thread::spawn(move || {
        // 使用 std 的网络库进行简单的 HTTP GET
        download_file_simple(&url_owned)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("下载线程 panic"))??;

    // 确保目标目录存在
    let install_dir = config.data_dir.join("bin");
    fs::create_dir_all(&install_dir)
        .context("创建 ~/.vimo/bin 目录失败")?;

    // 写入文件
    let install_path = install_dir.join("vimo-agent");
    fs::write(&install_path, response)
        .context("写入 vimo-agent 文件失败")?;

    // 设置可执行权限 (Unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&install_path, fs::Permissions::from_mode(0o755))
            .context("设置可执行权限失败")?;
    }

    tracing::info!("vimo-agent 已下载到: {:?}", install_path);

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
        _ => {
            return Err(anyhow::anyhow!(
                "不支持的平台: os={}, arch={}",
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
        .ok_or_else(|| anyhow::anyhow!("仅支持 HTTPS"))?;

    let (host, path) = url_parsed.split_once('/')
        .ok_or_else(|| anyhow::anyhow!("URL 格式错误"))?;

    // 使用 TLS 连接
    let stream = std::net::TcpStream::connect(format!("{}:443", host))
        .context("TCP 连接失败")?;

    let connector = native_tls::TlsConnector::new()
        .context("创建 TLS connector 失败")?;

    let mut stream = connector.connect(host, stream)
        .context("TLS 握手失败")?;

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
        .context("发送 HTTP 请求失败")?;

    // 读取响应
    let mut response = Vec::new();
    stream.read_to_end(&mut response)
        .context("读取 HTTP 响应失败")?;

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
                        .ok_or_else(|| anyhow::anyhow!("无法解析 Location 头"))?;
                    tracing::info!("跟随重定向: {}", redirect_url);
                    return download_file_simple(redirect_url);
                }
            }
            return Err(anyhow::anyhow!(
                "下载失败: 收到重定向但找不到 Location 头"
            ));
        }

        return Err(anyhow::anyhow!(
            "下载失败: HTTP 状态码异常\n响应头: {}",
            response_str.lines().take(10).collect::<Vec<_>>().join("\n")
        ));
    }

    // 分离 header 和 body
    let body_start = response.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("找不到 HTTP body"))?
        + 4;

    Ok(response[body_start..].to_vec())
}
