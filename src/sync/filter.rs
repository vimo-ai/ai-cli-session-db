//! 推送前过滤 - 敏感词替换 + 项目白名单
//!
//! 过滤只在 sync push 管道中发生，本地 DB 始终保留原始数据。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;

use super::{SyncBatch, SyncMessage};

// ==================== 敏感词过滤 ====================

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    pub enabled: bool,
    pub sensitive_words_file: String,
    pub mode: FilterMode,
    pub remote_list_url: Option<String>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sensitive_words_file: String::new(),
            mode: FilterMode::Redact,
            remote_list_url: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterMode {
    Redact,
    Block,
    Tag,
}

pub struct SensitiveWordFilter {
    automaton: aho_corasick::AhoCorasick,
    mode: FilterMode,
    patterns_count: usize,
}

impl SensitiveWordFilter {
    pub fn from_file(path: &Path, mode: FilterMode) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("无法读取敏感词文件: {}", path.display()))?;
        Self::from_str(&content, mode)
    }

    pub fn from_str(content: &str, mode: FilterMode) -> Result<Self> {
        let patterns: Vec<&str> = content
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();

        let patterns_count = patterns.len();

        let automaton = aho_corasick::AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(&patterns)
            .context("构建 Aho-Corasick 自动机失败")?;

        Ok(Self {
            automaton,
            mode,
            patterns_count,
        })
    }

    pub fn patterns_count(&self) -> usize {
        self.patterns_count
    }

    pub fn contains_sensitive(&self, text: &str) -> bool {
        self.automaton.is_match(text)
    }

    pub fn redact(&self, text: &str) -> String {
        self.automaton.replace_all(text, &vec!["***"; self.patterns_count])
    }

    pub fn filter_batch(&self, batch: &mut SyncBatch) {
        match self.mode {
            FilterMode::Redact => self.redact_batch(batch),
            FilterMode::Block => self.block_batch(batch),
            FilterMode::Tag => {} // tag 模式不修改内容，标记在 server 端处理
        }
    }

    fn redact_batch(&self, batch: &mut SyncBatch) {
        for msg in &mut batch.messages {
            msg.content_text = self.redact(&msg.content_text);
            msg.content_full = self.redact(&msg.content_full);
            if let Some(ref raw) = msg.raw {
                msg.raw = Some(self.redact(raw));
            }
            if let Some(ref args) = msg.tool_args {
                msg.tool_args = Some(self.redact(args));
            }
        }

        for talk in &mut batch.talks {
            talk.summary_l2 = self.redact(&talk.summary_l2);
            if let Some(ref l3) = talk.summary_l3 {
                talk.summary_l3 = Some(self.redact(l3));
            }
        }
    }

    fn block_batch(&self, batch: &mut SyncBatch) {
        batch.messages.retain(|msg| !self.is_message_sensitive(msg));
        batch.talks.retain(|talk| {
            !self.contains_sensitive(&talk.summary_l2)
                && !talk.summary_l3.as_ref().is_some_and(|l3| self.contains_sensitive(l3))
        });
    }

    fn is_message_sensitive(&self, msg: &SyncMessage) -> bool {
        self.contains_sensitive(&msg.content_text)
            || self.contains_sensitive(&msg.content_full)
            || msg.raw.as_ref().is_some_and(|r| self.contains_sensitive(r))
            || msg.tool_args.as_ref().is_some_and(|a| self.contains_sensitive(a))
    }
}

// ==================== 项目白名单 ====================

pub struct ProjectFilter {
    include: Option<globset::GlobSet>,
    exclude: Option<globset::GlobSet>,
}

impl ProjectFilter {
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        let include = if include.is_empty() {
            None
        } else {
            let mut builder = globset::GlobSetBuilder::new();
            for pattern in include {
                builder.add(
                    globset::Glob::new(pattern)
                        .with_context(|| format!("无效的 include glob: {pattern}"))?,
                );
            }
            Some(builder.build().context("构建 include GlobSet 失败")?)
        };

