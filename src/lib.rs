//! claude-session-db - 共享数据库库
//!
//! 为 ETerm、Memex、Vlaude 提供统一的 Claude session 数据库访问层。
//!
//! # 核心功能
//!
//! - **Writer 协调**: 多组件共存时，只有一个负责写入
//! - **数据读写**: Project/Session/Message 的 CRUD 操作
//! - **全文搜索**: FTS5 支持
//! - **增量扫描**: 基于时间戳的增量更新
//!
//! # Feature Flags
//!
//! - `writer`: 写入能力
//! - `reader`: 只读能力
//! - `search`: 搜索能力 (依赖 `fts`)
//! - `fts`: FTS5 全文搜索
//! - `coordination`: Writer 协调逻辑
//! - `ffi`: C FFI 导出 (Swift 绑定用)
//!
//! # 使用示例
//!
//! ```ignore
//! use claude_session_db::{SessionDB, DbConfig, WriterType};
//!
//! // 连接数据库
//! let config = DbConfig::local("~/.memex/session.db");
//! let db = SessionDB::connect(config)?;
//!
//! // 注册为 Writer
//! let role = db.register_writer(WriterType::MemexDaemon)?;
//!
//! // 根据角色执行不同逻辑
//! match role {
//!     Role::Writer => {
//!         // 扫描 JSONL，写入数据库
//!         db.heartbeat()?;  // 定期心跳
//!     }
//!     Role::Reader => {
//!         // 只读数据库
//!     }
//! }
//! ```

pub mod config;
pub mod db;
pub mod error;
pub mod migrations;
pub mod reader;
pub mod schema;
pub mod types;

#[cfg(feature = "coordination")]
pub mod coordination;

#[cfg(feature = "writer")]
pub mod writer;

#[cfg(feature = "search")]
pub mod search;

#[cfg(feature = "ffi")]
pub mod ffi;

// Re-exports
pub use config::DbConfig;
pub use db::{MessageInput, ProjectWithSource, SessionDB, SessionInput};
pub use error::{Error, Result};
pub use reader::{MessagesResult, Order, ProjectInfo, RawMessagesResult, SessionMetrics, SessionReader};
pub use types::*;

#[cfg(feature = "coordination")]
pub use coordination::{Role, WriterHealth, WriterType};

// Re-export ai-cli-session-collector 类型（统一入口）
// 下游项目应该从这里导入，避免直接依赖 ai-cli-session-collector
pub use ai_cli_session_collector::{
    ClaudeAdapter,
    CodexAdapter,
    ConversationAdapter,
    IndexableMessage,
    IndexableSession,
    MessageType,
    ParseResult,
    ParsedMessage,
    SessionMeta,
    Source,
};
