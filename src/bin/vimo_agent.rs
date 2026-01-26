//! vimo-agent - AI CLI Session 数据库 Agent
//!
//! 负责：
//! - 唯一写入者
//! - 文件监听 + Collection
//! - 事件推送
//! - 接收业务写入请求

use std::sync::Arc;

use ai_cli_session_db::agent::{Agent, AgentConfig, cleanup_stale_agent, is_agent_running};
use anyhow::Result;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("ai_cli_session_db=debug".parse()?))
        .init();

    tracing::info!("🚀 vimo-agent v{}", env!("CARGO_PKG_VERSION"));

    // 解析配置
    let config = AgentConfig::default();

    // 检查是否已有 Agent 运行
    if is_agent_running(&config) {
        tracing::error!("❌ Agent is already running, exiting");
        std::process::exit(1);
    }

    // 清理残留状态
    if let Err(e) = cleanup_stale_agent(&config) {
        tracing::warn!("Failed to cleanup stale state: {}", e);
    }

    // 创建并运行 Agent
    let agent = Arc::new(Agent::new(config)?);
    agent.run().await?;

    tracing::info!("👋 vimo-agent exiting");
    Ok(())
}