        let exclude = if exclude.is_empty() {
            None
        } else {
            let mut builder = globset::GlobSetBuilder::new();
            for pattern in exclude {
                builder.add(
                    globset::Glob::new(pattern)
                        .with_context(|| format!("无效的 exclude glob: {pattern}"))?,
                );
            }
            Some(builder.build().context("构建 exclude GlobSet 失败")?)
        };

        Ok(Self { include, exclude })
    }

    pub fn is_allowed(&self, project_path: &str) -> bool {
        if let Some(ref exclude) = self.exclude {
            if exclude.is_match(project_path) {
                return false;
            }
        }

        match self.include {
            Some(ref include) => include.is_match(project_path),
            None => true,
        }
    }

    pub fn filter_batch(&self, batch: &mut SyncBatch) {
        let allowed_paths: std::collections::HashSet<String> = batch
            .projects
            .iter()
            .filter(|p| self.is_allowed(&p.path))
            .map(|p| p.path.clone())
            .collect();

        batch.projects.retain(|p| allowed_paths.contains(&p.path));

        let allowed_sessions: std::collections::HashSet<String> = batch
            .sessions
            .iter()
            .filter(|s| allowed_paths.contains(&s.project_path))
            .map(|s| s.session_id.clone())
            .collect();

        batch.sessions.retain(|s| allowed_paths.contains(&s.project_path));
        batch.messages.retain(|m| allowed_sessions.contains(&m.session_id));
        batch.talks.retain(|t| allowed_sessions.contains(&t.session_id));
        batch.session_relations.retain(|r| {
            allowed_sessions.contains(&r.parent_session_id)
                || allowed_sessions.contains(&r.child_session_id)
        });
        batch.continuation_chains.retain(|c| {
            allowed_sessions.contains(&c.root_session_id)
        });
        batch.chain_nodes.retain(|n| {
            allowed_sessions.contains(&n.session_id)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sensitive_word_filter_redact() {
        let filter = SensitiveWordFilter::from_str("password\nsecret_key\napi_token", FilterMode::Redact).unwrap();

        assert_eq!(filter.patterns_count(), 3);
        assert!(filter.contains_sensitive("my password is 123"));
        assert!(!filter.contains_sensitive("nothing here"));
        assert_eq!(filter.redact("my password is 123"), "my *** is 123");
        assert_eq!(
            filter.redact("use secret_key and api_token"),
            "use *** and ***"
        );
    }

    #[test]
    fn test_sensitive_word_filter_comments() {
        let filter = SensitiveWordFilter::from_str(
            "# comment line\npassword\n  \n# another comment\nsecret",
            FilterMode::Redact,
        )
        .unwrap();
        assert_eq!(filter.patterns_count(), 2);
    }

    #[test]
    fn test_sensitive_word_filter_block() {
        let filter = SensitiveWordFilter::from_str("secret", FilterMode::Block).unwrap();

        let mut batch = make_test_batch();
        batch.messages.push(make_msg("msg-1", "contains secret data"));
        batch.messages.push(make_msg("msg-2", "safe message"));

        filter.filter_batch(&mut batch);
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].uuid, "msg-2");
    }

    #[test]
    fn test_sensitive_word_filter_redact_batch() {
        let filter = SensitiveWordFilter::from_str("secret", FilterMode::Redact).unwrap();

        let mut batch = make_test_batch();
        batch.messages.push(make_msg("msg-1", "my secret value"));

        filter.filter_batch(&mut batch);
        assert_eq!(batch.messages[0].content_text, "my *** value");
        assert_eq!(batch.messages[0].content_full, "my *** value");
    }

    #[test]
    fn test_project_filter_include() {
        let filter = ProjectFilter::new(
            &["/Users/*/code/*".to_string()],
            &[],
        )
        .unwrap();

        assert!(filter.is_allowed("/Users/alice/code/ETerm"));
        assert!(!filter.is_allowed("/Users/alice/scratch/test"));
    }

    #[test]
    fn test_project_filter_exclude() {
        let filter = ProjectFilter::new(
            &[],
            &["**/playground-*".to_string(), "**/scratch".to_string()],
        )
        .unwrap();

        assert!(filter.is_allowed("/Users/alice/code/ETerm"));
        assert!(!filter.is_allowed("/Users/alice/playground-test"));
        assert!(!filter.is_allowed("/Users/alice/scratch"));
    }

    #[test]
    fn test_project_filter_exclude_overrides_include() {
        let filter = ProjectFilter::new(
            &["/Users/*/code/*".to_string()],
            &["**/code/scratch".to_string()],
        )
        .unwrap();

        assert!(filter.is_allowed("/Users/alice/code/ETerm"));
        assert!(!filter.is_allowed("/Users/alice/code/scratch"));
    }

    #[test]
    fn test_project_filter_empty_allows_all() {
        let filter = ProjectFilter::new(&[], &[]).unwrap();
        assert!(filter.is_allowed("/any/path/here"));
    }

    #[test]
    fn test_project_filter_batch() {
        let filter = ProjectFilter::new(
            &["/Users/*/code/*".to_string()],
            &[],
        )
        .unwrap();

        let mut batch = make_test_batch();
        batch.projects.push(super::super::SyncProject {
            path: "/Users/alice/code/ETerm".to_string(),
            name: "ETerm".to_string(),
            source: "claude".to_string(),
            repo_url: None,
        });
        batch.projects.push(super::super::SyncProject {
            path: "/Users/alice/scratch/test".to_string(),
            name: "test".to_string(),
            source: "claude".to_string(),
            repo_url: None,
        });
        batch.sessions.push(super::super::SyncSession {
            session_id: "sess-1".to_string(),
            project_path: "/Users/alice/code/ETerm".to_string(),
            cwd: None, model: None, channel: None,
            message_count: 0, last_message_at: None,
            session_type: None, source: None, meta: None,
            created_at: 0, updated_at: 0,
        });
        batch.sessions.push(super::super::SyncSession {
            session_id: "sess-2".to_string(),
            project_path: "/Users/alice/scratch/test".to_string(),
            cwd: None, model: None, channel: None,
            message_count: 0, last_message_at: None,
            session_type: None, source: None, meta: None,
            created_at: 0, updated_at: 0,
        });
        batch.messages.push(make_msg_with_session("msg-1", "sess-1", "hello"));
        batch.messages.push(make_msg_with_session("msg-2", "sess-2", "world"));

        filter.filter_batch(&mut batch);

        assert_eq!(batch.projects.len(), 1);
        assert_eq!(batch.sessions.len(), 1);
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].session_id, "sess-1");
    }

    // ==================== helpers ====================

    fn make_test_batch() -> SyncBatch {
        SyncBatch {
            projects: vec![],
            sessions: vec![],
            messages: vec![],
            session_relations: vec![],
            continuation_chains: vec![],
            chain_nodes: vec![],
            talks: vec![],
        }
    }

    fn make_msg(uuid: &str, content: &str) -> super::super::SyncMessage {
        make_msg_with_session(uuid, "sess-default", content)
    }

    fn make_msg_with_session(uuid: &str, session_id: &str, content: &str) -> super::super::SyncMessage {
        super::super::SyncMessage {
            uuid: uuid.to_string(),
            session_id: session_id.to_string(),
            msg_type: "assistant".to_string(),
            content_text: content.to_string(),
            content_full: content.to_string(),
            timestamp: 0,
            sequence: 0,
            source: None,
            channel: None,
            model: None,
            tool_call_id: None,
            tool_name: None,
            tool_args: None,
            raw: None,
            approval_status: None,
            approval_resolved_at: None,
        }
    }
}
