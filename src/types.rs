//! 数据类型定义

use ai_cli_session_collector::MessageType;
use serde::{Deserialize, Serialize};

/// 项目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 会话
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: i64,
    pub session_id: String,
    pub project_id: i64,
    pub message_count: i64,
    pub last_message_at: Option<i64>,
    // 会话元数据 (来自 ai-cli-session-collector::SessionMeta)
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub channel: Option<String>,
    // 增量检测字段
    pub file_mtime: Option<i64>,
    pub file_size: Option<i64>,
    // 额外元信息
    pub encoded_dir_name: Option<String>,
    pub meta: Option<String>,
    // 时间戳
    pub created_at: i64,
    pub updated_at: i64,
}

/// 会话详情 (带项目名)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub id: i64,
    pub session_id: String,
    pub project_id: i64,
    pub project_name: String,
    pub message_count: i64,
    pub first_message_at: Option<i64>,
    pub last_message_at: Option<i64>,
}

/// 消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub session_id: String,
    pub uuid: String,
    pub r#type: MessageType,
    pub content_text: String,  // 纯对话文本（用于向量化）
    pub content_full: String,  // 完整格式化内容（用于 FTS）
    pub timestamp: i64,
    pub sequence: i64,
    pub source: Option<String>,
    pub channel: Option<String>,
    pub model: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_args: Option<String>,
    pub raw: Option<String>,
    pub vector_indexed: bool,  // 是否已向量索引
}

// MessageType 直接使用 ai_cli_session_collector::MessageType，在 lib.rs 中 re-export

/// 搜索结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub message_id: i64,
    pub session_id: String,
    pub project_id: i64,
    pub project_name: String,
    pub r#type: String,
    pub content_full: String,  // FTS 搜索使用 content_full
    pub snippet: String,
    pub score: f64,
    pub timestamp: Option<i64>,
}

/// 统计信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub project_count: i64,
    pub session_count: i64,
    pub message_count: i64,
}
