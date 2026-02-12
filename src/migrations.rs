//! 数据库迁移模块
//!
//! 采用幂等迁移策略：
//! - 不依赖版本号做 DDL，检查实际状态，缺什么补什么
//! - 表用 CREATE TABLE IF NOT EXISTS
//! - 列用 ensure_column 检查并补充
//! - 清理旧的 schema_migrations 系统

use crate::schema;
use rusqlite::{Connection, Result as SqliteResult};
use tracing::info;

/// 当前 schema 版本（用于未来可能的数据迁移）
const SCHEMA_VERSION: i32 = 1;

/// 确保数据库 schema 完整（幂等）
///
/// 该函数可以安全地多次调用，会自动检查并补充缺失的表和列。
/// 支持所有用户场景：新用户、老用户（V0/V1/V2）、脏迁移、备份恢复。
pub fn ensure_schema(conn: &Connection) -> SqliteResult<()> {
    info!("确保数据库 schema 完整...");

    let fts = cfg!(feature = "fts");
    let (tables_sql, indexes_sql, fts_sql) = schema::full_schema_parts(fts);

    // 1. 创建表（IF NOT EXISTS，幂等）
    conn.execute_batch(&tables_sql)?;
    info!("表结构已确保");

    // 2. 补充可能缺失的列（幂等）
    // 注意：必须在创建索引之前，因为索引可能依赖这些列
    ensure_sessions_columns(conn)?;
    ensure_messages_columns(conn)?;
    info!("列结构已确保");

    // 3. 创建索引（IF NOT EXISTS，幂等）
    conn.execute_batch(&indexes_sql)?;
    info!("索引已确保");

    // 4. FTS（如果启用）
    if let Some(fts) = fts_sql {
        conn.execute_batch(&fts)?;
        info!("FTS 已确保");
    }

    // 5. 清理旧的迁移系统
    cleanup_old_migration_system(conn)?;

    // 6. 更新 user_version（用于未来可能的数据迁移）
    let current_version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current_version < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        info!("user_version 更新: {} -> {}", current_version, SCHEMA_VERSION);
    }

    info!("数据库 schema 确保完成");
    Ok(())
}

/// 检查列是否存在
fn column_exists(conn: &Connection, table: &str, column: &str) -> SqliteResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let columns = stmt.query_map([], |row| {
        let col_name: String = row.get(1)?;
        Ok(col_name)
    })?;

    for col_name in columns.flatten() {
        if col_name == column {
            return Ok(true);
        }
    }

    Ok(false)
}

/// 检查表是否存在
fn table_exists(conn: &Connection, table: &str) -> SqliteResult<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// 幂等添加列
///
/// 如果列不存在则添加，存在则跳过。
/// 注意：SQLite ALTER TABLE ADD COLUMN 的限制：
/// - NOT NULL 列必须有默认值
/// - 不能加 PRIMARY KEY
fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> SqliteResult<()> {
    // 如果表不存在，跳过（表会由 CREATE TABLE IF NOT EXISTS 创建）
    if !table_exists(conn, table)? {
        return Ok(());
    }

    if !column_exists(conn, table, column)? {
        let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, definition);
        conn.execute(&sql, [])?;
        info!("补充列: {}.{}", table, column);
    }

    Ok(())
}

/// 确保 sessions 表的所有列存在
fn ensure_sessions_columns(conn: &Connection) -> SqliteResult<()> {
    // 基础字段（可能在老表中缺失）
    ensure_column(conn, "sessions", "message_count", "INTEGER DEFAULT 0")?;
    ensure_column(conn, "sessions", "last_message_at", "INTEGER")?;
    ensure_column(conn, "sessions", "cwd", "TEXT")?;
    ensure_column(conn, "sessions", "model", "TEXT")?;
    ensure_column(conn, "sessions", "channel", "TEXT")?;
    ensure_column(conn, "sessions", "file_mtime", "INTEGER")?;
    ensure_column(conn, "sessions", "file_size", "INTEGER")?;
    ensure_column(conn, "sessions", "file_offset", "INTEGER DEFAULT 0")?;
    ensure_column(conn, "sessions", "file_inode", "INTEGER")?;
    ensure_column(conn, "sessions", "encoded_dir_name", "TEXT")?;
    ensure_column(conn, "sessions", "meta", "TEXT")?;
    ensure_column(
        conn,
        "sessions",
        "created_at",
        "INTEGER DEFAULT (strftime('%s','now')*1000)",
    )?;
    ensure_column(
        conn,
        "sessions",
        "updated_at",
        "INTEGER DEFAULT (strftime('%s','now')*1000)",
    )?;

    // Session Chain Linking 新增列
    ensure_column(conn, "sessions", "session_type", "TEXT")?;
    ensure_column(conn, "sessions", "source", "TEXT")?;

    Ok(())
}

