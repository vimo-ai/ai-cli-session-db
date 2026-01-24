//! IPC 协议定义
//!
//! 通信方式：Unix Socket + JSONL（每条消息一行 JSON + '\n'）

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 请求类型（Client → Agent）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// 握手
    Handshake {
        /// 组件名称：memex-rs / vlaudekit / memexkit / vlaude-daemon
        component: String,
        /// 组件版本（用于日志和诊断）
        version: String,
    },

    /// Kit 通知文件变化（增强实时性）
    NotifyFileChange {
        /// 文件路径（transcriptPath from ClaudeKit Hooks）
        path: PathBuf,
    },

    /// 订阅事件
    Subscribe {
        /// 要订阅的事件类型
        events: Vec<EventType>,
    },

    /// 取消订阅
    Unsubscribe {
        /// 要取消的事件类型
        events: Vec<EventType>,
    },

    /// 写入 Index 结果（from memex-rs）
    WriteIndexResult {
        session_id: String,
        /// 已索引的消息 ID 列表
        indexed_message_ids: Vec<i64>,
    },

    /// 写入 Compact 结果（from memex-rs）
    WriteCompactResult {
        session_id: String,
        /// Talk ID
        talk_id: String,
        /// L2 摘要
        summary_l2: String,
        /// L3 摘要（可选）
        summary_l3: Option<String>,
    },

    /// 写入 Approve 结果（from vlaude/VlaudeKit）
    WriteApproveResult {
        /// Tool call ID
        tool_call_id: String,
        /// 审批状态
        status: ApprovalStatus,
        /// 解决时间
        resolved_at: i64,
    },

    /// 心跳（保持连接）
    Heartbeat,

    /// 查询（预留）
    Query {
        /// 查询类型
        query_type: QueryType,
    },
}

/// 响应类型（Agent → Client）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    /// 成功
    Ok,

    /// 错误
    Error {
        code: i32,
        message: String,
    },

    /// 握手成功
    HandshakeOk {
        /// Agent 版本
        agent_version: String,
    },

    /// 查询结果
    QueryResult {
        data: serde_json::Value,
    },
}

/// 推送事件（Agent → 订阅者）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Push {
    /// 新消息
    NewMessages {
        session_id: String,
        path: String,
        count: usize,
        message_ids: Vec<i64>,
    },

    /// 会话开始
    SessionStart {
        session_id: String,
        project_path: String,
    },

    /// 会话结束（预留）
    SessionEnd {
        session_id: String,
    },
}

/// 事件类型（用于订阅）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    NewMessage,
    SessionStart,
    SessionEnd,
}

/// 审批状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Timeout,
}

/// 查询类型（预留）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "query")]
pub enum QueryType {
    /// 获取 Agent 状态
    Status,
    /// 获取连接数
    ConnectionCount,
}

/// 事件（内部使用，用于广播）
#[derive(Debug, Clone)]
pub enum Event {
    NewMessages {
        session_id: String,
        path: PathBuf,
        count: usize,
        message_ids: Vec<i64>,
    },
    SessionStart {
        session_id: String,
        project_path: String,
    },
    SessionEnd {
        session_id: String,
    },
}

impl Event {
    /// 获取事件类型
    pub fn event_type(&self) -> EventType {
        match self {
            Event::NewMessages { .. } => EventType::NewMessage,
            Event::SessionStart { .. } => EventType::SessionStart,
            Event::SessionEnd { .. } => EventType::SessionEnd,
        }
    }

    /// 转换为 Push 消息
    pub fn to_push(&self) -> Push {
        match self {
            Event::NewMessages {
                session_id,
                path,
                count,
                message_ids,
            } => Push::NewMessages {
                session_id: session_id.clone(),
                path: path.to_string_lossy().to_string(),
                count: *count,
                message_ids: message_ids.clone(),
            },
            Event::SessionStart {
                session_id,
                project_path,
            } => Push::SessionStart {
                session_id: session_id.clone(),
                project_path: project_path.clone(),
            },
            Event::SessionEnd { session_id } => Push::SessionEnd {
                session_id: session_id.clone(),
            },
        }
    }
}
