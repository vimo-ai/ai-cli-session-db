//! 统一的会话读取器
//!
//! 提供读取 Claude Code 会话数据的统一 API，包括：
//! - 列出项目
//! - 列出会话（支持 agent 过滤）
//! - 读取消息（支持分页）
//! - 获取会话路径（从数据库查询，避免 encode_path）
//!
//! 所有业务逻辑在此实现，FFI 层只做类型转换。

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    ClaudeAdapter, ConversationAdapter, MessageType, ParseResult, ParsedMessage, SessionMeta,
    Source,
};

/// 生成消息预览（最多 100 个 Unicode 字符）
fn generate_preview(message: &ParsedMessage) -> String {
    match message.message_type {
        MessageType::User => {
            // 用户消息：直接使用纯文本内容
            truncate_chars(&message.content.text, 100)
        }
        MessageType::Assistant => {
            // 助手消息：尝试解析 content 数组生成摘要
            match &message.raw {
                Some(raw) => generate_assistant_preview(raw, &message.content.text),
                None => truncate_chars(&message.content.text, 100),
            }
        }
        _ => message.content.text.chars().take(100).collect(),
    }
}

/// 生成助手消息预览
fn generate_assistant_preview(raw: &str, fallback_text: &str) -> String {
    // 尝试解析 raw JSON 获取 content 数组
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(message) = json.get("message") {
            if let Some(content) = message.get("content") {
                if let Some(arr) = content.as_array() {
                    return generate_preview_from_content_blocks(arr);
                }
            }
        }
    }

    // 降级：使用纯文本
    truncate_chars(fallback_text, 100)
}

/// 从 content blocks 生成预览
fn generate_preview_from_content_blocks(blocks: &[serde_json::Value]) -> String {
    let mut result = String::new();
    let mut has_text = false;
    let mut has_thinking_only = true;

    for block in blocks {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match block_type {
            "text" => {
                has_text = true;
                has_thinking_only = false;
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !result.is_empty() {
                        result.push(' ');
                    }
                    result.push_str(text);
                }
            }
            "tool_use" => {
                has_thinking_only = false;
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let preview = generate_tool_use_preview(name, block.get("input"));
                if !result.is_empty() {
                    result.push(' ');
                }
                result.push_str(&preview);
            }
            "thinking" => {
                // 跳过 thinking，但记录存在
            }
            _ => {
                has_thinking_only = false;
            }
        }
    }

    // 如果只有 thinking 块
    if has_thinking_only && !has_text && result.is_empty() {
        return "💭 思考中...".to_string();
    }

    // 如果结果为空
    if result.is_empty() {
        return "（空消息）".to_string();
    }

    truncate_chars(&result, 100)
}

/// 生成 tool_use 预览
fn generate_tool_use_preview(name: &str, input: Option<&serde_json::Value>) -> String {
    let param = input
        .and_then(|i| match name {
            "Bash" => i.get("command").and_then(|c| c.as_str()),
            "Read" | "Write" | "Edit" => i.get("file_path").and_then(|f| {
                f.as_str().map(|s| {
                    // 只取文件名
                    std::path::Path::new(s)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(s)
                })
            }),
            "Glob" | "Grep" => i.get("pattern").and_then(|p| p.as_str()),
            _ => None,
        })
        .map(|s| truncate_chars(s, 30));

    match param {
        Some(p) => format!("🔧 {}: {}", name, p),
        None => format!("🔧 {}", name),
    }
}

/// 按 Unicode 字符截断
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        let mut result: String = chars.into_iter().collect();
        result.push_str("...");
        result
    } else {
        chars.into_iter().collect()
    }
}

/// 解析时间戳为毫秒
fn parse_timestamp_to_millis(ts: &str) -> Option<i64> {
    // 尝试解析 RFC3339 格式
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.timestamp_millis());
    }
    // 尝试解析纯数字（假设是毫秒）
    if let Ok(millis) = ts.parse::<i64>() {
        return Some(millis);
    }
    None
}

