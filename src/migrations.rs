//! 数据库迁移模块

use rusqlite::{Connection, Result as SqliteResult};
use tracing::{info, warn};

/// 迁移版本
const MIGRATION_VERSION: i64 = 2;

/// 初始化迁移系统
pub fn initialize_migrations(conn: &Connection) -> SqliteResult<()> {
    // 创建迁移版本表
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )
        "#,
        [],
    )?;

    info!("Migration system initialized");
    Ok(())
}

/// 获取当前数据库版本
fn get_current_version(conn: &Connection) -> SqliteResult<i64> {
    let version: SqliteResult<i64> =
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        });

    match version {
        Ok(v) => Ok(v),
        Err(_) => Ok(0), // 如果表为空，返回 0
    }
}

/// 记录迁移版本
fn record_migration(conn: &Connection, version: i64) -> SqliteResult<()> {
    let current_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    conn.execute(
        "INSERT OR REPLACE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        [version, current_time_ms],
    )?;

    Ok(())
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

/// 迁移 1: 添加审批字段到 messages 表
fn migration_001_add_approval_fields(conn: &Connection) -> SqliteResult<()> {
    info!("Running migration 001: Add approval fields");

    // 如果表不存在，跳过迁移（schema 会创建完整表）
    if !table_exists(conn, "messages")? {
        info!("messages table does not exist, skipping migration (will be created by schema)");
        return Ok(());
    }

    // 检查 approval_status 列是否存在
    let approval_status_exists = column_exists(conn, "messages", "approval_status")?;

    if !approval_status_exists {
        info!("Adding approval_status column");
        conn.execute("ALTER TABLE messages ADD COLUMN approval_status TEXT", [])?;
    } else {
        info!("approval_status column already exists, skipping");
    }

    // 检查 approval_resolved_at 列是否存在
    let approval_resolved_at_exists = column_exists(conn, "messages", "approval_resolved_at")?;

    if !approval_resolved_at_exists {
        info!("Adding approval_resolved_at column");
        conn.execute(
            "ALTER TABLE messages ADD COLUMN approval_resolved_at INTEGER",
            [],
        )?;
    } else {
        info!("approval_resolved_at column already exists, skipping");
    }

    // 创建索引（如果不存在）
    // SQLite 的 CREATE INDEX IF NOT EXISTS 是安全的
    info!("Creating approval status indexes");
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_messages_approval_status ON messages(approval_status) WHERE approval_status IS NOT NULL",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_messages_approval_pending ON messages(session_id, approval_status) WHERE approval_status = 'pending'",
        [],
    )?;

    info!("Migration 001 complete");
    Ok(())
}

/// 迁移 2: 添加增量读取字段到 sessions 表
fn migration_002_add_incremental_fields(conn: &Connection) -> SqliteResult<()> {
    info!("Running migration 002: Add incremental read fields");

    // 如果表不存在，跳过迁移
    if !table_exists(conn, "sessions")? {
        info!("sessions table does not exist, skipping migration (will be created by schema)");
        return Ok(());
    }

    // 添加 file_offset 列
    if !column_exists(conn, "sessions", "file_offset")? {
        info!("Adding file_offset column");
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN file_offset INTEGER DEFAULT 0",
            [],
        )?;
    } else {
        info!("file_offset column already exists, skipping");
    }

    // 添加 file_inode 列
    if !column_exists(conn, "sessions", "file_inode")? {
        info!("Adding file_inode column");
        conn.execute("ALTER TABLE sessions ADD COLUMN file_inode INTEGER", [])?;
    } else {
        info!("file_inode column already exists, skipping");
    }

    info!("Migration 002 complete");
    Ok(())
}

/// 执行所有待应用的迁移
pub fn run_migrations(conn: &Connection) -> SqliteResult<()> {
    // 初始化迁移系统
    initialize_migrations(conn)?;

    // 获取当前版本
    let current_version = get_current_version(conn)?;
    info!("Current database version: {}", current_version);

    // 如果已经是最新版本，直接返回
    if current_version >= MIGRATION_VERSION {
        info!("Database is up to date, no migration needed");
        return Ok(());
    }

    // 执行迁移（事务保证原子性）
    let tx = conn.unchecked_transaction()?;

    // 迁移 1: 添加审批字段
    if current_version < 1 {
        match migration_001_add_approval_fields(&tx) {
            Ok(_) => {
                record_migration(&tx, 1)?;
                info!("Migration 1 applied");
            }
            Err(e) => {
                warn!("Migration 1 failed: {}", e);
                return Err(e);
            }
        }
    }

    // 迁移 2: 添加增量读取字段
    if current_version < 2 {
        match migration_002_add_incremental_fields(&tx) {
            Ok(_) => {
                record_migration(&tx, 2)?;
                info!("Migration 2 applied");
            }
            Err(e) => {
                warn!("Migration 2 failed: {}", e);
                return Err(e);
            }
        }
    }

    // 提交事务
    tx.commit()?;

    info!("All migrations applied successfully, current version: {}", MIGRATION_VERSION);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_migrations() {
        // 创建内存数据库
        let conn = Connection::open_in_memory().unwrap();

        // 创建基础 schema（模拟老版本数据库）
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                uuid TEXT NOT NULL UNIQUE,
                type TEXT NOT NULL,
                content_text TEXT NOT NULL,
                content_full TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                sequence INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL UNIQUE,
                project_id INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        // 运行迁移
        run_migrations(&conn).unwrap();

        // 验证迁移 1 的列是否存在
        assert!(column_exists(&conn, "messages", "approval_status").unwrap());
        assert!(column_exists(&conn, "messages", "approval_resolved_at").unwrap());

        // 验证迁移 2 的列是否存在
        assert!(column_exists(&conn, "sessions", "file_offset").unwrap());
        assert!(column_exists(&conn, "sessions", "file_inode").unwrap());

        // 验证版本
        assert_eq!(get_current_version(&conn).unwrap(), 2);

        // 再次运行迁移应该是幂等的
        run_migrations(&conn).unwrap();
        assert_eq!(get_current_version(&conn).unwrap(), 2);
    }
}
