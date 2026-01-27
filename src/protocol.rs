//! IPC 协议定义
//!
//! 通信方式：Unix Socket + JSONL（每条消息一行 JSON + '\n'）

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Claude Code Hook 事件（L2 瞬时通知）
///
/// 由 claude_hook.sh 发送，用于即时 UI 反馈（如 Tab 装饰）。
/// event_type 使用 string 保证向前兼容（未知类型静默忽略）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    /// 事件类型：SessionStart/SessionEnd/UserPromptSubmit/Stop/Notification/PermissionRequest
    pub event_type: String,
    /// 会话 ID
    pub session_id: String,
    /// transcript 文件路径（用于触发 Collection）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    /// 工作目录
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 用户输入（UserPromptSubmit 事件）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// 工具名称（PermissionRequest 事件）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// 工具输入（PermissionRequest 事件）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    /// 工具调用 ID（PermissionRequest 事件）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// 通知类型（Notification 事件）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_type: Option<String>,
    /// 通知消息（Notification 事件）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 事件上下文（来源特定数据，vimo-agent 透传不解析）
    ///
    /// 用于携带消费者特定的数据，如 ETerm 的 terminal_id。
    /// vimo-agent 只负责透传，不解析内容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

/// 已知的 Hook 事件类型常量
pub mod hook_event_type {
    pub const SESSION_START: &str = "SessionStart";
    pub const SESSION_END: &str = "SessionEnd";
    pub const USER_PROMPT_SUBMIT: &str = "UserPromptSubmit";
    pub const STOP: &str = "Stop";
    pub const NOTIFICATION: &str = "Notification";
    pub const PERMISSION_REQUEST: &str = "PermissionRequest";
}

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

    /// Hook 事件（来自 claude_hook.sh）
    ///
    /// 触发即时 Collection 并广播给订阅者
    HookEvent(HookEvent),
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

    /// Hook 事件广播（L2 瞬时通知）
    HookEvent(HookEvent),
}

/// 事件类型（用于订阅）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    NewMessage,
    SessionStart,
    SessionEnd,
    /// Hook 事件（L2 瞬时通知，用于 UI 即时反馈）
    HookEvent,
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
    /// Hook 事件（L2 瞬时通知）
    HookEvent(HookEvent),
}

impl Event {
    /// 获取事件类型
    pub fn event_type(&self) -> EventType {
        match self {
            Event::NewMessages { .. } => EventType::NewMessage,
            Event::SessionStart { .. } => EventType::SessionStart,
            Event::SessionEnd { .. } => EventType::SessionEnd,
            Event::HookEvent(_) => EventType::HookEvent,
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
            Event::HookEvent(hook_event) => Push::HookEvent(hook_event.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_event_serialize_minimal() {
        // 最小 HookEvent（只有必填字段）
        let event = HookEvent {
            event_type: "SessionStart".to_string(),
            session_id: "test-session-123".to_string(),
            transcript_path: None,
            cwd: None,
            prompt: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            notification_type: None,
            message: None,
            context: None,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"SessionStart\""));
        assert!(json.contains("\"session_id\":\"test-session-123\""));
        // 可选字段应被跳过
        assert!(!json.contains("transcript_path"));
        assert!(!json.contains("cwd"));
        assert!(!json.contains("context"));
    }

    #[test]
    fn test_hook_event_serialize_full() {
        // 完整 HookEvent（PermissionRequest 场景）
        let event = HookEvent {
            event_type: "PermissionRequest".to_string(),
            session_id: "test-session-456".to_string(),
            transcript_path: Some("/path/to/transcript.jsonl".to_string()),
            cwd: Some("/Users/test/project".to_string()),
            prompt: None,
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            tool_use_id: Some("tool-123".to_string()),
            notification_type: None,
            message: None,
            context: None,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"tool_name\":\"Bash\""));
        assert!(json.contains("\"tool_use_id\":\"tool-123\""));
    }

    #[test]
    fn test_hook_event_deserialize() {
        // 从 claude_hook.sh 发送的 JSON
        let json = r#"{
            "type": "HookEvent",
            "event_type": "UserPromptSubmit",
            "session_id": "abc-123",
            "transcript_path": "/path/to/file.jsonl",
            "cwd": "/Users/test",
            "prompt": "Hello, Claude!"
        }"#;

        let request: Request = serde_json::from_str(json).unwrap();
        match request {
            Request::HookEvent(event) => {
                assert_eq!(event.event_type, "UserPromptSubmit");
                assert_eq!(event.session_id, "abc-123");
                assert_eq!(event.prompt, Some("Hello, Claude!".to_string()));
            }
            _ => panic!("Expected HookEvent"),
        }
    }

