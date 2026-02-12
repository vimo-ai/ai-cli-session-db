//! 数据库 Schema 定义

/// 表定义 SQL（不包含索引）
pub const TABLES_SQL: &str = r#"
-- Projects 表
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'claude',
    encoded_dir_name TEXT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000)
);

-- Sessions 表
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL UNIQUE,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    message_count INTEGER NOT NULL DEFAULT 0,
    last_message_at INTEGER,  -- 用于增量扫描的检查点 (毫秒时间戳)
    -- 会话元数据 (来自 ai-cli-session-collector::SessionMeta)
    cwd TEXT,                 -- 工作目录
    model TEXT,               -- 默认模型
    channel TEXT,             -- 渠道 (cli/code/gui)
    -- 增量检测字段
    file_mtime INTEGER,       -- 文件修改时间戳 (毫秒)
    file_size INTEGER,        -- 文件大小 (字节)
    file_offset INTEGER DEFAULT 0,  -- 文件读取偏移量 (字节)
    file_inode INTEGER,       -- 文件 inode (用于检测文件替换)
    encoded_dir_name TEXT,    -- 编码后的目录名
    -- 额外元信息
    meta TEXT,                -- 额外元信息 (JSON)
    -- 时间戳
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000)
);

-- Messages 表
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    uuid TEXT NOT NULL UNIQUE,  -- 消息的唯一 ID，用于去重
    type TEXT NOT NULL,         -- "user" | "assistant" | "tool"
    content_text TEXT NOT NULL, -- 纯对话文本（用于向量化）
    content_full TEXT NOT NULL, -- 完整格式化内容（用于 FTS，包含 tool_use 等）
    timestamp INTEGER NOT NULL,
    sequence INTEGER NOT NULL,
    source TEXT DEFAULT 'claude',   -- 来源: claude, codex, opencode
    channel TEXT,                   -- 渠道: code, chat
    model TEXT,                     -- 使用的模型
    tool_call_id TEXT,              -- Tool call ID
    tool_name TEXT,                 -- Tool 名称
    tool_args TEXT,                 -- Tool 参数
    raw TEXT,                       -- 原始 JSONL 数据（用于重解析）
    vector_indexed INTEGER DEFAULT 0, -- 是否已向量索引 (0=未索引, 1=已索引)
    approval_status TEXT,           -- 审批状态: pending, approved, rejected, timeout, NULL
    approval_resolved_at INTEGER,   -- 审批解决时间戳（毫秒）

    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

-- Talks 表 (Compact 摘要)
CREATE TABLE IF NOT EXISTS talks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    talk_id TEXT NOT NULL,          -- Talk 唯一标识
    summary_l2 TEXT NOT NULL,       -- L2 摘要（每个 Talk 的摘要）
    summary_l3 TEXT,                -- L3 摘要（Session 级别汇总）
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
    UNIQUE(session_id, talk_id),
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

-- Session Relations 表（会话间关系：parent→child）
CREATE TABLE IF NOT EXISTS session_relations (
    parent_session_id TEXT NOT NULL,
    child_session_id  TEXT NOT NULL,
    relation_type     TEXT NOT NULL,  -- subagent / rollout
    source            TEXT NOT NULL,  -- claude / codex / ...
    created_at        INTEGER NOT NULL DEFAULT (strftime('%s','now')*1000),
    PRIMARY KEY (parent_session_id, child_session_id, relation_type),
    CHECK (parent_session_id <> child_session_id)
);
"#;

/// 索引定义 SQL
pub const INDEXES_SQL: &str = r#"
-- 索引
CREATE INDEX IF NOT EXISTS idx_projects_path ON projects(path);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project_id);
CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_last_message ON sessions(last_message_at);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_uuid ON messages(uuid);
CREATE INDEX IF NOT EXISTS idx_messages_type ON messages(type);
CREATE INDEX IF NOT EXISTS idx_messages_vector_indexed ON messages(vector_indexed);
CREATE INDEX IF NOT EXISTS idx_messages_approval_status ON messages(approval_status) WHERE approval_status IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_messages_approval_pending ON messages(session_id, approval_status) WHERE approval_status = 'pending';
CREATE INDEX IF NOT EXISTS idx_talks_session ON talks(session_id);
CREATE INDEX IF NOT EXISTS idx_talks_talk_id ON talks(talk_id);
CREATE INDEX IF NOT EXISTS idx_session_relations_parent ON session_relations(parent_session_id);
CREATE INDEX IF NOT EXISTS idx_session_relations_child ON session_relations(child_session_id);
"#;

