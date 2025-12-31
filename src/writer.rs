//! Writer 模式辅助功能
//!
//! 提供增量扫描、批量写入等 Writer 专用功能

use crate::db::{MessageInput, SessionDB};
use crate::error::Result;
use ai_cli_session_collector::MessageType;

/// 安全边界时间 (毫秒)
/// 增量扫描时回退的时间，防止边界消息丢失
const SAFETY_MARGIN_MS: i64 = 60_000; // 60s

impl SessionDB {
    /// 增量扫描单个 session
    ///
    /// 根据 checkpoint 过滤消息，只处理新增的
    ///
    /// # 参数
    /// - `session_id`: 会话 ID
    /// - `project_id`: 项目 ID
    /// - `messages`: 所有消息 (从 JSONL 解析)
    ///
    /// # 返回
    /// 实际插入的消息数量
    pub fn scan_session_incremental(
        &self,
        session_id: &str,
        project_id: i64,
        messages: Vec<MessageInput>,
    ) -> Result<usize> {
        // 确保 session 存在
        self.upsert_session(session_id, project_id)?;

        // 获取检查点
        let checkpoint = self.get_scan_checkpoint(session_id)?;

        // 过滤需要处理的消息
        let messages_to_process: Vec<_> = match checkpoint {
            Some(last_ts) => {
                // 增量扫描：回退安全边界
                let cutoff = last_ts.saturating_sub(SAFETY_MARGIN_MS);
                messages
                    .into_iter()
                    .filter(|m| m.timestamp > cutoff)
                    .collect()
            }
            None => {
                // 首次扫描：全量
                messages
            }
        };

        if messages_to_process.is_empty() {
            return Ok(0);
        }

        // 写入
        let inserted = self.insert_messages(session_id, &messages_to_process)?;

        // 更新检查点
        if let Some(last) = messages_to_process.last() {
            self.update_session_last_message(session_id, last.timestamp)?;
        }

        Ok(inserted)
    }
}

/// 从 ai-cli-session-collector 消息转换为 MessageInput
pub fn convert_message(msg: &ai_cli_session_collector::ParsedMessage, sequence: i64) -> MessageInput {
    let message_type = match msg.message_type {
        ai_cli_session_collector::MessageType::User => MessageType::User,
        ai_cli_session_collector::MessageType::Assistant => MessageType::Assistant,
        ai_cli_session_collector::MessageType::Tool => MessageType::Tool,
    };

    // 解析时间戳 (ISO 8601 -> 毫秒)
    let timestamp = msg.timestamp
        .as_ref()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);

    MessageInput {
        uuid: msg.uuid.clone(),
        r#type: message_type,
        content_text: msg.content.text.clone(),
        content_full: msg.content.full.clone(),
        timestamp,
        sequence,
        source: Some(msg.source.to_string()),
        channel: msg.channel.clone(),
        model: msg.model.clone(),
        tool_call_id: msg.tool_call_id.clone(),
        tool_name: msg.tool_name.clone(),
        tool_args: msg.tool_args.clone(),
        raw: msg.raw.clone(),
    }
}

/// 批量转换消息
pub fn convert_messages(messages: &[ai_cli_session_collector::ParsedMessage]) -> Vec<MessageInput> {
    messages
        .iter()
        .enumerate()
        .map(|(i, m)| convert_message(m, i as i64))
        .collect()
}