/// 排序方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

/// 项目信息
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    /// 编码后的目录名
    pub encoded_name: String,
    /// 解码后的真实路径
    pub path: String,
    /// 项目名称（从路径提取）
    pub name: String,
    /// 会话数量（不包含 agent session）
    pub session_count: usize,
    /// 最后活跃时间（毫秒时间戳）
    pub last_active: Option<u64>,
}

/// 消息读取结果
#[derive(Debug, Clone)]
pub struct MessagesResult {
    pub messages: Vec<ParsedMessage>,
    pub total: usize,
    pub has_more: bool,
}

/// 原始消息读取结果（不做格式转换）
#[derive(Debug, Clone)]
pub struct RawMessagesResult {
    pub messages: Vec<serde_json::Value>,
    pub total: usize,
    pub has_more: bool,
}

/// 会话 Metrics
#[derive(Debug, Clone)]
pub struct SessionMetrics {
    pub message_count: usize,
    pub user_message_count: usize,
    pub assistant_message_count: usize,
    pub estimated_tokens: usize,
    pub duration_seconds: Option<u64>,
}

/// 计算会话文件路径
///
/// 路径规则: `{projects_path}/{encoded_dir_name}/{session_id}.jsonl`
pub fn compute_session_path(
    projects_path: &std::path::Path,
    encoded_dir_name: &str,
    session_id: &str,
) -> PathBuf {
    projects_path
        .join(encoded_dir_name)
        .join(format!("{}.jsonl", session_id))
}

/// 统一的会话读取器
///
/// 提供读取 Claude Code 会话数据的所有功能。
/// 所有业务逻辑（包括 agent 过滤）都在这里实现。
pub struct SessionReader {
    /// Claude projects 目录路径
    projects_path: PathBuf,
    /// 内部 adapter
    adapter: ClaudeAdapter,
    /// 编码目录名缓存: project_path -> encoded_dir_name
    encoded_dir_cache: HashMap<String, String>,
}

impl SessionReader {
    /// 创建读取器
    pub fn new(projects_path: PathBuf) -> Self {
        let adapter = ClaudeAdapter::with_path(projects_path.clone());
        Self {
            projects_path,
            adapter,
            encoded_dir_cache: HashMap::new(),
        }
    }

    /// 使用默认路径创建读取器（跨平台）
    pub fn with_default_path() -> Option<Self> {
        let home = dirs::home_dir()?;
        let projects_path = home.join(".claude/projects");
        Some(Self::new(projects_path))
    }

    /// 从路径提取项目名
    pub fn extract_project_name(path: &str) -> String {
        ClaudeAdapter::extract_project_name(path)
    }

    /// 列出所有项目
    ///
    /// 会话数量不包含 agent session。
    pub fn list_projects(&mut self, limit: Option<usize>) -> Vec<ProjectInfo> {
        let mut results = Vec::new();

        if !self.projects_path.exists() {
            return results;
        }

        let entries = match fs::read_dir(&self.projects_path) {
            Ok(e) => e,
            Err(_) => return results,
        };

        for entry in entries.flatten() {
            let project_dir = entry.path();

            if !project_dir.is_dir() {
                continue;
            }

            let encoded_name = match project_dir.file_name().and_then(|s| s.to_str()) {
                Some(s) if !s.is_empty() && !s.starts_with('.') => s.to_string(),
                _ => continue,
            };

            // 从 JSONL 文件读取真实路径（优先使用 cwd）
            let (decoded_path, session_count, last_active) =
                self.scan_project_dir(&project_dir, &encoded_name);

            let project_name = Self::extract_project_name(&decoded_path);

            // 更新缓存
            self.encoded_dir_cache
                .insert(decoded_path.clone(), encoded_name.clone());

            results.push(ProjectInfo {
                encoded_name,
                path: decoded_path,
                name: project_name,
                session_count,
                last_active,
            });
        }

        // 按最后活跃时间排序（降序）
        results.sort_by(|a, b| b.last_active.cmp(&a.last_active));

        // 应用 limit
        if let Some(limit) = limit {
            results.truncate(limit);
        }

        results
    }

