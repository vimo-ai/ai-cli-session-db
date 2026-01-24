//! Agent Client 模块
//!
//! 提供连接 Agent 的客户端功能

mod connect;

#[cfg(feature = "ffi")]
pub mod ffi;

pub use connect::{AgentClient, ClientConfig, connect_or_start_agent};
