//! sync.db - 独立的同步状态数据库
//!
//! 与主 DB (ai-cli-session.db) 完全隔离。
//! sync 模块是 sync.db 的唯一写入者。
//! 损坏可直接删除重建，最多重推一遍（server uuid 去重保证幂等）。

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;

const SYNC_DB_SCHEMA: &str = r#"
-- 每个 session 的 L0 消息推送游标
CREATE TABLE IF NOT EXISTS sync_cursors (
    session_id    TEXT PRIMARY KEY,
    last_sequence INTEGER NOT NULL DEFAULT 0,
    last_push_at  INTEGER NOT NULL DEFAULT 0
);

-- compact 推送游标（按 session + level 粒度）
CREATE TABLE IF NOT EXISTS compact_cursors (
    session_id   TEXT NOT NULL,
    level        TEXT NOT NULL,
    last_id      TEXT NOT NULL,
    last_push_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, level)
);

-- 向量推送游标
CREATE TABLE IF NOT EXISTS vector_cursors (
    session_id      TEXT PRIMARY KEY,
    last_message_id INTEGER NOT NULL DEFAULT 0,
    last_push_at    INTEGER NOT NULL DEFAULT 0
);

-- 全局同步状态
CREATE TABLE IF NOT EXISTS sync_state (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now')*1000)
);
"#;

const MIGRATIONS: &[&str] = &[
    // v1: sync_cursors 加 synced_message_count，用于 session 级快速变更检测
    "ALTER TABLE sync_cursors ADD COLUMN synced_message_count INTEGER NOT NULL DEFAULT 0;",
];

pub struct SyncDb {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl SyncDb {
    pub fn open(db_dir: &Path) -> Result<Self> {
        let path = db_dir.join("sync.db");
        let conn = Connection::open(&path)
            .with_context(|| format!("无法打开 sync.db: {}", path.display()))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;",
        )?;

        conn.execute_batch(SYNC_DB_SCHEMA)?;
        Self::run_migrations(&conn);

        info!("sync.db 已初始化: {}", path.display());

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    fn run_migrations(conn: &Connection) {
        for sql in MIGRATIONS {
            match conn.execute_batch(sql) {
                Ok(_) => info!("migration applied: {}", &sql[..sql.len().min(60)]),
                Err(_) => {} // column already exists, skip
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // ==================== sync_cursors ====================

    pub fn get_cursor(&self, session_id: &str) -> Result<(i64, i64)> {
        let conn = self.conn.lock();
        let result = conn.query_row(
            "SELECT last_sequence, last_push_at FROM sync_cursors WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        match result {
            Ok(cursor) => Ok(cursor),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((0, 0)),
            Err(e) => Err(e.into()),
        }
    }

    pub fn update_cursor(&self, session_id: &str, last_sequence: i64) -> Result<()> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO sync_cursors (session_id, last_sequence, last_push_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                last_sequence = excluded.last_sequence,
                last_push_at = excluded.last_push_at",
            rusqlite::params![session_id, last_sequence, now],
        )?;
        Ok(())
    }

    pub fn update_cursor_with_count(&self, session_id: &str, last_sequence: i64, message_count: i64) -> Result<()> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO sync_cursors (session_id, last_sequence, last_push_at, synced_message_count)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
                last_sequence = excluded.last_sequence,
                last_push_at = excluded.last_push_at,
                synced_message_count = excluded.synced_message_count",
            rusqlite::params![session_id, last_sequence, now, message_count],
        )?;
        Ok(())
    }

    pub fn get_all_cursors(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT session_id, last_sequence FROM sync_cursors"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    // ==================== compact_cursors ====================

    pub fn get_compact_cursor(&self, session_id: &str, level: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let result = conn.query_row(
            "SELECT last_id FROM compact_cursors WHERE session_id = ?1 AND level = ?2",
            rusqlite::params![session_id, level],
            |row| row.get(0),
        );

        match result {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn update_compact_cursor(
        &self,
        session_id: &str,
        level: &str,
        last_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO compact_cursors (session_id, level, last_id, last_push_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, level) DO UPDATE SET
                last_id = excluded.last_id,
                last_push_at = excluded.last_push_at",
            rusqlite::params![session_id, level, last_id, now],
        )?;
        Ok(())
    }

    // ==================== vector_cursors ====================

    pub fn get_vector_cursor(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock();
        let result = conn.query_row(
            "SELECT last_message_id FROM vector_cursors WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        );

        match result {
            Ok(id) => Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    pub fn update_vector_cursor(&self, session_id: &str, last_message_id: i64) -> Result<()> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO vector_cursors (session_id, last_message_id, last_push_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                last_message_id = excluded.last_message_id,
                last_push_at = excluded.last_push_at",
            rusqlite::params![session_id, last_message_id, now],
        )?;
        Ok(())
    }

    // ==================== sync_state ====================

    pub fn get_state(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let result = conn.query_row(
            "SELECT value FROM sync_state WHERE key = ?1",
            [key],
            |row| row.get(0),
        );

        match result {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO sync_state (key, value, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at",
            rusqlite::params![key, value, now],
        )?;
        Ok(())
    }

    pub fn get_or_init_device_id(&self) -> Result<String> {
        if let Some(id) = self.get_state("device_id")? {
            return Ok(id);
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.set_state("device_id", &id)?;
        Ok(id)
    }

    pub fn reset(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "DELETE FROM sync_cursors;
             DELETE FROM compact_cursors;
             DELETE FROM vector_cursors;"
        )?;
        info!("sync.db 游标已重置，下次将全量重推");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_sync_db_init() {
        let dir = TempDir::new().unwrap();
        let db = SyncDb::open(dir.path()).unwrap();
        assert!(db.path().exists());
    }

    #[test]
    fn test_cursor_operations() {
        let dir = TempDir::new().unwrap();
        let db = SyncDb::open(dir.path()).unwrap();

        assert_eq!(db.get_cursor("sess-1").unwrap(), (0, 0));

        db.update_cursor("sess-1", 42).unwrap();
        let (seq, push_at) = db.get_cursor("sess-1").unwrap();
        assert_eq!(seq, 42);
        assert!(push_at > 0);

        db.update_cursor("sess-1", 100).unwrap();
        assert_eq!(db.get_cursor("sess-1").unwrap().0, 100);
    }

    #[test]
    fn test_compact_cursor() {
        let dir = TempDir::new().unwrap();
        let db = SyncDb::open(dir.path()).unwrap();

        assert_eq!(db.get_compact_cursor("sess-1", "L2").unwrap(), None);

        db.update_compact_cursor("sess-1", "L2", "talk-abc").unwrap();
        assert_eq!(
            db.get_compact_cursor("sess-1", "L2").unwrap(),
            Some("talk-abc".to_string())
        );
    }

    #[test]
    fn test_device_id() {
        let dir = TempDir::new().unwrap();
        let db = SyncDb::open(dir.path()).unwrap();

        let id1 = db.get_or_init_device_id().unwrap();
        let id2 = db.get_or_init_device_id().unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_reset() {
        let dir = TempDir::new().unwrap();
        let db = SyncDb::open(dir.path()).unwrap();

        db.update_cursor("sess-1", 42).unwrap();
        db.reset().unwrap();
        assert_eq!(db.get_cursor("sess-1").unwrap(), (0, 0));
    }
}
