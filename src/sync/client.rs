//! Sync Client - 从本地 DB 只读组装 SyncBatch + push 到 server
//!
//! 对 ai-cli-session.db 以 read-only 模式打开（PRAGMA query_only=ON）。
//! 同步游标从 sync.db 读取/写入。

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use super::{
    FilterConfig, ProjectFilter, SensitiveWordFilter, SyncBatch, SyncChainNode,
    SyncConfig, SyncContinuationChain, SyncCursor, SyncDb, SyncMessage, SyncProject,
    SyncPushRequest, SyncPushResponse, SyncSession, SyncSessionRelation, SyncTalk,
};

pub struct SyncClient {
    config: SyncConfig,
    sync_db: Arc<SyncDb>,
    main_db_path: String,
    project_filter: ProjectFilter,
    word_filter: Option<SensitiveWordFilter>,
    http: reqwest::Client,
}

impl SyncClient {
    pub fn new(
        config: SyncConfig,
        filter_config: FilterConfig,
        sync_db: Arc<SyncDb>,
        db_dir: &Path,
    ) -> Result<Self> {
        let main_db_path = db_dir
            .join("ai-cli-session.db")
            .to_string_lossy()
            .to_string();

        let project_filter =
            ProjectFilter::new(&config.include_projects, &config.exclude_projects)?;

        let word_filter = if filter_config.enabled && !filter_config.sensitive_words_file.is_empty()
        {
            let path = shellexpand::tilde(&filter_config.sensitive_words_file);
            let path = Path::new(path.as_ref());
            if path.exists() {
                Some(SensitiveWordFilter::from_file(path, filter_config.mode)?)
            } else {
                warn!(
                    "敏感词文件不存在，跳过过滤: {}",
                    filter_config.sensitive_words_file
                );
                None
            }
        } else {
            None
        };

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .gzip(config.compress)
            .build()
            .context("创建 HTTP client 失败")?;

        Ok(Self {
            config,
            sync_db,
            main_db_path,
            project_filter,
            word_filter,
            http,
        })
    }

    /// 单次同步：collect → filter → push → update cursor
    pub async fn sync_once(&self) -> Result<SyncStats> {
        let mut stats = SyncStats::default();

        // 在同步块内完成所有 DB 读取，不让 Connection 跨 await
        let sessions = {
            let conn = self.open_readonly()?;
            self.list_syncable_sessions(&conn)?
        };

        if sessions.is_empty() {
            debug!("无需同步的 session");
            return Ok(stats);
        }

        for (session_id, project_path) in &sessions {
            match self.sync_session(session_id, project_path).await {
                Ok(batch_stats) => {
                    stats.sessions_synced += 1;
                    stats.messages_pushed += batch_stats.messages_pushed;
                    stats.messages_skipped += batch_stats.messages_skipped;
                }
                Err(e) => {
                    stats.sessions_failed += 1;
                    error!("sync session {} 失败: {}", &session_id[..8.min(session_id.len())], e);
                }
            }
        }

        info!(
            "同步完成: {} sessions, {} messages pushed, {} skipped, {} failed",
            stats.sessions_synced, stats.messages_pushed, stats.messages_skipped, stats.sessions_failed
        );

        Ok(stats)
    }

    async fn sync_session(
        &self,
        session_id: &str,
        project_path: &str,
    ) -> Result<BatchStats> {
        let (last_sequence, _) = self.sync_db.get_cursor(session_id)?;
        let mut total_pushed = 0u64;
        let mut total_skipped = 0u64;
        let mut current_cursor = last_sequence;

        loop {
            // collect 阶段：打开只读连接，读完立即释放，不跨 await
            let (mut batch, batch_max_seq, batch_len) = {
                let conn = self.open_readonly()?;
                let mut batch =
                    self.collect_batch(&conn, session_id, project_path, current_cursor)?;

                if batch.messages.is_empty() {
                    return Ok(BatchStats {
                        messages_pushed: total_pushed,
                        messages_skipped: total_skipped,
                    });
                }

                let batch_max_seq = batch
                    .messages
                    .iter()
                    .map(|m| m.sequence)
                    .max()
                    .unwrap_or(current_cursor);

                self.project_filter.filter_batch(&mut batch);

                if let Some(ref wf) = self.word_filter {
                    wf.filter_batch(&mut batch);
                }

                let batch_len = batch.messages.len();
                (batch, batch_max_seq, batch_len)
            };
            // conn 已释放，可以安全 await

            if batch.messages.is_empty() {
                current_cursor = batch_max_seq;
                self.sync_db.update_cursor(session_id, current_cursor)?;
                continue;
            }

            let response = self.push_batch(&batch).await?;

            total_pushed += response.accepted;
            total_skipped += response.skipped;
            current_cursor = batch_max_seq;
            self.sync_db.update_cursor(session_id, current_cursor)?;

            if batch_len < self.config.batch_size {
                break;
            }
        }

        Ok(BatchStats {
            messages_pushed: total_pushed,
            messages_skipped: total_skipped,
        })
    }

