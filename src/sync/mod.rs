//! 同步模块 - 本地 → 服务端增量推送
//!
//! 核心原则：
//! - 对 ai-cli-session.db 和 compact.db 只读（`PRAGMA query_only=ON`）
//! - 同步状态写入独立的 sync.db
//! - 推送前经过敏感词过滤和项目白名单

mod db;
mod filter;
mod client;
mod worker;

pub use db::SyncDb;
pub use filter::{SensitiveWordFilter, ProjectFilter, FilterConfig, FilterMode};
pub use client::{SyncClient, SyncStats};
pub use worker::SyncWorker;

use serde::{Deserialize, Serialize};

/// 同步配置
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    pub enabled: bool,
    pub server: String,
    pub api_key: String,
    pub ca_cert: Option<String>,
    pub interval_seconds: u64,
    pub include_projects: Vec<String>,
    pub exclude_projects: Vec<String>,
    pub sync_raw: bool,
    pub sync_vectors: bool,
    pub embedding_model: String,
    pub encryption: String,
    pub batch_size: usize,
    pub compress: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server: String::new(),
            api_key: String::new(),
            ca_cert: None,
            interval_seconds: 300,
            include_projects: Vec::new(),
            exclude_projects: Vec::new(),
            sync_raw: false,
            sync_vectors: true,
            embedding_model: "nomic-embed-text".to_string(),
            encryption: "none".to_string(),
            batch_size: 500,
            compress: true,
        }
    }
}

/// 推送批次
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncBatch {
    pub projects: Vec<SyncProject>,
    pub sessions: Vec<SyncSession>,
    pub messages: Vec<SyncMessage>,
    pub session_relations: Vec<SyncSessionRelation>,
    pub continuation_chains: Vec<SyncContinuationChain>,
    pub chain_nodes: Vec<SyncChainNode>,
    pub talks: Vec<SyncTalk>,
}

/// 推送请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPushRequest {
    pub device_id: String,
    pub encryption: String,
    pub batch: SyncBatch,
    pub cursors: std::collections::HashMap<String, SyncCursor>,
}

/// 推送响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPushResponse {
    pub accepted: u64,
    pub skipped: u64,
    pub server_cursors: std::collections::HashMap<String, SyncCursor>,
}

/// 同步游标
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncCursor {
    pub last_sequence: i64,
    pub last_timestamp: i64,
}

// ==================== 同步数据类型 ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncProject {
    pub path: String,
    pub name: String,
    pub source: String,
    pub repo_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncSession {
    pub session_id: String,
    pub project_path: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub channel: Option<String>,
    pub message_count: i64,
    pub last_message_at: Option<i64>,
    pub session_type: Option<String>,
    pub source: Option<String>,
    pub meta: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncMessage {
    pub uuid: String,
    pub session_id: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub content_text: String,
    pub content_full: String,
    pub timestamp: i64,
    pub sequence: i64,
    pub source: Option<String>,
    pub channel: Option<String>,
    pub model: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_args: Option<String>,
    pub raw: Option<String>,
    pub approval_status: Option<String>,
    pub approval_resolved_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncSessionRelation {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub relation_type: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncContinuationChain {
    pub chain_id: String,
    pub root_session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncChainNode {
    pub session_id: String,
    pub chain_id: String,
    pub prev_session_id: Option<String>,
    pub depth: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncTalk {
    pub session_id: String,
    pub talk_id: String,
    pub summary_l2: String,
    pub summary_l3: Option<String>,
}