    /// 扫描项目目录，返回 (decoded_path, session_count, last_active)
    ///
    /// session_count 不包含 agent session
    fn scan_project_dir(
        &mut self,
        project_dir: &std::path::Path,
        encoded_name: &str,
    ) -> (String, usize, Option<u64>) {
        let mut session_count = 0;
        let mut last_active: Option<u64> = None;
        let mut decoded_path: Option<String> = None;

        let files = match fs::read_dir(project_dir) {
            Ok(f) => f,
            // 读取失败时使用 encoded_name 作为占位符（数据迁移后会有正确值）
            Err(_) => return (encoded_name.to_string(), 0, None),
        };

        for file_entry in files.flatten() {
            let file_path = file_entry.path();

            if !file_path.is_file() {
                continue;
            }

            let file_name = match file_path.file_name().and_then(|s| s.to_str()) {
                Some(s) if s.ends_with(".jsonl") => s,
                _ => continue,
            };

            let session_id = file_name.trim_end_matches(".jsonl");
            if session_id.is_empty() {
                continue;
            }

            // 过滤 agent session
            if session_id.starts_with("agent-") {
                continue;
            }

            session_count += 1;

            // 获取文件修改时间
            if let Ok(meta) = fs::metadata(&file_path) {
                if let Ok(mtime) = meta.modified() {
                    let ts = mtime
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    last_active = Some(last_active.map(|t| t.max(ts)).unwrap_or(ts));
                }
            }

            // 尝试从第一个文件读取 cwd（只需要读一次）
            if decoded_path.is_none() {
                if let Some(cwd) = Self::read_cwd_from_jsonl(&file_path) {
                    decoded_path = Some(cwd);
                }
            }
        }

        // 优先使用 cwd，无则使用 encoded_name 作为占位符
        let path = decoded_path.unwrap_or_else(|| encoded_name.to_string());
        (path, session_count, last_active)
    }

