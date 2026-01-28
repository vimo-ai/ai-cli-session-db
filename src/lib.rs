//! ai-cli-session-db - 共享数据库库
//!
//! 为 ETerm、Memex、Vlaude 提供统一的 Claude session 数据库访问层。
//!
//! # 核心功能
//!
//! - **数据读写**: Project/Session/Message 的 CRUD 操作
//! - **全文搜索**: FTS5 支持
//! - **增量扫描**: 基于时间戳的增量更新
//! - **Agent 模式**: 唯一 Writer + 文件监听 + 事件推送
//!
//! # Feature Flags
//!
//! - `writer`: 写入能力
//! - `reader`: 只读能力
//! - `search`: 搜索能力 (依赖 `fts`)
//! - `fts`: FTS5 全文搜索
//! - `ffi`: C FFI 导出 (Swift 绑定用)
//! - `agent`: Agent 模式（唯一 Writer + 文件监听 + 事件推送）
//! - `client`: Agent Client（供组件使用）
//!
//! # 架构
//!
//! 所有写入操作统一通过 vimo-agent 处理，其他组件使用 AgentClient 进行通信。
//! 这消除了多组件同时写入 DB 的冲突问题。

pub mod config;
pub mod db;
pub mod error;
pub mod migrations;
pub mod protocol;
pub mod reader;
pub mod schema;
pub mod types;

#[cfg(feature = "writer")]
pub mod writer;

#[cfg(feature = "writer")]
pub mod collector;

#[cfg(feature = "search")]
pub mod search;

#[cfg(feature = "search")]
pub use search::escape_fts5_query;

#[cfg(feature = "ffi")]
pub mod ffi;

#[cfg(feature = "agent")]
pub mod agent;

#[cfg(feature = "client")]
pub mod client;

// Re-exports
pub use config::DbConfig;
pub use db::{IntegrityCheckResult, MessageInput, ProjectWithSource, SessionDB, SessionInput};
pub use error::{Error, Result};
pub use reader::{
    MessagesResult, Order, ProjectInfo, RawMessagesResult, SessionMetrics, SessionReader,
};
pub use types::*;

#[cfg(feature = "writer")]
pub use collector::{CollectResult, Collector};

// Protocol types (always available)
pub use protocol::{ApprovalStatus as AgentApprovalStatus, Event, EventType, Push, QueryType, Request, Response};

#[cfg(feature = "agent")]
pub use agent::{Agent, AgentConfig, cleanup_stale_agent, is_agent_running};

#[cfg(feature = "client")]
pub use client::{AgentClient, ClientConfig, connect_or_start_agent};

/// 编译时间戳（用于版本一致性检查）
///
/// Agent 和 Client 共享此常量。如果 CI 一起编译，时间戳相同。
/// 格式：Unix 时间戳（秒）
pub const BUILD_TIMESTAMP: u64 = {
    // const 中不能直接用 parse()，需要手动解析
    const BYTES: &[u8] = env!("BUILD_TIMESTAMP").as_bytes();
    const fn parse_u64(bytes: &[u8]) -> u64 {
        let mut result = 0u64;
        let mut i = 0;
        while i < bytes.len() {
            result = result * 10 + (bytes[i] - b'0') as u64;
            i += 1;
        }
        result
    }
    parse_u64(BYTES)
};

/// 完整版本号（语义版本 + 编译时间戳）
///
/// 格式：`{CARGO_PKG_VERSION}-{BUILD_TIMESTAMP}`
/// 例如：`0.1.0-1706400000`
pub const VERSION_FULL: &str = concat!(env!("CARGO_PKG_VERSION"), "-", env!("BUILD_TIMESTAMP"));

// Re-export ai-cli-session-collector 类型（统一入口）
// 下游项目应该从这里导入，避免直接依赖 ai-cli-session-collector
pub use ai_cli_session_collector::{
    // 工厂函数（适配器自注册机制）
    adapter_for_path,
    all_adapters,
    all_extensions,
    all_watch_configs,
    // 核心类型
    AdapterMeta,
    // 具体适配器
    ClaudeAdapter,
    CodexAdapter,
    ConversationAdapter,
    // 增量读取
    FileIdentity,
    IncrementalAdapter,
    IncrementalParseResult,
    JsonlIncrementalReader,
    ReadStats,
    ReaderState,
    // 领域类型
    IndexableMessage,
    IndexableSession,
    MessageType,
    OpenCodeAdapter,
    ParseResult,
    ParsedMessage,
    SessionMeta,
    Source,
    WatchConfig,
};