    fn collect_batch(
        &self,
        conn: &Connection,
        session_id: &str,
        project_path: &str,
        after_sequence: i64,
    ) -> Result<SyncBatch> {
        let project = self.read_project(conn, project_path)?;
        let session = self.read_session(conn, session_id)?;
        let messages = self.read_messages(conn, session_id, after_sequence)?;
        let relations = self.read_relations(conn, session_id)?;
        let talks = self.read_talks(conn, session_id)?;
        let (chains, nodes) = self.read_chains(conn, session_id)?;

        Ok(SyncBatch {
            projects: project.into_iter().collect(),
            sessions: session.into_iter().collect(),
            messages,
            session_relations: relations,
            continuation_chains: chains,
            chain_nodes: nodes,
            talks,
        })
    }

    async fn push_batch(&self, batch: &SyncBatch) -> Result<SyncPushResponse> {
        let device_id = self.sync_db.get_or_init_device_id()?;

        let cursors = batch
            .messages
            .iter()
            .fold(std::collections::HashMap::new(), |mut acc, m| {
                let entry = acc
                    .entry(m.session_id.clone())
                    .or_insert_with(|| SyncCursor {
                        last_sequence: 0,
                        last_timestamp: 0,
                    });
                if m.sequence > entry.last_sequence {
                    entry.last_sequence = m.sequence;
                    entry.last_timestamp = m.timestamp;
                }
                acc
            });

        let request = SyncPushRequest {
            device_id,
            encryption: self.config.encryption.clone(),
            batch: batch.clone(),
            cursors,
        };

        let url = format!("{}/api/sync/push", self.config.server.trim_end_matches('/'));

        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("push 请求失败: {url}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("push 失败 ({}): {}", status, body);
        }

        response
            .json::<SyncPushResponse>()
            .await
            .context("解析 push 响应失败")
    }

    // ==================== 只读 DB 操作 ====================

    fn open_readonly(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.main_db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("无法只读打开主 DB: {}", self.main_db_path))?;

        conn.execute_batch("PRAGMA query_only=ON;")?;