/// 确保 messages 表的所有列存在
fn ensure_messages_columns(conn: &Connection) -> SqliteResult<()> {
    // 基础字段（可能在老表中缺失）
    ensure_column(conn, "messages", "source", "TEXT DEFAULT 'claude'")?;
    ensure_column(conn, "messages", "channel", "TEXT")?;
    ensure_column(conn, "messages", "model", "TEXT")?;
    ensure_column(conn, "messages", "tool_call_id", "TEXT")?;
    ensure_column(conn, "messages", "tool_name", "TEXT")?;
    ensure_column(conn, "messages", "tool_args", "TEXT")?;
    ensure_column(conn, "messages", "raw", "TEXT")?;
    ensure_column(conn, "messages", "vector_indexed", "INTEGER DEFAULT 0")?;
    ensure_column(conn, "messages", "approval_status", "TEXT")?;
    ensure_column(conn, "messages", "approval_resolved_at", "INTEGER")?;

    Ok(())
}

/// 清理旧的迁移系统
///
/// 删除旧的 schema_migrations 表，因为新系统不再需要它。
fn cleanup_old_migration_system(conn: &Connection) -> SqliteResult<()> {
    if table_exists(conn, "schema_migrations")? {
        conn.execute("DROP TABLE schema_migrations", [])?;
        info!("已清理旧的 schema_migrations 表");
    }
    Ok(())
}

// ==================== 兼容性导出 ====================

/// 兼容旧代码的入口（重定向到 ensure_schema）
#[deprecated(note = "请使用 ensure_schema")]
pub fn run_migrations(conn: &Connection) -> SqliteResult<()> {
    ensure_schema(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_ensure_schema_new_database() {
        // 新数据库
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();

        // 验证表存在
        assert!(table_exists(&conn, "projects").unwrap());
        assert!(table_exists(&conn, "sessions").unwrap());
        assert!(table_exists(&conn, "messages").unwrap());
        assert!(table_exists(&conn, "talks").unwrap());

        // 验证关键列存在
        assert!(column_exists(&conn, "sessions", "file_offset").unwrap());
        assert!(column_exists(&conn, "sessions", "file_inode").unwrap());
        assert!(column_exists(&conn, "messages", "approval_status").unwrap());
    }

    #[test]
    fn test_ensure_schema_old_database() {
        // 模拟老数据库（只有基础表）
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                uuid TEXT NOT NULL UNIQUE,
                type TEXT NOT NULL,
                content_text TEXT NOT NULL,
                content_full TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                sequence INTEGER NOT NULL
            );
            CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL UNIQUE,
                project_id INTEGER NOT NULL
            );
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );
            INSERT INTO schema_migrations VALUES (1, 1234567890);
            "#,
        )
        .unwrap();

        // 运行幂等迁移
        ensure_schema(&conn).unwrap();

        // 验证列被补充
        assert!(column_exists(&conn, "sessions", "file_offset").unwrap());
        assert!(column_exists(&conn, "sessions", "file_inode").unwrap());
        assert!(column_exists(&conn, "sessions", "cwd").unwrap());
        assert!(column_exists(&conn, "messages", "approval_status").unwrap());
        assert!(column_exists(&conn, "messages", "source").unwrap());

        // 验证旧迁移系统被清理
        assert!(!table_exists(&conn, "schema_migrations").unwrap());
    }

    #[test]
    fn test_ensure_schema_idempotent() {
        let conn = Connection::open_in_memory().unwrap();

        // 多次运行应该是幂等的
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();

        // 验证状态正确
        assert!(table_exists(&conn, "sessions").unwrap());
        assert!(column_exists(&conn, "sessions", "file_offset").unwrap());
    }

    #[test]
    fn test_ensure_schema_dirty_migration() {
        // 模拟脏迁移：schema_migrations 记录版本 2，但实际列缺失
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                uuid TEXT NOT NULL UNIQUE,
                type TEXT NOT NULL,
                content_text TEXT NOT NULL,
                content_full TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                sequence INTEGER NOT NULL
            );
            CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL UNIQUE,
                project_id INTEGER NOT NULL
            );
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );
            INSERT INTO schema_migrations VALUES (1, 1234567890);
            INSERT INTO schema_migrations VALUES (2, 1234567891);
            "#,
        )
        .unwrap();

        // 验证列确实缺失
        assert!(!column_exists(&conn, "sessions", "file_offset").unwrap());

        // 运行幂等迁移，应该自动补充缺失的列
        ensure_schema(&conn).unwrap();

        // 验证列被补充
        assert!(column_exists(&conn, "sessions", "file_offset").unwrap());
        assert!(column_exists(&conn, "sessions", "file_inode").unwrap());
    }
}
