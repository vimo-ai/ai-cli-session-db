//! Agent 模块 - 唯一 Writer + 文件监听 + 事件推送
//!
//! Agent 是数据库的唯一写入者，负责：
//! - 监听 AI CLI 会话文件变化
//! - 执行 Collection（解析 JSONL → 写入 DB）
//! - 推送事件给订阅者
//! - 接收业务写入请求（index 结果、approve 结果）

mod broadcaster;
mod handler;
mod server;
mod watcher;

// Re-export protocol types from crate root
pub use crate::protocol::{Event, EventType, Push, Request, Response};
pub use server::{Agent, AgentConfig, cleanup_stale_agent, is_agent_running};