        Ok(conn)
    }

    fn list_syncable_sessions(&self, conn: &Connection) -> Result<Vec<(String, String)>> {
        let mut stmt = conn.prepare(
            r#"
            SELECT s.session_id, p.path
            FROM sessions s
            JOIN projects p ON s.project_id = p.id
            WHERE s.message_count > 0
            ORDER BY s.updated_at DESC
            "#,
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let all: Vec<(String, String)> = rows
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(all
            .into_iter()
            .filter(|(_, path)| self.project_filter.is_allowed(path))
            .collect())
    }

    fn read_project(&self, conn: &Connection, path: &str) -> Result<Option<SyncProject>> {
        let result = conn.query_row(
            "SELECT path, name, source, repo_url FROM projects WHERE path = ?1",
            [path],
            |row| {
                Ok(SyncProject {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    source: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    repo_url: row.get(3)?,
                })
            },
        );

        match result {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn read_session(&self, conn: &Connection, session_id: &str) -> Result<Option<SyncSession>> {
        let result = conn.query_row(
            r#"
            SELECT s.session_id, p.path, s.cwd, s.model, s.channel,
                   s.message_count, s.last_message_at,
                   s.session_type, s.source, s.meta,
                   s.created_at, s.updated_at
            FROM sessions s
            JOIN projects p ON s.project_id = p.id
            WHERE s.session_id = ?1
            "#,
            [session_id],
            |row| {
                Ok(SyncSession {
                    session_id: row.get(0)?,
                    project_path: row.get(1)?,
                    cwd: row.get(2)?,
                    model: row.get(3)?,
                    channel: row.get(4)?,
                    message_count: row.get(5)?,
                    last_message_at: row.get(6)?,
                    session_type: row.get(7)?,
                    source: row.get(8)?,
                    meta: row.get(9)?,
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            },
        );

        match result {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn read_messages(
        &self,
        conn: &Connection,
        session_id: &str,
        after_sequence: i64,
    ) -> Result<Vec<SyncMessage>> {
        let select_raw = if self.config.sync_raw {
            "raw"
        } else {
            "NULL"
        };

        let sql = format!(
            r#"
            SELECT uuid, session_id, type, content_text, content_full,
                   timestamp, sequence, source, channel, model,
                   tool_call_id, tool_name, tool_args, {select_raw},
                   approval_status, approval_resolved_at
            FROM messages
            WHERE session_id = ?1 AND sequence > ?2
            ORDER BY sequence ASC
            LIMIT ?3
            "#
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![session_id, after_sequence, self.config.batch_size as i64],
            |row| {
                Ok(SyncMessage {
                    uuid: row.get(0)?,
                    session_id: row.get(1)?,
                    msg_type: row.get(2)?,
                    content_text: row.get(3)?,
                    content_full: row.get(4)?,
                    timestamp: row.get(5)?,
                    sequence: row.get(6)?,
                    source: row.get(7)?,
                    channel: row.get(8)?,
                    model: row.get(9)?,
                    tool_call_id: row.get(10)?,
                    tool_name: row.get(11)?,
                    tool_args: row.get(12)?,
                    raw: row.get(13)?,
                    approval_status: row.get(14)?,
                    approval_resolved_at: row.get(15)?,
                })
            },
        )?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn read_relations(
        &self,
        conn: &Connection,
        session_id: &str,
    ) -> Result<Vec<SyncSessionRelation>> {
        let mut stmt = conn.prepare(
            r#"
            SELECT parent_session_id, child_session_id, relation_type, source
            FROM session_relations
            WHERE parent_session_id = ?1 OR child_session_id = ?1
            "#,
        )?;

        let rows = stmt.query_map([session_id], |row| {
            Ok(SyncSessionRelation {
                parent_session_id: row.get(0)?,
                child_session_id: row.get(1)?,
                relation_type: row.get(2)?,
                source: row.get(3)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn read_talks(&self, conn: &Connection, session_id: &str) -> Result<Vec<SyncTalk>> {
        let mut stmt = conn.prepare(
            r#"
            SELECT session_id, talk_id, summary_l2, summary_l3
            FROM talks
            WHERE session_id = ?1
            "#,
        )?;

        let rows = stmt.query_map([session_id], |row| {
            Ok(SyncTalk {
                session_id: row.get(0)?,
                talk_id: row.get(1)?,
                summary_l2: row.get(2)?,
                summary_l3: row.get(3)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn read_chains(
        &self,
        conn: &Connection,
        session_id: &str,
    ) -> Result<(Vec<SyncContinuationChain>, Vec<SyncChainNode>)> {
        let chain_id: Option<String> = conn
            .query_row(
                "SELECT chain_id FROM continuation_chain_nodes WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .ok();

        let Some(chain_id) = chain_id else {
            return Ok((vec![], vec![]));
        };

        let chain = conn.query_row(
            "SELECT chain_id, root_session_id FROM continuation_chains WHERE chain_id = ?1",
            [&chain_id],
            |row| {
                Ok(SyncContinuationChain {
                    chain_id: row.get(0)?,
                    root_session_id: row.get(1)?,
                })
            },
        )?;

        let mut stmt = conn.prepare(
            r#"
            SELECT session_id, chain_id, prev_session_id, depth
            FROM continuation_chain_nodes
            WHERE chain_id = ?1
            "#,
        )?;

        let nodes = stmt
            .query_map([&chain_id], |row| {
                Ok(SyncChainNode {
                    session_id: row.get(0)?,
                    chain_id: row.get(1)?,
                    prev_session_id: row.get(2)?,
                    depth: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok((vec![chain], nodes))
    }
}

// ==================== 统计 ====================

#[derive(Debug, Default)]
pub struct SyncStats {
    pub sessions_synced: u64,
    pub sessions_failed: u64,
    pub messages_pushed: u64,
    pub messages_skipped: u64,
}

#[derive(Debug, Default)]
struct BatchStats {
    messages_pushed: u64,
    messages_skipped: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn setup_test_db(dir: &Path) -> String {
        let db_path = dir.join("ai-cli-session.db");
        let conn = Connection::open(&db_path).unwrap();

        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;

            CREATE TABLE projects (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                source TEXT NOT NULL DEFAULT 'claude',
                encoded_dir_name TEXT,
                repo_url TEXT,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE sessions (
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
                session_type TEXT,
                source TEXT,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE messages (
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
                approval_resolved_at INTEGER
            );

            CREATE TABLE talks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                talk_id TEXT NOT NULL,
                summary_l2 TEXT NOT NULL,
                summary_l3 TEXT,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0,
                UNIQUE(session_id, talk_id)
            );

            CREATE TABLE session_relations (
                parent_session_id TEXT NOT NULL,
                child_session_id TEXT NOT NULL,
                relation_type TEXT NOT NULL,
                source TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (parent_session_id, child_session_id, relation_type)
            );

            CREATE TABLE continuation_chains (
                chain_id TEXT PRIMARY KEY,
                root_session_id TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE continuation_chain_nodes (
                session_id TEXT PRIMARY KEY,
                chain_id TEXT NOT NULL,
                prev_session_id TEXT,
                depth INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )
        .unwrap();

        db_path.to_string_lossy().to_string()
    }

    fn insert_project(conn: &Connection, path: &str, name: &str, repo_url: Option<&str>) -> i64 {
        conn.execute(
            "INSERT INTO projects (path, name, source, repo_url) VALUES (?1, ?2, 'claude', ?3)",
            params![path, name, repo_url],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_session(conn: &Connection, session_id: &str, project_id: i64, msg_count: i64) {
        conn.execute(
            "INSERT INTO sessions (session_id, project_id, message_count, created_at, updated_at) VALUES (?1, ?2, ?3, 1000, 1000)",
            params![session_id, project_id, msg_count],
        )
        .unwrap();
    }

    fn insert_message(
        conn: &Connection,
        uuid: &str,
        session_id: &str,
        sequence: i64,
        content: &str,
        raw: Option<&str>,
    ) {
        conn.execute(
            r#"INSERT INTO messages (session_id, uuid, type, content_text, content_full, timestamp, sequence, raw)
               VALUES (?1, ?2, 'assistant', ?3, ?3, ?4, ?4, ?5)"#,
            params![session_id, uuid, content, sequence, raw],
        )
        .unwrap();
    }

    fn make_client(dir: &Path, config: SyncConfig) -> SyncClient {
        let sync_db = Arc::new(SyncDb::open(dir).unwrap());
        SyncClient::new(config, FilterConfig::default(), sync_db, dir).unwrap()
    }

    fn default_config() -> SyncConfig {
        SyncConfig {
            enabled: true,
            server: "http://localhost:10013".to_string(),
            api_key: "test-key".to_string(),
            batch_size: 500,
            ..Default::default()
        }
    }

    // ==================== 只读保护 ====================

    #[test]
    fn test_readonly_prevents_writes() {
        let dir = TempDir::new().unwrap();
        setup_test_db(dir.path());
        let client = make_client(dir.path(), default_config());

        let conn = client.open_readonly().unwrap();
        let result = conn.execute("INSERT INTO projects (path, name) VALUES ('x', 'x')", []);
        assert!(result.is_err(), "只读连接不应该允许写入");
    }

    // ==================== 增量读取 ====================

    #[test]
    fn test_read_messages_incremental() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());
        let client = make_client(dir.path(), default_config());

        {
            let conn = Connection::open(&db_path).unwrap();
            let pid = insert_project(&conn, "/code/test", "test", None);
            insert_session(&conn, "sess-1", pid, 5);
            for i in 1..=5 {
                insert_message(
                    &conn,
                    &format!("uuid-{i}"),
                    "sess-1",
                    i,
                    &format!("msg {i}"),
                    None,
                );
            }
        }

        let conn = client.open_readonly().unwrap();

        // 从 0 开始读，应该拿到全部 5 条
        let msgs = client.read_messages(&conn, "sess-1", 0).unwrap();
        assert_eq!(msgs.len(), 5);

        // 从 sequence=3 开始，应该拿到 4,5
        let msgs = client.read_messages(&conn, "sess-1", 3).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].sequence, 4);
        assert_eq!(msgs[1].sequence, 5);

        // 从 sequence=5 开始，应该拿到 0 条
        let msgs = client.read_messages(&conn, "sess-1", 5).unwrap();
        assert_eq!(msgs.len(), 0);
    }

    // ==================== sync_raw 控制 ====================

    #[test]
    fn test_sync_raw_false_skips_raw_field() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());

        let mut config = default_config();
        config.sync_raw = false;
        let client = make_client(dir.path(), config);

        {
            let conn = Connection::open(&db_path).unwrap();
            let pid = insert_project(&conn, "/code/test", "test", None);
            insert_session(&conn, "sess-1", pid, 1);
            insert_message(
                &conn,
                "uuid-1",
                "sess-1",
                1,
                "hello",
                Some(r#"{"huge":"raw data"}"#),
            );
        }

        let conn = client.open_readonly().unwrap();
        let msgs = client.read_messages(&conn, "sess-1", 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].raw.is_none(), "sync_raw=false 时 raw 应为 None");
    }

    #[test]
    fn test_sync_raw_true_includes_raw_field() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());

        let mut config = default_config();
        config.sync_raw = true;
        let client = make_client(dir.path(), config);

        {
            let conn = Connection::open(&db_path).unwrap();
            let pid = insert_project(&conn, "/code/test", "test", None);
            insert_session(&conn, "sess-1", pid, 1);
            insert_message(
                &conn,
                "uuid-1",
                "sess-1",
                1,
                "hello",
                Some(r#"{"huge":"raw data"}"#),
            );
        }

        let conn = client.open_readonly().unwrap();
        let msgs = client.read_messages(&conn, "sess-1", 0).unwrap();
        assert_eq!(msgs[0].raw.as_deref(), Some(r#"{"huge":"raw data"}"#));
    }

    // ==================== batch_size 限制 ====================

    #[test]
    fn test_batch_size_limits_messages() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());

        let mut config = default_config();
        config.batch_size = 3;
        let client = make_client(dir.path(), config);

        {
            let conn = Connection::open(&db_path).unwrap();
            let pid = insert_project(&conn, "/code/test", "test", None);
            insert_session(&conn, "sess-1", pid, 10);
            for i in 1..=10 {
                insert_message(
                    &conn,
                    &format!("uuid-{i}"),
                    "sess-1",
                    i,
                    &format!("msg {i}"),
                    None,
                );
            }
        }

        let conn = client.open_readonly().unwrap();
        let msgs = client.read_messages(&conn, "sess-1", 0).unwrap();
        assert_eq!(msgs.len(), 3, "batch_size=3 时应只返回 3 条");
        assert_eq!(msgs[2].sequence, 3);
    }

    // ==================== 项目白名单过滤 ====================

    #[test]
    fn test_list_syncable_sessions_filters_by_project() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());

        let mut config = default_config();
        config.include_projects = vec!["/Users/*/code/*".to_string()];
        let client = make_client(dir.path(), config);

        {
            let conn = Connection::open(&db_path).unwrap();
            let pid1 = insert_project(&conn, "/Users/alice/code/ETerm", "ETerm", None);
            let pid2 = insert_project(&conn, "/Users/alice/scratch/test", "test", None);
            insert_session(&conn, "sess-1", pid1, 5);
            insert_session(&conn, "sess-2", pid2, 3);
        }

        let conn = client.open_readonly().unwrap();
        let sessions = client.list_syncable_sessions(&conn).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "sess-1");
        assert_eq!(sessions[0].1, "/Users/alice/code/ETerm");
    }

    // ==================== collect_batch 组装 ====================

    #[test]
    fn test_collect_batch_assembles_all_data() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());
        let client = make_client(dir.path(), default_config());

        {
            let conn = Connection::open(&db_path).unwrap();
            let pid = insert_project(
                &conn,
                "/code/ETerm",
                "ETerm",
                Some("git@github.com:vimo-ai/ETerm.git"),
            );
            insert_session(&conn, "sess-1", pid, 2);
            insert_message(&conn, "uuid-1", "sess-1", 1, "hello", None);
            insert_message(&conn, "uuid-2", "sess-1", 2, "world", None);

            conn.execute(
                "INSERT INTO talks (session_id, talk_id, summary_l2) VALUES ('sess-1', 'talk-1', 'summary here')",
                [],
            )
            .unwrap();

            conn.execute(
                "INSERT INTO session_relations (parent_session_id, child_session_id, relation_type, source) VALUES ('sess-1', 'agent-abc', 'subagent', 'claude')",
                [],
            )
            .unwrap();
        }

        let conn = client.open_readonly().unwrap();
        let batch = client
            .collect_batch(&conn, "sess-1", "/code/ETerm", 0)
            .unwrap();

        assert_eq!(batch.projects.len(), 1);
        assert_eq!(batch.projects[0].repo_url.as_deref(), Some("git@github.com:vimo-ai/ETerm.git"));
        assert_eq!(batch.sessions.len(), 1);
        assert_eq!(batch.messages.len(), 2);
        assert_eq!(batch.talks.len(), 1);
        assert_eq!(batch.talks[0].summary_l2, "summary here");
        assert_eq!(batch.session_relations.len(), 1);
        assert_eq!(batch.session_relations[0].child_session_id, "agent-abc");
    }

    // ==================== repo_url 透传 ====================

    #[test]
    fn test_read_project_with_repo_url() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());
        let client = make_client(dir.path(), default_config());

        {
            let conn = Connection::open(&db_path).unwrap();
            insert_project(
                &conn,
                "/code/ETerm",
                "ETerm",
                Some("git@github.com:vimo-ai/ETerm.git"),
            );
        }

        let conn = client.open_readonly().unwrap();
        let project = client.read_project(&conn, "/code/ETerm").unwrap().unwrap();
        assert_eq!(project.name, "ETerm");
        assert_eq!(
            project.repo_url.as_deref(),
            Some("git@github.com:vimo-ai/ETerm.git")
        );
    }

    #[test]
    fn test_read_project_without_repo_url() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());
        let client = make_client(dir.path(), default_config());

        {
            let conn = Connection::open(&db_path).unwrap();
            insert_project(&conn, "/code/local-only", "local", None);
        }

        let conn = client.open_readonly().unwrap();
        let project = client
            .read_project(&conn, "/code/local-only")
            .unwrap()
            .unwrap();
        assert!(project.repo_url.is_none());
    }

    // ==================== 零消息 session 不同步 ====================

    #[test]
    fn test_empty_sessions_not_synced() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());
        let client = make_client(dir.path(), default_config());

        {
            let conn = Connection::open(&db_path).unwrap();
            let pid = insert_project(&conn, "/code/test", "test", None);
            insert_session(&conn, "sess-empty", pid, 0);
            insert_session(&conn, "sess-has-msg", pid, 3);
        }

        let conn = client.open_readonly().unwrap();
        let sessions = client.list_syncable_sessions(&conn).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "sess-has-msg");
    }

    // ==================== continuation chain ====================

    #[test]
    fn test_read_chains() {
        let dir = TempDir::new().unwrap();
        let db_path = setup_test_db(dir.path());
        let client = make_client(dir.path(), default_config());

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO continuation_chains (chain_id, root_session_id) VALUES ('chain-1', 'sess-root')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO continuation_chain_nodes (session_id, chain_id, prev_session_id, depth) VALUES ('sess-root', 'chain-1', NULL, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO continuation_chain_nodes (session_id, chain_id, prev_session_id, depth) VALUES ('sess-cont', 'chain-1', 'sess-root', 1)",
                [],
            )
            .unwrap();
        }

        let conn = client.open_readonly().unwrap();
        let (chains, nodes) = client.read_chains(&conn, "sess-root").unwrap();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].root_session_id, "sess-root");
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn test_read_chains_no_chain() {
        let dir = TempDir::new().unwrap();
        setup_test_db(dir.path());
        let client = make_client(dir.path(), default_config());

        let conn = client.open_readonly().unwrap();
        let (chains, nodes) = client.read_chains(&conn, "sess-no-chain").unwrap();
        assert!(chains.is_empty());
        assert!(nodes.is_empty());
    }
}
