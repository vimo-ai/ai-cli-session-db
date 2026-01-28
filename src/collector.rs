//! 采集服务 - 扫描和收集多种 CLI 会话
//!
//! 从 memex-rs/collector 下沉，统一业务逻辑。
//! 支持多数据源：Claude、OpenCode、Codex 等。

use crate::db::{MessageInput, SessionDB, SessionInput};
use crate::{
    all_adapters, ConversationAdapter, FileIdentity, IncrementalAdapter, ReaderState, SessionMeta,
};
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
                    tracing::debug!("Skipping empty project_path: session_id={}", meta.id);
                    continue;
                }

                // mtime 剪枝：文件未变化则跳过
                if let Some(file_mtime) = meta.file_mtime {
                    if let Ok(Some(db_mtime)) = self.db.get_session_file_mtime(&meta.id) {
                        if file_mtime == db_mtime as u64 {
                            continue; // 文件未变化，跳过
                        }
                    }
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
                    file_mtime: meta.file_mtime.map(|t| t as i64),
                    file_size: meta.file_size.map(|s| s as i64),
                    file_offset: None, // 全量扫描不使用增量读取
                    file_inode: None,
                    meta: None,
                };
                if let Err(e) = self.db.upsert_session_full(&session_input) {
                    result
                        .errors
                        .push(format!("Failed to create session: {}", e));
                    continue;
                }

                // 获取当前最大 sequence，增量写入时从 max+1 开始
                let max_sequence = self
                    .db
                    .get_session_max_sequence(&meta.id)
                    .unwrap_or(None)
                    .unwrap_or(-1);
                let start_sequence = max_sequence + 1;

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
                            sequence: start_sequence + i as i64,
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
    /// 直接从文件路径解析，不扫描目录。
    /// 使用字节偏移量增量采集：只读取文件新增的部分。
    pub fn collect_by_path(&self, path: &str) -> Result<CollectResult> {
        use std::fs;

        let mut result = CollectResult::default();
        let file_path = Path::new(path);

        // 从路径提取 session_id
        let session_id = match file_path.file_stem().and_then(|s| s.to_str()) {
            Some(id) => id.to_string(),
            None => {
                tracing::debug!("Invalid file path: {}", path);
                return Ok(result);
            }
        };

        // 获取文件元数据
        let file_metadata = match fs::metadata(file_path) {
            Ok(meta) => meta,
            Err(e) => {
                tracing::debug!("Cannot get file metadata {}: {}", path, e);
                return Ok(result);
            }
        };

        let file_mtime = file_metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        let file_size = file_metadata.len() as i64;
        // 跨平台文件标识：Unix 用 inode，Windows 用 file_index
        let file_inode = file_id::get_file_id(file_path)
            .map(|id| match id {
                file_id::FileId::Inode { inode_number, .. } => inode_number as i64,
                file_id::FileId::LowRes { file_index, .. } => file_index as i64,
                file_id::FileId::HighRes { file_id, .. } => file_id as i64,
            })
            .unwrap_or(0);

        // 从路径提取 encoded_dir_name
        let encoded_dir_name = extract_encoded_dir_name(path);

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

        // 尝试获取 IncrementalAdapter（目前只有 ClaudeAdapter 支持）
        let incremental_adapter: Option<&dyn IncrementalAdapter> =
            if adapter.meta().source == crate::Source::Claude {
                // 安全的向下转换：ClaudeAdapter 实现了 IncrementalAdapter
                // 由于 Rust trait object 的限制，我们需要创建一个新的 ClaudeAdapter
                None // 暂时禁用，下面会直接用 ClaudeAdapter
            } else {
                None
            };

        // 直接构造 SessionMeta，不扫描目录
        let meta = SessionMeta {
            id: session_id.clone(),
            source,
            channel: Some("code".to_string()),
            project_path: String::new(), // 后面从解析结果获取
            project_name: None,
            encoded_dir_name: encoded_dir_name.clone(),
            session_path: Some(path.to_string()),
            file_mtime: None,
            file_size: None,
            message_count: None,
            cwd: None,
            model: None,
            meta: None,
            created_at: None,
            updated_at: None,
            last_message_type: None,
            last_message_preview: None,
            last_message_at: None,
        };

        // 检查是否支持增量读取
        let use_incremental = source == crate::Source::Claude && incremental_adapter.is_none();

        // 如果是 Claude 源，使用增量读取
        let (parse_result, new_state) = if use_incremental {
            // 获取数据库中保存的增量状态
            let saved_state = self.db.get_session_incremental_state(&session_id)?;

            // 构建 ReaderState
            let reader_state = saved_state.map(|(offset, mtime, size, inode)| {
                let file_id = FileIdentity::new(
                    inode.unwrap_or(0) as u64,
                    mtime.unwrap_or(0) as u64,
                    size.unwrap_or(0) as u64,
                );
                ReaderState::from_saved(offset as u64, file_id)
            });

            // 使用 ClaudeAdapter 的增量读取
            let claude_adapter = crate::ClaudeAdapter::new();
            let incremental_result = claude_adapter.parse_session_incremental(&meta, reader_state)?;

            if incremental_result.was_reset {
                tracing::info!(
                    "Session {} file changed, triggering full re-parse",
                    session_id
                );
            }

            (incremental_result.result, Some(incremental_result.state))
        } else {
            // 使用传统的全量解析
            let result = adapter.parse_session(&meta)?;
            (result, None)
        };

        let parse_result = match parse_result {
            Some(r) => r,
            None => return Ok(result),
        };

        // 从解析结果获取 project_path（cwd）
        let project_path = match &parse_result.cwd {
            Some(cwd) if !cwd.is_empty() => cwd.clone(),
            _ => {
                tracing::debug!("Skipping empty cwd: session_id={}", session_id);
                return Ok(result);
            }
        };

        // 获取或创建项目
        let project_name = extract_project_name(&project_path);

        let project_id = match self.db.get_or_create_project_with_encoded(
            project_name,
            &project_path,
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
            session_id: session_id.clone(),
            project_id,
            cwd: parse_result.cwd.clone(),
            model: parse_result.model.clone(),
            channel: Some("code".to_string()),
            message_count: Some(parse_result.messages.len() as i64),
            file_mtime,
            file_size: Some(file_size),
            file_offset: new_state.as_ref().map(|s| s.offset as i64),
            file_inode: Some(file_inode),
            meta: None,
        };
        if let Err(e) = self.db.upsert_session_full(&session_input) {
            result
                .errors
                .push(format!("Failed to create session: {}", e));
            return Ok(result);
        }

        // 获取当前最大 sequence，增量写入时从 max+1 开始
        let max_sequence = self
            .db
            .get_session_max_sequence(&session_id)
            .unwrap_or(None)
            .unwrap_or(-1);
        let start_sequence = max_sequence + 1;

        // 转换消息格式
        let messages: Vec<MessageInput> = parse_result
            .messages
            .iter()
            .enumerate()
            .map(|(i, msg)| {
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

                MessageInput {
                    uuid: msg.uuid.clone(),
                    r#type: msg.message_type,
                    content_text: msg.content.text.clone(),
                    content_full: msg.content.full.clone(),
                    timestamp,
                    sequence: start_sequence + i as i64,
                    source: Some(msg.source.to_string()),
                    channel: msg.channel.clone(),
                    model: msg.model.clone(),
                    tool_call_id: msg.tool_call_id.clone(),
                    tool_name: msg.tool_name.clone(),
                    tool_args: msg.tool_args.clone(),
                    raw: msg.raw.clone(),
                    approval_status: None,
                    approval_resolved_at: None,
                }
            })
            .collect();

        if messages.is_empty() {
            result.projects_scanned = 1;
            return Ok(result);
        }

        // 插入消息（ON CONFLICT DO NOTHING 保证不重复）
        match self.db.insert_messages(&session_id, &messages) {
            Ok(inserted) => {
                result.sessions_scanned = 1;
                result.messages_inserted = inserted;
                if inserted > 0 {
                    tracing::info!(
                        "Incremental indexing [{}]: session {} inserted {} messages",
                        source_str,
                        session_id,
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

        // 更新增量状态
        if let Some(state) = new_state {
            if let Some(file_id) = &state.file_id {
                if let Err(e) = self.db.update_session_incremental_state(
                    &session_id,
                    state.offset as i64,
                    file_id.mtime as i64,
                    file_id.size as i64,
                    file_id.inode as i64,
                ) {
                    tracing::warn!("Failed to update incremental state: {}", e);
                }
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

/// 从路径提取项目名（跨平台）
fn extract_project_name(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
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
