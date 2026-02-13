//! 数据类型定义

use ai_cli_session_collector::MessageType;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// 审批状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Timeout,
}

impl FromStr for ApprovalStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "pending" => Ok(ApprovalStatus::Pending),
            "approved" => Ok(ApprovalStatus::Approved),
            "rejected" => Ok(ApprovalStatus::Rejected),
            "timeout" => Ok(ApprovalStatus::Timeout),
            _ => Err(format!("Invalid approval status: {}", s)),
        }
    }
}

impl fmt::Display for ApprovalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApprovalStatus::Pending => write!(f, "pending"),
            ApprovalStatus::Approved => write!(f, "approved"),
            ApprovalStatus::Rejected => write!(f, "rejected"),
            ApprovalStatus::Timeout => write!(f, "timeout"),
        }
    }
}

/// 项目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub source: String,
    pub encoded_dir_name: Option<String>,
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
    pub meta: Option<String>,
    // 会话分类
    pub session_type: Option<String>,
    pub source: Option<String>,
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
    pub content_text: String, // 纯对话文本（用于向量化）
    pub content_full: String, // 完整格式化内容（用于 FTS）
    pub timestamp: i64,
    pub sequence: i64,
    pub source: Option<String>,
    pub channel: Option<String>,
    pub model: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_args: Option<String>,
    pub raw: Option<String>,
    pub vector_indexed: bool,                    // 是否已向量索引
    pub approval_status: Option<ApprovalStatus>, // 审批状态: pending, approved, rejected, timeout
    pub approval_resolved_at: Option<i64>,       // 审批解决时间戳（毫秒）
}

// MessageType 直接使用 ai_cli_session_collector::MessageType，在 lib.rs 中 re-export

/// 搜索排序方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchOrderBy {
    /// 按相关性分数排序（默认）
    #[default]
    Score,
    /// 按时间倒序（最新优先）
    TimeDesc,
    /// 按时间正序（最早优先）
    TimeAsc,
}

/// 搜索结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub message_id: i64,
    pub session_id: String,
    pub project_id: i64,
    pub project_name: String,
    pub r#type: String,
    pub content_full: String, // FTS 搜索使用 content_full
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

/// 项目（带统计信息）
/// 使用 camelCase 序列化，与 JSON API 标准保持一致
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectWithStats {
    pub id: i64,
    pub name: String,
    #[serde(rename = "projectPath")]
    pub path: String,
    pub session_count: i64,
    pub message_count: i64,
    pub last_active: Option<i64>, // 最后活跃时间（毫秒时间戳）
}

/// 会话（带项目信息）- 用于返回给客户端
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWithProject {
    pub id: i64,
    pub session_id: String,
    pub project_id: i64,
    pub project_name: String,
    pub project_path: String,
    pub message_count: i64,
    pub last_message_at: Option<i64>,
    // 会话元数据
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub channel: Option<String>,
    // 增量检测字段
    pub file_mtime: Option<i64>,
    pub file_size: Option<i64>,
    // 额外元信息
    pub encoded_dir_name: Option<String>,
    pub meta: Option<String>,
    // 会话分类
    pub session_type: Option<String>,
    pub source: Option<String>,
    // 时间戳
    pub created_at: i64,
    pub updated_at: i64,
    // 最后一条消息预览（V5）
    pub last_message_type: Option<String>,
    pub last_message_preview: Option<String>,
    // Session Chain 关系（V6）
    pub children_count: Option<i64>,
    pub parent_session_id: Option<String>,
    pub child_session_ids: Option<Vec<String>>,
}

/// Talk 摘要 (Compact 结果)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TalkSummary {
    pub id: i64,
    pub session_id: String,
    pub talk_id: String,
    pub summary_l2: String,         // L2 摘要（每个 Talk 的摘要）
    pub summary_l3: Option<String>, // L3 摘要（Session 级别汇总）
    pub created_at: i64,
    pub updated_at: i64,
}

/// 会话关系（parent→child）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRelation {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub relation_type: String,
    pub source: String,
    pub created_at: i64,
}