/// FTS5 全文搜索 Schema (索引 content_full)
pub const FTS_SCHEMA_SQL: &str = r#"
-- 全文搜索虚拟表 (带触发器自动维护)
-- 使用 content_full 进行 FTS 索引，包含完整对话内容（含 tool_use/tool_result）
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content_full,
    content='messages',
    content_rowid='id',
    tokenize='unicode61'
);

-- FTS 触发器
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content_full) VALUES (new.id, new.content_full);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content_full) VALUES('delete', old.id, old.content_full);
END;

CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content_full) VALUES('delete', old.id, old.content_full);
    INSERT INTO messages_fts(rowid, content_full) VALUES (new.id, new.content_full);
END;
"#;

/// 兼容旧代码：核心 Schema SQL（表 + 索引）
#[deprecated(note = "请使用 TABLES_SQL + INDEXES_SQL")]
pub const SCHEMA_SQL: &str = r#"
-- Projects 表
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'claude',
    encoded_dir_name TEXT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000)
);

-- Sessions 表
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL UNIQUE,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    message_count INTEGER NOT NULL DEFAULT 0,
    last_message_at INTEGER,
    cwd TEXT,
    model TEXT,
    channel TEXT,
    file_mtime INTEGER,
    file_size INTEGER,
    file_offset INTEGER DEFAULT 0,
    file_inode INTEGER,
    encoded_dir_name TEXT,
    meta TEXT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000)
);

-- Messages 表
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    uuid TEXT NOT NULL UNIQUE,
    type TEXT NOT NULL,
    content_text TEXT NOT NULL,
    content_full TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    sequence INTEGER NOT NULL,
    source TEXT DEFAULT 'claude',
    channel TEXT,
    model TEXT,
    tool_call_id TEXT,
    tool_name TEXT,
    tool_args TEXT,
    raw TEXT,
    vector_indexed INTEGER DEFAULT 0,
    approval_status TEXT,
    approval_resolved_at INTEGER,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

-- 索引
CREATE INDEX IF NOT EXISTS idx_projects_path ON projects(path);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project_id);
CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_last_message ON sessions(last_message_at);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_uuid ON messages(uuid);
CREATE INDEX IF NOT EXISTS idx_messages_type ON messages(type);
CREATE INDEX IF NOT EXISTS idx_messages_vector_indexed ON messages(vector_indexed);
CREATE INDEX IF NOT EXISTS idx_messages_approval_status ON messages(approval_status) WHERE approval_status IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_messages_approval_pending ON messages(session_id, approval_status) WHERE approval_status = 'pending';

-- Talks 表
CREATE TABLE IF NOT EXISTS talks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    talk_id TEXT NOT NULL,
    summary_l2 TEXT NOT NULL,
    summary_l3 TEXT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
    UNIQUE(session_id, talk_id),
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

-- Session Relations 表（会话间关系：parent→child）
CREATE TABLE IF NOT EXISTS session_relations (
    parent_session_id TEXT NOT NULL,
    child_session_id  TEXT NOT NULL,
    relation_type     TEXT NOT NULL,
    source            TEXT NOT NULL,
    created_at        INTEGER NOT NULL DEFAULT (strftime('%s','now')*1000),
    PRIMARY KEY (parent_session_id, child_session_id, relation_type),
    CHECK (parent_session_id <> child_session_id)
);

CREATE INDEX IF NOT EXISTS idx_talks_session ON talks(session_id);
CREATE INDEX IF NOT EXISTS idx_talks_talk_id ON talks(talk_id);
CREATE INDEX IF NOT EXISTS idx_session_relations_parent ON session_relations(parent_session_id);
CREATE INDEX IF NOT EXISTS idx_session_relations_child ON session_relations(child_session_id);
"#;

/// 获取完整 Schema (根据 feature flags)
#[deprecated(note = "请使用 full_schema_parts")]
pub fn full_schema(fts: bool) -> String {
    let mut sql = TABLES_SQL.to_string();
    sql.push_str(INDEXES_SQL);

    if fts {
        sql.push_str(FTS_SCHEMA_SQL);
    }

    sql
}

/// 获取 Schema 各部分（用于分步执行）
pub fn full_schema_parts(fts: bool) -> (String, String, Option<String>) {
    let tables = TABLES_SQL.to_string();
    let indexes = INDEXES_SQL.to_string();
    let fts_sql = if fts {
        Some(FTS_SCHEMA_SQL.to_string())
    } else {
        None
    };
    (tables, indexes, fts_sql)
}
