//! 采集服务 - 扫描和收集多种 CLI 会话
//!
//! 从 memex-rs/collector 下沉，统一业务逻辑。
//! 支持多数据源：Claude、OpenCode、Codex 等。

use crate::db::{MessageInput, SessionDB, SessionInput};
use crate::{all_adapters, ConversationAdapter};
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

/// 采集结果
#[derive(Debug, Default, Clone)]
pub struct CollectResult {
    pub projects_scanned: usize,
    pub sessions_scanned: usize,
    pub messages_inserted: usize,
    pub new_message_ids: Vec<i64>,
    pub errors: Vec<String>,
}

/// 采集服务
///
/// 封装多数据源采集逻辑，支持全量和增量采集。
pub struct Collector<'a> {
    db: &'a SessionDB,
    adapters: Vec<Arc<dyn ConversationAdapter>>,
}

impl<'a> Collector<'a> {
    /// 创建采集服务
    pub fn new(db: &'a SessionDB) -> Self {
        Self {
            db,
            adapters: all_adapters(),
        }
    }

    /// 执行全量采集
    ///
    /// 遍历所有适配器，扫描所有会话文件，增量写入数据库。
    /// 使用时间戳增量采集：只采集比数据库中最新消息更新的消息（提前量 30 分钟）。
    pub fn collect_all(&self) -> Result<CollectResult> {
        const BUFFER_MS: i64 = 30 * 60 * 1000; // 30 分钟提前量

        let mut result = CollectResult::default();

        // 遍历所有适配器
        for adapter in &self.adapters {
            let source = adapter.source();

            // 列出所有会话
            let sessions = match adapter.list_sessions() {
                Ok(s) => s,
                Err(e) => {
                    let err_msg = format!("{:?} failed to list sessions: {}", source, e);
                    tracing::warn!("{}", err_msg);
                    result.errors.push(err_msg);
                    continue;
                }
            };

            for meta in sessions {
                // 跳过空 project_path 的会话（文件可能不完整，下次采集会重试）
                if meta.project_path.is_empty() {
                    tracing::debug!("跳过空 project_path: session_id={}", meta.id);
                    continue;
                }

                // 获取或创建项目
                let project_name = meta
                    .project_name
                    .as_deref()
                    .unwrap_or_else(|| extract_project_name(&meta.project_path));
                let source_str = source.to_string();

                let project_id = match self.db.get_or_create_project_with_encoded(
                    project_name,
                    &meta.project_path,
                    &source_str,
                    meta.encoded_dir_name.as_deref(),
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        result
                            .errors
                            .push(format!("Failed to create project: {}", e));
                        continue;
                    }
                };

                // 获取数据库中该会话的最新消息时间戳（时间戳增量采集）
                let latest_ts = self
                    .db
                    .get_session_latest_timestamp(&meta.id)
                    .unwrap_or(None);
                let cutoff_ts = latest_ts.map(|ts| ts - BUFFER_MS).unwrap_or(0);

                // 解析会话
                let parse_result = match adapter.parse_session(&meta) {
                    Ok(Some(r)) => r,
                    Ok(None) => continue,
                    Err(e) => {
                        let err_msg = format!("Failed to parse session {}: {}", meta.id, e);
                        tracing::debug!("{}", err_msg);
                        result.errors.push(err_msg);
                        continue;
                    }
                };

                // 创建会话
                let session_input = SessionInput {
                    session_id: meta.id.clone(),
                    project_id,
                    cwd: parse_result.cwd.clone(),
                    model: parse_result.model.clone(),
                    channel: meta.channel.clone(),
                    message_count: Some(parse_result.messages.len() as i64),
                    file_mtime: None,
                    file_size: None,
                    meta: None,
                };
                if let Err(e) = self.db.upsert_session_full(&session_input) {
                    result
                        .errors
                        .push(format!("Failed to create session: {}", e));
                    continue;
                }

                // 转换并插入消息（时间戳增量过滤）
                let messages: Vec<MessageInput> = parse_result
                    .messages
                    .iter()
                    .enumerate()
                    .filter_map(|(i, msg)| {
                        let timestamp = msg
                            .timestamp
                            .as_ref()
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or_else(|| {
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as i64)
                                    .unwrap_or(0)
                            });

                        // 只保留比 cutoff_ts 更新的消息
                        if timestamp <= cutoff_ts {
                            return None;
                        }

                        Some(MessageInput {
                            uuid: msg.uuid.clone(),
                            r#type: msg.message_type,
                            content_text: msg.content.text.clone(),
                            content_full: msg.content.full.clone(),
                            timestamp,
                            sequence: i as i64,
                            source: Some(msg.source.to_string()),
                            channel: msg.channel.clone(),
                            model: msg.model.clone(),
                            tool_call_id: msg.tool_call_id.clone(),
                            tool_name: msg.tool_name.clone(),
                            tool_args: msg.tool_args.clone(),
                            raw: msg.raw.clone(),
                            approval_status: None,
                            approval_resolved_at: None,
                        })
                    })
                    .collect();

                // 如果没有新消息，跳过
                if messages.is_empty() {
                    continue;
                }

                match self.db.insert_messages(&meta.id, &messages) {
                    Ok(inserted) => {
                        if inserted > 0 {
                            result.sessions_scanned += 1;
                            result.messages_inserted += inserted;
                            tracing::debug!("Session {} inserted {} messages", meta.id, inserted);
                        }
                    }
                    Err(e) => {
                        result
                            .errors
                            .push(format!("Failed to insert messages: {}", e));
                    }
                }
            }