    /// 从 JSONL 文件读取 cwd 字段
    fn read_cwd_from_jsonl(file_path: &std::path::Path) -> Option<String> {
        let file = fs::File::open(file_path).ok()?;
        let reader = BufReader::new(file);

        for line in reader.lines().take(10) {
            let line = line.ok()?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(cwd) = json.get("cwd").and_then(|v| v.as_str()) {
                    return Some(cwd.to_string());
                }
            }
        }
        None
    }

    /// 列出项目下的所有会话
    ///
    /// # Arguments
    /// * `project_path` - 可选的项目路径过滤
    /// * `include_agents` - 是否包含 agent session (agent-xxx)
    pub fn list_sessions(
        &mut self,
        project_path: Option<&str>,
        include_agents: bool,
    ) -> Vec<SessionMeta> {
        // 使用 ClaudeAdapter 获取所有会话
        let mut sessions = match self.adapter.list_sessions() {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        // 更新 encoded_dir_cache
        for session in &sessions {
            if let Some(encoded) = &session.encoded_dir_name {
                self.encoded_dir_cache
                    .insert(session.project_path.clone(), encoded.clone());
            }
        }

        // 过滤 agent session
        if !include_agents {
            sessions.retain(|s| !s.id.starts_with("agent-"));
        }

        // 按项目路径过滤
        if let Some(path) = project_path {
            sessions.retain(|s| s.project_path == path);
        }

        // 按修改时间排序（降序）
        sessions.sort_by(|a, b| b.file_mtime.cmp(&a.file_mtime));

        sessions
    }

    /// 列出会话（带最后消息预览）
    pub fn list_sessions_with_preview(
        &mut self,
        project_path: Option<&str>,
        include_agents: bool,
    ) -> Vec<SessionMeta> {
        let mut sessions = self.list_sessions(project_path, include_agents);

        // 为每个 session 填充 lastMessage 预览
        for session in &mut sessions {
            if let Some(session_path) = &session.session_path {
                if let Some(last_msg) = self.read_last_message(session_path) {
                    session.last_message_type = Some(match last_msg.message_type {
                        MessageType::User => "user".to_string(),
                        MessageType::Assistant => "assistant".to_string(),
                        _ => "system".to_string(),
                    });
                    session.last_message_preview = Some(generate_preview(&last_msg));
                    session.last_message_at = last_msg
                        .timestamp
                        .as_ref()
                        .and_then(|ts| parse_timestamp_to_millis(ts));
                }
            }
        }

        sessions
    }

    /// 读取最后一条消息
    fn read_last_message(&self, session_path: &str) -> Option<ParsedMessage> {
        // 使用 read_messages 获取最后一条
        let result = self.read_messages(session_path, 1, 0, Order::Desc)?;
        result.messages.into_iter().next()
    }

    /// 查找最新会话
    pub fn find_latest_session(
        &mut self,
        project_path: &str,
        within_seconds: Option<u64>,
    ) -> Option<SessionMeta> {
        let sessions = self.list_sessions(Some(project_path), false);

        if sessions.is_empty() {
            return None;
        }

        let latest = sessions.into_iter().next()?;

        // 如果指定了时间范围，检查会话是否在范围内
        if let Some(within_secs) = within_seconds {
            if let Some(mtime) = latest.file_mtime {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let diff_ms = now.saturating_sub(mtime);
                if diff_ms > within_secs * 1000 {
                    return None;
                }
            }
        }

        Some(latest)
    }

    /// 获取会话文件路径
    ///
    /// 在 projects_path 下搜索 `{session_id}.jsonl` 文件
    pub fn get_session_path(&self, session_id: &str) -> Option<String> {
        let target_filename = format!("{}.jsonl", session_id);

        // 遍历所有项目目录
        let entries = fs::read_dir(&self.projects_path).ok()?;
        for entry in entries.flatten() {
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }

            // 跳过隐藏目录
            if project_dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with('.'))
                .unwrap_or(true)
            {
                continue;
            }

            // 检查文件是否存在
            let session_path = project_dir.join(&target_filename);
            if session_path.is_file() {
                return Some(session_path.to_string_lossy().to_string());
            }
        }

        None
    }

    /// 获取项目的编码目录名
    ///
    /// 优先从缓存获取
    pub fn get_encoded_dir_name(&mut self, project_path: &str) -> Option<String> {
        // 先查缓存
        if let Some(encoded) = self.encoded_dir_cache.get(project_path) {
            return Some(encoded.clone());
        }

        // 缓存未命中，刷新项目列表
        let _ = self.list_projects(None);

        // 再次查缓存
        self.encoded_dir_cache.get(project_path).cloned()
    }

    /// 读取会话消息（支持分页）
    pub fn read_messages(
        &self,
        session_path: &str,
        limit: usize,
        offset: usize,
        order: Order,
    ) -> Option<MessagesResult> {
        // 构造临时 SessionMeta
        let session_id = std::path::Path::new(session_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let meta = SessionMeta {
            id: session_id,
            source: Source::Claude,
            channel: Some("code".to_string()),
            project_path: String::new(),
            project_name: None,
            encoded_dir_name: None,
            session_path: Some(session_path.to_string()),
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
            parent_session_id: None,
            session_type: None,
            continuation_from: None,
        };

        let result = self.adapter.parse_session(&meta).ok()??;

        let mut all_messages = result.messages;
        let total = all_messages.len();

        // 排序
        if order == Order::Desc {
            all_messages.reverse();
        }

        // 分页
        let start = offset.min(total);
        let end = (offset + limit).min(total);
        let messages: Vec<_> = all_messages[start..end].to_vec();
        let has_more = end < total;

        Some(MessagesResult {
            messages,
            total,
            has_more,
        })
    }

    /// 读取原始 JSONL 消息（不做格式转换）
    pub fn read_messages_raw(
        &self,
        session_path: &str,
        limit: usize,
        offset: usize,
        order: Order,
    ) -> Option<RawMessagesResult> {
        let file = fs::File::open(session_path).ok()?;
        let reader = BufReader::new(file);

        let mut all_messages: Vec<serde_json::Value> = Vec::new();
        for line in reader.lines() {
            let line = line.ok()?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                all_messages.push(json);
            }
        }

        let total = all_messages.len();

        // 排序
        if order == Order::Desc {
            all_messages.reverse();
        }

        // 分页
        let start = offset.min(total);
        let end = (offset + limit).min(total);
        let messages: Vec<_> = all_messages[start..end].to_vec();
        let has_more = end < total;

        Some(RawMessagesResult {
            messages,
            total,
            has_more,
        })
    }

    /// 解析完整会话
    pub fn parse_session(&self, meta: &SessionMeta) -> Option<ParseResult> {
        self.adapter.parse_session(meta).ok()?
    }

    /// 解析 JSONL 文件（用于索引）
    ///
    /// 返回 IndexableSession，包含正确解析的项目路径
    pub fn parse_jsonl_for_index(&self, jsonl_path: &str) -> Option<crate::IndexableSession> {
        ClaudeAdapter::parse_session_from_path(jsonl_path).ok()?
    }

    /// 计算会话 Metrics
    pub fn calculate_metrics(&self, meta: &SessionMeta) -> Option<SessionMetrics> {
        let result = self.parse_session(meta)?;

        let user_count = result
            .messages
            .iter()
            .filter(|m| m.message_type == MessageType::User)
            .count();
        let assistant_count = result
            .messages
            .iter()
            .filter(|m| m.message_type == MessageType::Assistant)
            .count();

        // 估算 token 数（简单按字符数 / 4）
        let total_chars: usize = result.messages.iter().map(|m| m.content.full.len()).sum();
        let estimated_tokens = total_chars / 4;

        // 计算时长
        let duration = if let (Some(first), Some(last)) = (&result.created_at, &result.updated_at) {
            if let (Ok(first_dt), Ok(last_dt)) = (
                chrono::DateTime::parse_from_rfc3339(first),
                chrono::DateTime::parse_from_rfc3339(last),
            ) {
                Some((last_dt - first_dt).num_seconds().max(0) as u64)
            } else {
                None
            }
        } else {
            None
        };

        Some(SessionMetrics {
            message_count: result.messages.len(),
            user_message_count: user_count,
            assistant_message_count: assistant_count,
            estimated_tokens,
            duration_seconds: duration,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_project_name() {
        assert_eq!(
            SessionReader::extract_project_name("/Users/xxx/project"),
            "project"
        );
        assert_eq!(SessionReader::extract_project_name("/a/b/c/d"), "d");
    }

    #[test]
    fn test_compute_session_path() {
        let projects_path = PathBuf::from("/home/user/.claude/projects");
        let encoded_dir = "-Users-xxx-project";
        let session_id = "abc123";

        let result = compute_session_path(&projects_path, encoded_dir, session_id);
        assert_eq!(
            result,
            PathBuf::from("/home/user/.claude/projects/-Users-xxx-project/abc123.jsonl")
        );
    }

    #[test]
    fn test_compute_session_path_with_uuid() {
        let projects_path = PathBuf::from("/Users/test/.claude/projects");
        let encoded_dir = "-Users-test-Desktop-myproject";
        let session_id = "550e8400-e29b-41d4-a716-446655440000";

        let result = compute_session_path(&projects_path, encoded_dir, session_id);
        assert_eq!(
            result,
            PathBuf::from("/Users/test/.claude/projects/-Users-test-Desktop-myproject/550e8400-e29b-41d4-a716-446655440000.jsonl")
        );
    }
}
