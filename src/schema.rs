//! 数据库 Schema 定义

/// 核心 Schema SQL
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
    last_message_at INTEGER,  -- 用于增量扫描的检查点 (毫秒时间戳)
    -- 会话元数据 (来自 ai-cli-session-collector::SessionMeta)
    cwd TEXT,                 -- 工作目录
    model TEXT,               -- 默认模型
    channel TEXT,             -- 渠道 (cli/code/gui)
    -- 增量检测字段
    file_mtime INTEGER,       -- 文件修改时间戳 (毫秒)
    file_size INTEGER,        -- 文件大小 (字节)
    -- 额外元信息
    encoded_dir_name TEXT,    -- Claude 编码目录名
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
    source TEXT DEFAULT 'claude',   -- 来源: claude, codex
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

/// Writer 协调表 Schema
pub const COORDINATION_SCHEMA_SQL: &str = r#"
-- Writer 注册表 (协调用)
CREATE TABLE IF NOT EXISTS writer_registry (
    id INTEGER PRIMARY KEY CHECK (id = 1),  -- 只能有一行
    writer_type TEXT NOT NULL,
    writer_id TEXT NOT NULL,      -- UUID，每次启动生成新的
    priority INTEGER NOT NULL,
    heartbeat INTEGER NOT NULL,   -- 毫秒时间戳
    registered_at INTEGER NOT NULL
);
"#;

/// 获取完整 Schema (根据 feature flags)
pub fn full_schema(fts: bool, coordination: bool) -> String {
    let mut sql = SCHEMA_SQL.to_string();

    if fts {
        sql.push_str(FTS_SCHEMA_SQL);
    }

    if coordination {
        sql.push_str(COORDINATION_SCHEMA_SQL);
    }

    sql
}