            result.projects_scanned += 1;
        }

        // Only print when there are new messages
        if result.messages_inserted > 0 {
            tracing::info!(
                "Collect: {} sessions, {} new messages",
                result.sessions_scanned,
                result.messages_inserted
            );
        }

        Ok(result)
    }

    /// 按路径采集单个会话（精确索引）
    ///
    /// 使用时间戳增量采集：只采集比数据库中最新消息更新的消息（提前量 30 分钟）。
    /// 支持多数据源：根据文件路径自动选择正确的 Adapter。
    pub fn collect_by_path(&self, path: &str) -> Result<CollectResult> {
        const BUFFER_MS: i64 = 30 * 60 * 1000; // 30 分钟提前量

        let mut result = CollectResult::default();
        let file_path = Path::new(path);

        // 根据路径找到对应的 adapter
        let adapter = match self.adapters.iter().find(|a| a.should_handle(file_path)) {
            Some(a) => a.clone(),
            None => {
                tracing::debug!("No adapter found for path: {}", path);
                return Ok(result);
            }
        };

        let source = adapter.source();
        let source_str = source.to_string();

        // 列出会话元数据（找到匹配此路径的会话）
        let sessions = match adapter.list_sessions() {
            Ok(s) => s,
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to list sessions: {}", e));
                return Ok(result);
            }
        };

        // 找到对应的会话元数据
        let meta = match sessions
            .iter()
            .find(|m| m.session_path.as_deref() == Some(path))
        {
            Some(m) => m,
            None => {
                tracing::debug!("Session meta not found for path: {}", path);
                return Ok(result);
            }
        };

        // 解析会话
        let parse_result = match adapter.parse_session(meta) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(result),
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to parse session {}: {}", meta.id, e));
                return Ok(result);
            }
        };

        let encoded_dir_name = extract_encoded_dir_name(path);

        // 获取数据库中该会话的最新消息时间戳
        let latest_ts = self
            .db
            .get_session_latest_timestamp(&meta.id)
            .unwrap_or(None);
        let cutoff_ts = latest_ts.map(|ts| ts - BUFFER_MS).unwrap_or(0);

        // 跳过空 project_path 的会话（文件可能不完整，下次采集会重试）
        if meta.project_path.is_empty() {
            tracing::debug!("跳过空 project_path: session_id={}", meta.id);
            return Ok(result);
        }

        // 获取或创建项目
        let project_name = meta
            .project_name
            .as_deref()
            .unwrap_or_else(|| extract_project_name(&meta.project_path));

        let project_id = match self.db.get_or_create_project_with_encoded(
            project_name,
            &meta.project_path,
            &source_str,
            encoded_dir_name.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to create project: {}", e));
                return Ok(result);
            }
        };

        // 创建/更新会话
        let session_input = SessionInput {
            session_id: meta.id.clone(),
            project_id,
            cwd: parse_result.cwd.clone(),
            model: parse_result.model.clone(),
            channel: meta.channel.clone(),
            message_count: Some(parse_result.messages.len() as i64),
            file_mtime: None,
            file_size: None,
            meta: None,
        };
        if let Err(e) = self.db.upsert_session_full(&session_input) {
            result
                .errors
                .push(format!("Failed to create session: {}", e));
            return Ok(result);
        }

        // 转换消息格式，过滤掉旧消息（时间戳增量采集）
        let messages: Vec<MessageInput> = parse_result
            .messages
            .iter()
            .enumerate()
            .filter_map(|(i, msg)| {
                let timestamp = msg
                    .timestamp
                    .as_ref()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or_else(|| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0)
                    });

                // 只保留比 cutoff_ts 更新的消息
                if timestamp <= cutoff_ts {
                    return None;
                }

                Some(MessageInput {
                    uuid: msg.uuid.clone(),
                    r#type: msg.message_type,
                    content_text: msg.content.text.clone(),
                    content_full: msg.content.full.clone(),
                    timestamp,
                    sequence: i as i64,
                    source: Some(msg.source.to_string()),
                    channel: msg.channel.clone(),
                    model: msg.model.clone(),
                    tool_call_id: msg.tool_call_id.clone(),
                    tool_name: msg.tool_name.clone(),
                    tool_args: msg.tool_args.clone(),
                    raw: msg.raw.clone(),
                    approval_status: None,
                    approval_resolved_at: None,
                })
            })
            .collect();

        if messages.is_empty() {
            result.projects_scanned = 1;
            return Ok(result);
        }

        // 插入消息（ON CONFLICT DO NOTHING 保证不重复）
        match self.db.insert_messages(&meta.id, &messages) {
            Ok(inserted) => {
                result.sessions_scanned = 1;
                result.messages_inserted = inserted;
                if inserted > 0 {
                    tracing::info!(
                        "Incremental indexing [{}]: session {} inserted {} messages",
                        source_str,
                        meta.id,
                        inserted
                    );
                }
            }
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to insert messages: {}", e));
            }
        }

        result.projects_scanned = 1;
        Ok(result)
    }
}

/// 从 JSONL 文件路径提取 encoded_dir_name
fn extract_encoded_dir_name(path: &str) -> Option<String> {
    let path = Path::new(path);
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

/// 从路径提取项目名
fn extract_project_name(path: &str) -> &str {
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_encoded_dir_name() {
        // Claude 路径
        let path = "/Users/test/.claude/projects/-Users-test-myproject/session-123.jsonl";
        let encoded = extract_encoded_dir_name(path);
        assert_eq!(encoded, Some("-Users-test-myproject".to_string()));

        // OpenCode 路径
        let path = "/Users/test/.local/share/opencode/storage/session/proj1/ses_123.json";
        let encoded = extract_encoded_dir_name(path);
        assert_eq!(encoded, Some("proj1".to_string()));

        // 根目录文件
        let path = "/file.txt";
        let encoded = extract_encoded_dir_name(path);
        assert_eq!(encoded, None);
    }

    #[test]
    fn test_extract_project_name() {
        assert_eq!(extract_project_name("/Users/test/myproject"), "myproject");
        assert_eq!(
            extract_project_name("/Users/test/my-project/"),
            "my-project"
        );
        assert_eq!(extract_project_name("simple"), "simple");
        assert_eq!(extract_project_name(""), "");
    }
}
