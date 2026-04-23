//! vimo-agent - AI CLI Session 数据库 Agent
//!
//! 负责：
//! - 唯一写入者
//! - 文件监听 + Collection
//! - 事件推送
//! - 接收业务写入请求

use std::sync::Arc;

use ai_cli_session_db::agent::{Agent, AgentConfig, cleanup_stale_agent, is_agent_running};
use ai_cli_session_db::repair;
use anyhow::Result;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env().add_directive("ai_cli_session_db=debug".parse()?))
        .init();

    if std::env::args().any(|a| a == "--repair") {
        return repair::run_repair();
    }

    tracing::info!("🚀 vimo-agent v{}", env!("CARGO_PKG_VERSION"));

    let config = AgentConfig::default();

    if is_agent_running(&config) {
        tracing::error!("❌ Agent is already running, exiting");
        std::process::exit(1);
    }

    if let Err(e) = cleanup_stale_agent(&config) {
        tracing::warn!("Failed to cleanup stale state: {}", e);
    }

    let agent = Arc::new(Agent::new(config)?);
    agent.run().await?;

    tracing::info!("👋 vimo-agent exiting");
    Ok(())
}