    #[test]
    fn test_hook_event_deserialize_unknown_fields() {
        // 未来 Claude Code 可能新增字段，应能正常解析
        let json = r#"{
            "type": "HookEvent",
            "event_type": "FutureEvent",
            "session_id": "xyz-789",
            "new_field": "should be ignored"
        }"#;

        let request: Request = serde_json::from_str(json).unwrap();
        match request {
            Request::HookEvent(event) => {
                assert_eq!(event.event_type, "FutureEvent");
                assert_eq!(event.session_id, "xyz-789");
            }
            _ => panic!("Expected HookEvent"),
        }
    }

    #[test]
    fn test_event_type_hook_event() {
        let hook_event = HookEvent {
            event_type: "Stop".to_string(),
            session_id: "test".to_string(),
            transcript_path: None,
            cwd: None,
            prompt: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            notification_type: None,
            message: None,
            context: None,
        };

        let event = Event::HookEvent(hook_event.clone());
        assert_eq!(event.event_type(), EventType::HookEvent);

        // to_push 转换
        let push = event.to_push();
        match push {
            Push::HookEvent(e) => {
                assert_eq!(e.event_type, "Stop");
                assert_eq!(e.session_id, "test");
            }
            _ => panic!("Expected Push::HookEvent"),
        }
    }

    #[test]
    fn test_push_hook_event_serialize() {
        let hook_event = HookEvent {
            event_type: "SessionEnd".to_string(),
            session_id: "test-session".to_string(),
            transcript_path: Some("/path/to/file.jsonl".to_string()),
            cwd: None,
            prompt: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            notification_type: None,
            message: None,
            context: None,
        };

        let push = Push::HookEvent(hook_event);
        let json = serde_json::to_string(&push).unwrap();

        assert!(json.contains("\"type\":\"HookEvent\""));
        assert!(json.contains("\"event_type\":\"SessionEnd\""));
    }

    #[test]
    fn test_event_type_subscribe() {
        // 验证 HookEvent 可以被订阅
        let events = vec![EventType::NewMessage, EventType::HookEvent];
        let request = Request::Subscribe { events };
        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"HookEvent\""));
    }

    #[test]
    fn test_hook_event_with_context() {
        // ETerm 场景：context 包含 terminal_id
        let event = HookEvent {
            event_type: "SessionStart".to_string(),
            session_id: "abc-123".to_string(),
            transcript_path: Some("/path/to/file.jsonl".to_string()),
            cwd: Some("/Users/test/project".to_string()),
            prompt: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            notification_type: None,
            message: None,
            context: Some(serde_json::json!({"terminal_id": 5})),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"context\":{\"terminal_id\":5}"));

        // 反序列化验证
        let parsed: HookEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.context.unwrap()["terminal_id"], 5);
    }

    #[test]
    fn test_hook_event_deserialize_with_context() {
        // 从 claude_hook.sh 发送的带 context 的 JSON
        let json = r#"{
            "type": "HookEvent",
            "event_type": "SessionStart",
            "session_id": "abc-123",
            "transcript_path": "/path/to/file.jsonl",
            "context": {"terminal_id": 123, "extra_field": "value"}
        }"#;

        let request: Request = serde_json::from_str(json).unwrap();
        match request {
            Request::HookEvent(event) => {
                assert_eq!(event.event_type, "SessionStart");
                assert_eq!(event.session_id, "abc-123");
                let ctx = event.context.unwrap();
                assert_eq!(ctx["terminal_id"], 123);
                assert_eq!(ctx["extra_field"], "value");
            }
            _ => panic!("Expected HookEvent"),
        }
    }

    #[test]
    fn test_hook_event_context_transparent() {
        // vimo-agent 透传 context，不解析内容
        let hook_event = HookEvent {
            event_type: "Stop".to_string(),
            session_id: "test".to_string(),
            transcript_path: None,
            cwd: None,
            prompt: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            notification_type: None,
            message: None,
            context: Some(serde_json::json!({
                "terminal_id": 42,
                "custom_data": {"nested": true}
            })),
        };

        // Event → Push 转换应保留 context
        let event = Event::HookEvent(hook_event);
        let push = event.to_push();

        match push {
            Push::HookEvent(e) => {
                let ctx = e.context.unwrap();
                assert_eq!(ctx["terminal_id"], 42);
                assert_eq!(ctx["custom_data"]["nested"], true);
            }
            _ => panic!("Expected Push::HookEvent"),
        }
    }
}
