//! 数据库连接和操作

use crate::config::{ConnectionMode, DbConfig};
use crate::error::{Error, Result};
use crate::migrations;
use crate::schema;
use crate::types::{Message, Project, ProjectWithStats, Session, SessionWithProject, Stats};
use ai_cli_session_collector::MessageType;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "coordination")]
use crate::coordination::{CoordinationConfig, Coordinator, Role, WriterHealth, WriterType};

/// 数据库连接
pub struct SessionDB {
    pub(crate) conn: Arc<Mutex<Connection>>,
    #[allow(dead_code)]
    config: DbConfig,

    #[cfg(feature = "coordination")]
    coordinator: Option<Coordinator>,
}

impl SessionDB {
    /// 连接数据库
    pub fn connect(config: DbConfig) -> Result<Self> {
        match config.mode {
            ConnectionMode::Local => Self::connect_local(&config),
            ConnectionMode::Remote => Err(Error::Config("远程连接暂不支持".into())),
        }
    }

    /// 连接本地 SQLite
    fn connect_local(config: &DbConfig) -> Result<Self> {
        let path = Path::new(&config.url);

        // 确保目录存在
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;

        // 执行数据库迁移（先于 schema，为老数据库添加缺失的列）
        // 注意：如果是新数据库，迁移会跳过（表不存在）
        migrations::run_migrations(&conn)?;

        // 初始化 schema（创建表和索引）
        let fts = cfg!(feature = "fts");
        let coordination = cfg!(feature = "coordination");
        let full_schema = schema::full_schema(fts, coordination);
        conn.execute_batch(&full_schema)?;

        tracing::info!("数据库已连接: {:?}", path);

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            config: config.clone(),
            #[cfg(feature = "coordination")]
            coordinator: None,
        })
    }

    /// 获取底层连接 (用于测试)
    #[doc(hidden)]
    pub fn connection(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    // ==================== Writer 协调 ====================

    #[cfg(feature = "coordination")]
    /// 注册为 Writer，返回实际角色
    pub fn register_writer(&mut self, writer_type: WriterType) -> Result<Role> {
        let coordinator = Coordinator::new(writer_type, CoordinationConfig::default());
        let conn = self.conn.lock();
        let role = coordinator.try_register(&conn)?;
        self.coordinator = Some(coordinator);
        Ok(role)
    }

    #[cfg(feature = "coordination")]
    /// 更新心跳 (Writer 定期调用)
    pub fn heartbeat(&self) -> Result<()> {
        let coordinator = self
            .coordinator
            .as_ref()
            .ok_or_else(|| Error::Coordination("未注册为 Writer".into()))?;
        let conn = self.conn.lock();
        coordinator.heartbeat(&conn)
    }

    #[cfg(feature = "coordination")]
    /// 释放 Writer (正常退出时调用)
    pub fn release_writer(&self) -> Result<()> {
        if let Some(ref coordinator) = self.coordinator {
            let conn = self.conn.lock();
            coordinator.release(&conn)?;
        }
        Ok(())
    }

    #[cfg(feature = "coordination")]
    /// 检查 Writer 健康状态 (Reader 调用)
    pub fn check_writer_health(&self) -> Result<WriterHealth> {
        let coordinator = self
            .coordinator
            .as_ref()
            .ok_or_else(|| Error::Coordination("未注册".into()))?;
        let conn = self.conn.lock();
        coordinator.check_writer_health(&conn)
    }

    #[cfg(feature = "coordination")]
    /// 尝试接管 Writer (Reader 在检测到超时后调用)
    pub fn try_takeover(&mut self) -> Result<bool> {
        let coordinator = self
            .coordinator
            .as_ref()
            .ok_or_else(|| Error::Coordination("未注册".into()))?;
        let conn = self.conn.lock();
        coordinator.try_takeover(&conn)
    }

    #[cfg(feature = "coordination")]
    /// 监听角色变化
    pub fn watch_role_change(&self) -> Option<tokio::sync::watch::Receiver<Role>> {
        self.coordinator.as_ref().map(|c| c.watch_role())
    }

    // ==================== Project 操作 ====================

    /// 获取或创建 Project
    pub fn get_or_create_project(&self, name: &str, path: &str, source: &str) -> Result<i64> {
        self.get_or_create_project_with_encoded(name, path, source, None)
    }

    /// 获取或创建 Project（支持 encoded_dir_name）
    pub fn get_or_create_project_with_encoded(
        &self,
        name: &str,
        path: &str,
        source: &str,
        encoded_dir_name: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.lock();

        // 先查找
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM projects WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(id) = existing {
            // 更新 updated_at，如果有 encoded_dir_name 也一并更新
            let now = current_time_ms();
            if let Some(encoded) = encoded_dir_name {
                conn.execute(
                    "UPDATE projects SET updated_at = ?1, encoded_dir_name = COALESCE(?2, encoded_dir_name) WHERE id = ?3",
                    params![now, encoded, id],
                )?;
            } else {
                conn.execute(
                    "UPDATE projects SET updated_at = ?1 WHERE id = ?2",
                    params![now, id],
                )?;
            }
            return Ok(id);
        }

        // 创建
        let now = current_time_ms();
        conn.execute(
            "INSERT INTO projects (name, path, source, encoded_dir_name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![name, path, source, encoded_dir_name, now],
        )?;

        Ok(conn.last_insert_rowid())
    }

    /// 获取所有 Projects
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, path, source, encoded_dir_name, created_at, updated_at FROM projects ORDER BY updated_at DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                source: row.get(3)?,
                encoded_dir_name: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 获取所有 Projects（带统计信息，支持分页）
    pub fn list_projects_with_stats(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ProjectWithStats>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT
                p.id,
                p.name,
                p.path,
                COUNT(DISTINCT s.id) as session_count,
                COALESCE(SUM(s.message_count), 0) as message_count,
                MAX(COALESCE(s.last_message_at, s.updated_at)) as last_active
            FROM projects p
            LEFT JOIN sessions s ON s.project_id = p.id
            GROUP BY p.id
            ORDER BY last_active DESC NULLS LAST
            LIMIT ?1 OFFSET ?2
            "#,
        )?;

        let rows = stmt.query_map(params![limit as i64, offset as i64], |row| {
            Ok(ProjectWithStats {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                session_count: row.get(3)?,
                message_count: row.get(4)?,
                last_active: row.get(5)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 获取单个 Project
    pub fn get_project(&self, id: i64) -> Result<Option<Project>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, name, path, source, encoded_dir_name, created_at, updated_at FROM projects WHERE id = ?1",
            params![id],
            |row| {
                Ok(Project {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    source: row.get(3)?,
                    encoded_dir_name: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// 根据路径获取 Project
    pub fn get_project_by_path(&self, path: &str) -> Result<Option<Project>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, name, path, source, encoded_dir_name, created_at, updated_at FROM projects WHERE path = ?1",
            params![path],
            |row| {
                Ok(Project {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    source: row.get(3)?,
                    encoded_dir_name: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    // ==================== Session 操作 ====================

    /// 创建或更新 Session (简化版，仅 session_id 和 project_id)
    pub fn upsert_session(&self, session_id: &str, project_id: i64) -> Result<()> {
        let conn = self.conn.lock();
        let now = current_time_ms();

        conn.execute(
            r#"
            INSERT INTO sessions (session_id, project_id, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?3)
            ON CONFLICT(session_id) DO UPDATE SET
                updated_at = excluded.updated_at
            "#,
            params![session_id, project_id, now],
        )?;

        Ok(())
    }

    /// 创建或更新 Session (完整版，支持所有元数据字段)
    pub fn upsert_session_full(&self, input: &SessionInput) -> Result<()> {
        let conn = self.conn.lock();
        let now = current_time_ms();

        conn.execute(
            r#"
            INSERT INTO sessions (session_id, project_id, cwd, model, channel, message_count, file_mtime, file_size, meta, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
            ON CONFLICT(session_id) DO UPDATE SET
                cwd = COALESCE(excluded.cwd, sessions.cwd),
                model = COALESCE(excluded.model, sessions.model),
                channel = COALESCE(excluded.channel, sessions.channel),
                message_count = COALESCE(excluded.message_count, sessions.message_count),
                file_mtime = COALESCE(excluded.file_mtime, sessions.file_mtime),
                file_size = COALESCE(excluded.file_size, sessions.file_size),
                meta = COALESCE(excluded.meta, sessions.meta),
                updated_at = excluded.updated_at
            "#,
            params![
                input.session_id,
                input.project_id,
                input.cwd,
                input.model,
                input.channel,
                input.message_count,
                input.file_mtime,
                input.file_size,
                input.meta,
                now,
            ],
        )?;

        Ok(())
    }

    /// 获取 Project 的 Sessions
    pub fn list_sessions(&self, project_id: i64) -> Result<Vec<Session>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, session_id, project_id, message_count, last_message_at,
                   cwd, model, channel, file_mtime, file_size, meta,
                   created_at, updated_at
            FROM sessions
            WHERE project_id = ?1
            ORDER BY updated_at DESC
            "#,
        )?;

        let rows = stmt.query_map(params![project_id], |row| {
            Ok(Session {
                id: row.get(0)?,
                session_id: row.get(1)?,
                project_id: row.get(2)?,
                message_count: row.get(3)?,
                last_message_at: row.get(4)?,
                cwd: row.get(5)?,
                model: row.get(6)?,
                channel: row.get(7)?,
                file_mtime: row.get(8)?,
                file_size: row.get(9)?,
                meta: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 根据项目路径列出会话（带项目信息，支持分页）
    pub fn list_sessions_by_project_path(
        &self,
        project_path: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SessionWithProject>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT s.id, s.session_id, s.project_id, p.name, p.path,
                   s.message_count, s.last_message_at,
                   s.cwd, s.model, s.channel, s.file_mtime, s.file_size, s.encoded_dir_name, s.meta,
                   s.created_at, s.updated_at
            FROM sessions s
            INNER JOIN projects p ON s.project_id = p.id
            WHERE p.path = ?1 AND s.session_id NOT LIKE 'agent-%'
            ORDER BY s.updated_at DESC
            LIMIT ?2 OFFSET ?3
            "#,
        )?;

        let rows = stmt.query_map(params![project_path, limit as i64, offset as i64], |row| {
            Ok(SessionWithProject {
                id: row.get(0)?,
                session_id: row.get(1)?,
                project_id: row.get(2)?,
                project_name: row.get(3)?,
                project_path: row.get(4)?,
                message_count: row.get(5)?,
                last_message_at: row.get(6)?,
                cwd: row.get(7)?,
                model: row.get(8)?,
                channel: row.get(9)?,
                file_mtime: row.get(10)?,
                file_size: row.get(11)?,
                encoded_dir_name: row.get(12)?,
                meta: row.get(13)?,
                created_at: row.get(14)?,
                updated_at: row.get(15)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 获取单个 Session
    pub fn get_session(&self, session_id: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock();
        conn.query_row(
            r#"
            SELECT id, session_id, project_id, message_count, last_message_at,
                   cwd, model, channel, file_mtime, file_size, meta,
                   created_at, updated_at
            FROM sessions
            WHERE session_id = ?1
            "#,
            params![session_id],
            |row| {
                Ok(Session {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    project_id: row.get(2)?,
                    message_count: row.get(3)?,
                    last_message_at: row.get(4)?,
                    cwd: row.get(5)?,
                    model: row.get(6)?,
                    channel: row.get(7)?,
                    file_mtime: row.get(8)?,
                    file_size: row.get(9)?,
                    meta: row.get(10)?,
                    created_at: row.get(11)?,
                    updated_at: row.get(12)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// 检查 Session 是否存在
    pub fn session_exists(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// 获取 Session 的消息数量
    pub fn get_session_message_count(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// 获取 Session 的最新消息时间戳（毫秒）
    pub fn get_session_latest_timestamp(&self, session_id: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT MAX(timestamp) FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// 获取 Sessions (支持可选的 project_id 过滤)
    pub fn get_sessions(&self, project_id: Option<i64>, limit: usize) -> Result<Vec<Session>> {
        let conn = self.conn.lock();

        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(pid) = project_id
        {
            (
                r#"
                SELECT id, session_id, project_id, message_count, last_message_at,
                       cwd, model, channel, file_mtime, file_size, meta,
                       created_at, updated_at
                FROM sessions
                WHERE project_id = ?1
                ORDER BY updated_at DESC
                LIMIT ?2
                "#,
                vec![
                    Box::new(pid) as Box<dyn rusqlite::ToSql>,
                    Box::new(limit as i64),
                ],
            )
        } else {
            (
                r#"
                SELECT id, session_id, project_id, message_count, last_message_at,
                       cwd, model, channel, file_mtime, file_size, meta,
                       created_at, updated_at
                FROM sessions
                ORDER BY updated_at DESC
                LIMIT ?1
                "#,
                vec![Box::new(limit as i64)],
            )
        };

        let mut stmt = conn.prepare(sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(Session {
                id: row.get(0)?,
                session_id: row.get(1)?,
                project_id: row.get(2)?,
                message_count: row.get(3)?,
                last_message_at: row.get(4)?,
                cwd: row.get(5)?,
                model: row.get(6)?,
                channel: row.get(7)?,
                file_mtime: row.get(8)?,
                file_size: row.get(9)?,
                meta: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 通过前缀解析完整会话 ID
    pub fn resolve_session_id(&self, prefix: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let pattern = format!("{}%", prefix);
        conn.query_row(
            "SELECT session_id FROM sessions WHERE session_id LIKE ?1 LIMIT 1",
            params![pattern],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// 按 session_id 前缀搜索会话列表
    pub fn search_sessions_by_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Session>> {
        let conn = self.conn.lock();
        let pattern = format!("{}%", prefix);

        let mut stmt = conn.prepare(
            r#"
            SELECT id, session_id, project_id, message_count, last_message_at,
                   cwd, model, channel, file_mtime, file_size, meta,
                   created_at, updated_at
            FROM sessions
            WHERE session_id LIKE ?1
            ORDER BY updated_at DESC
            LIMIT ?2
            "#,
        )?;

        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            Ok(Session {
                id: row.get(0)?,
                session_id: row.get(1)?,
                project_id: row.get(2)?,
                message_count: row.get(3)?,
                last_message_at: row.get(4)?,
                cwd: row.get(5)?,
                model: row.get(6)?,
                channel: row.get(7)?,
                file_mtime: row.get(8)?,
                file_size: row.get(9)?,
                meta: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 获取 session 的扫描检查点 (用于增量扫描)
    ///
    /// 返回:
    /// - `Ok(None)` - session 不存在或 last_message_at 为空
    /// - `Ok(Some(ts))` - 检查点时间戳
    pub fn get_scan_checkpoint(&self, session_id: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        let result: Option<Option<i64>> = conn
            .query_row(
                "SELECT last_message_at FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?;

        // 展平 Option<Option<i64>> 为 Option<i64>
        Ok(result.flatten())
    }

    /// 更新 session 的最后消息时间
    pub fn update_session_last_message(&self, session_id: &str, timestamp: i64) -> Result<()> {
        let conn = self.conn.lock();
        let now = current_time_ms();

        conn.execute(
            "UPDATE sessions SET last_message_at = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![timestamp, now, session_id],
        )?;

        Ok(())
    }

    // ==================== Message 操作 ====================

    /// 批量写入 Messages (自动去重)
    /// 返回实际插入的数量
    pub fn insert_messages(&self, session_id: &str, messages: &[MessageInput]) -> Result<usize> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        let mut inserted = 0;
        for msg in messages {
            let result = tx.execute(
                r#"
                INSERT INTO messages (session_id, uuid, type, content_text, content_full, timestamp, sequence, source, channel, model, tool_call_id, tool_name, tool_args, raw, approval_status, approval_resolved_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                ON CONFLICT(uuid) DO NOTHING
                "#,
                params![
                    session_id,
                    &msg.uuid,
                    msg.r#type.to_string(),
                    &msg.content_text,
                    &msg.content_full,
                    msg.timestamp,
                    msg.sequence,
                    &msg.source,
                    &msg.channel,
                    &msg.model,
                    &msg.tool_call_id,
                    &msg.tool_name,
                    &msg.tool_args,
                    &msg.raw,
                    &msg.approval_status.map(|s| s.to_string()),
                    &msg.approval_resolved_at,
                ],
            );

            if let Ok(n) = result {
                inserted += n;
            }
        }

        // 更新 session 的 message_count
        tx.execute(
            r#"
            UPDATE sessions SET
                message_count = (SELECT COUNT(*) FROM messages WHERE session_id = ?1),
                updated_at = ?2
            WHERE session_id = ?1
            "#,
            params![session_id, current_time_ms()],
        )?;

        tx.commit()?;
        Ok(inserted)
    }

    /// 获取 Session 的 Messages
    pub fn list_messages(
        &self,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Message>> {
        self.list_messages_ordered(session_id, limit, offset, false)
    }

    /// 列出会话消息（支持排序）
    /// - desc: true 表示倒序（最新的在前）
    pub fn list_messages_ordered(
        &self,
        session_id: &str,
        limit: usize,
        offset: usize,
        desc: bool,
    ) -> Result<Vec<Message>> {
        let conn = self.conn.lock();
        let order = if desc { "DESC" } else { "ASC" };
        let sql = format!(
            r#"
            SELECT id, session_id, uuid, type, content_text, content_full, timestamp, sequence,
                   source, channel, model, tool_call_id, tool_name, tool_args, raw, vector_indexed,
                   approval_status, approval_resolved_at
            FROM messages
            WHERE session_id = ?1
            ORDER BY sequence {}
            LIMIT ?2 OFFSET ?3
            "#,
            order
        );
        let mut stmt = conn.prepare(&sql)?;

        let rows = stmt.query_map(params![session_id, limit as i64, offset as i64], |row| {
            let type_str: String = row.get(3)?;
            let vector_indexed: i64 = row.get(15)?;
            Ok(Message {
                id: row.get(0)?,
                session_id: row.get(1)?,
                uuid: row.get(2)?,
                r#type: type_str.parse().unwrap_or(MessageType::User),
                content_text: row.get(4)?,
                content_full: row.get(5)?,
                timestamp: row.get(6)?,
                sequence: row.get(7)?,
                source: row.get(8)?,
                channel: row.get(9)?,
                model: row.get(10)?,
                tool_call_id: row.get(11)?,
                tool_name: row.get(12)?,
                tool_args: row.get(13)?,
                raw: row.get(14)?,
                vector_indexed: vector_indexed != 0,
                approval_status: row
                    .get::<_, Option<String>>(16)?
                    .and_then(|s| s.parse().ok()),
                approval_resolved_at: row.get(17)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 获取 Session 的所有 Messages (无分页)
    pub fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        self.get_messages_with_options(session_id, None, false)
    }

    /// 获取 Session 的 Messages (带分页和排序选项)
    /// - limit: 返回数量限制，None 表示不限制
    /// - desc: true 表示倒序（最新的在前）
    pub fn get_messages_with_options(
        &self,
        session_id: &str,
        limit: Option<usize>,
        desc: bool,
    ) -> Result<Vec<Message>> {
        let conn = self.conn.lock();
        let order = if desc { "DESC" } else { "ASC" };

        let sql = format!(
            r#"
            SELECT id, session_id, uuid, type, content_text, content_full, timestamp, sequence,
                   source, channel, model, tool_call_id, tool_name, tool_args, raw, vector_indexed,
                   approval_status, approval_resolved_at
            FROM messages
            WHERE session_id = ?1
            ORDER BY sequence {}
            LIMIT ?2
            "#,
            order
        );

        let limit_val = limit.unwrap_or(i64::MAX as usize) as i64;
        let mut stmt = conn.prepare(&sql)?;

        let rows = stmt.query_map(params![session_id, limit_val], |row| {
            let type_str: String = row.get(3)?;
            let vector_indexed: i64 = row.get(15)?;
            Ok(Message {
                id: row.get(0)?,
                session_id: row.get(1)?,
                uuid: row.get(2)?,
                r#type: type_str.parse().unwrap_or(MessageType::User),
                content_text: row.get(4)?,
                content_full: row.get(5)?,
                timestamp: row.get(6)?,
                sequence: row.get(7)?,
                source: row.get(8)?,
                channel: row.get(9)?,
                model: row.get(10)?,
                tool_call_id: row.get(11)?,
                tool_name: row.get(12)?,
                tool_args: row.get(13)?,
                raw: row.get(14)?,
                vector_indexed: vector_indexed != 0,
                approval_status: row
                    .get::<_, Option<String>>(16)?
                    .and_then(|s| s.parse().ok()),
                approval_resolved_at: row.get(17)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    // ==================== 统计 ====================

    /// 获取统计信息
    pub fn get_stats(&self) -> Result<Stats> {
        let conn = self.conn.lock();

        let project_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))?;
        let session_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        let message_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;

        Ok(Stats {
            project_count,
            session_count,
            message_count,
        })
    }

    // ==================== 向量索引 ====================

    /// 获取未向量索引的消息（用于增量索引）
    /// 只返回 assistant 类型的消息
    pub fn get_unindexed_messages(&self, limit: usize) -> Result<Vec<Message>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, session_id, uuid, type, content_text, content_full, timestamp, sequence,
                   source, channel, model, tool_call_id, tool_name, tool_args, raw, vector_indexed,
                   approval_status, approval_resolved_at
            FROM messages
            WHERE vector_indexed = 0 AND type = 'assistant'
            ORDER BY id ASC
            LIMIT ?1
            "#,
        )?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            let type_str: String = row.get(3)?;
            let vector_indexed: i64 = row.get(15)?;
            Ok(Message {
                id: row.get(0)?,
                session_id: row.get(1)?,
                uuid: row.get(2)?,
                r#type: type_str.parse().unwrap_or(MessageType::User),
                content_text: row.get(4)?,
                content_full: row.get(5)?,
                timestamp: row.get(6)?,
                sequence: row.get(7)?,
                source: row.get(8)?,
                channel: row.get(9)?,
                model: row.get(10)?,
                tool_call_id: row.get(11)?,
                tool_name: row.get(12)?,
                tool_args: row.get(13)?,
                raw: row.get(14)?,
                vector_indexed: vector_indexed != 0,
                approval_status: row
                    .get::<_, Option<String>>(16)?
                    .and_then(|s| s.parse().ok()),
                approval_resolved_at: row.get(17)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 标记消息已向量索引
    pub fn mark_messages_indexed(&self, message_ids: &[i64]) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }

        let conn = self.conn.lock();
        let placeholders: String = message_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE messages SET vector_indexed = 1 WHERE id IN ({})",
            placeholders
        );

        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = message_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();

        let count = stmt.execute(params.as_slice())?;
        Ok(count)
    }

    /// 获取未索引消息的数量
    pub fn count_unindexed_messages(&self) -> Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE vector_indexed = 0 AND type = 'assistant'",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// 标记消息向量索引失败
    /// vector_indexed = -1 表示失败
    pub fn mark_message_index_failed(&self, message_id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE messages SET vector_indexed = -1 WHERE id = ?1",
            params![message_id],
        )?;
        Ok(())
    }

    /// 批量标记消息向量索引失败
    pub fn mark_messages_index_failed(&self, message_ids: &[i64]) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }

        let conn = self.conn.lock();
        let placeholders: String = message_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE messages SET vector_indexed = -1 WHERE id IN ({})",
            placeholders
        );

        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = message_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();

        let count = stmt.execute(params.as_slice())?;
        Ok(count)
    }

    /// 获取索引失败的消息
    /// vector_indexed = -1 表示失败
    pub fn get_failed_indexed_messages(&self, limit: usize) -> Result<Vec<Message>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, session_id, uuid, type, content_text, content_full, timestamp, sequence,
                   source, channel, model, tool_call_id, tool_name, tool_args, raw, vector_indexed,
                   approval_status, approval_resolved_at
            FROM messages
            WHERE vector_indexed = -1
            ORDER BY id ASC
            LIMIT ?1
            "#,
        )?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            let type_str: String = row.get(3)?;
            let vector_indexed: i64 = row.get(15)?;
            Ok(Message {
                id: row.get(0)?,
                session_id: row.get(1)?,
                uuid: row.get(2)?,
                r#type: type_str.parse().unwrap_or(MessageType::User),
                content_text: row.get(4)?,
                content_full: row.get(5)?,
                timestamp: row.get(6)?,
                sequence: row.get(7)?,
                source: row.get(8)?,
                channel: row.get(9)?,
                model: row.get(10)?,
                tool_call_id: row.get(11)?,
                tool_name: row.get(12)?,
                tool_args: row.get(13)?,
                raw: row.get(14)?,
                vector_indexed: vector_indexed != 0,
                approval_status: row
                    .get::<_, Option<String>>(16)?
                    .and_then(|s| s.parse().ok()),
                approval_resolved_at: row.get(17)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 统计索引失败的消息数量
    pub fn count_failed_indexed_messages(&self) -> Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE vector_indexed = -1",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// 重置失败的索引状态（将 -1 改为 0，可重新索引）
    pub fn reset_failed_indexed_messages(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let count = conn.execute(
            "UPDATE messages SET vector_indexed = 0 WHERE vector_indexed = -1",
            [],
        )?;
        Ok(count)
    }

    /// 按 ID 列表获取消息
    pub fn get_messages_by_ids(&self, ids: &[i64]) -> Result<Vec<Message>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock();
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            r#"
            SELECT id, session_id, uuid, type, content_text, content_full, timestamp, sequence,
                   source, channel, model, tool_call_id, tool_name, tool_args, raw, vector_indexed,
                   approval_status, approval_resolved_at
            FROM messages
            WHERE id IN ({})
            ORDER BY id ASC
            "#,
            placeholders
        );

        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();

        let rows = stmt.query_map(params.as_slice(), |row| {
            let type_str: String = row.get(3)?;
            let vector_indexed: i64 = row.get(15)?;
            Ok(Message {
                id: row.get(0)?,
                session_id: row.get(1)?,
                uuid: row.get(2)?,
                r#type: type_str.parse().unwrap_or(MessageType::User),
                content_text: row.get(4)?,
                content_full: row.get(5)?,
                timestamp: row.get(6)?,
                sequence: row.get(7)?,
                source: row.get(8)?,
                channel: row.get(9)?,
                model: row.get(10)?,
                tool_call_id: row.get(11)?,
                tool_name: row.get(12)?,
                tool_args: row.get(13)?,
                raw: row.get(14)?,
                vector_indexed: vector_indexed != 0,
                approval_status: row
                    .get::<_, Option<String>>(16)?
                    .and_then(|s| s.parse().ok()),
                approval_resolved_at: row.get(17)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    // ==================== 审批操作 ====================

    /// 获取待审批的消息
    /// - session_id: 会话 ID
    /// 返回 approval_status = 'pending' 的消息
    pub fn get_pending_approvals(&self, session_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, session_id, uuid, type, content_text, content_full, timestamp, sequence,
                   source, channel, model, tool_call_id, tool_name, tool_args, raw, vector_indexed,
                   approval_status, approval_resolved_at
            FROM messages
            WHERE session_id = ?1 AND approval_status = 'pending'
            ORDER BY sequence ASC
            "#,
        )?;

        let rows = stmt.query_map(params![session_id], |row| {
            let type_str: String = row.get(3)?;
            let vector_indexed: i64 = row.get(15)?;
            Ok(Message {
                id: row.get(0)?,
                session_id: row.get(1)?,
                uuid: row.get(2)?,
                r#type: type_str.parse().unwrap_or(MessageType::User),
                content_text: row.get(4)?,
                content_full: row.get(5)?,
                timestamp: row.get(6)?,
                sequence: row.get(7)?,
                source: row.get(8)?,
                channel: row.get(9)?,
                model: row.get(10)?,
                tool_call_id: row.get(11)?,
                tool_name: row.get(12)?,
                tool_args: row.get(13)?,
                raw: row.get(14)?,
                vector_indexed: vector_indexed != 0,
                approval_status: row
                    .get::<_, Option<String>>(16)?
                    .and_then(|s| s.parse().ok()),
                approval_resolved_at: row.get(17)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 更新审批状态
    /// - uuid: 消息的 UUID
    /// - status: 审批状态 (approved, rejected, timeout)
    /// - resolved_at: 审批解决时间戳（毫秒）
    pub fn update_approval_status(
        &self,
        uuid: &str,
        status: crate::types::ApprovalStatus,
        resolved_at: i64,
    ) -> Result<usize> {
        let conn = self.conn.lock();
        let count = conn.execute(
            r#"
            UPDATE messages
            SET approval_status = ?1, approval_resolved_at = ?2
            WHERE uuid = ?3
            "#,
            params![status.to_string(), resolved_at, uuid],
        )?;
        Ok(count)
    }

    /// 通过 tool_call_id 更新审批状态
    /// - tool_call_id: 工具调用 ID
    /// - status: 审批状态 (approved, rejected, timeout, pending)
    /// - resolved_at: 审批解决时间戳（毫秒，pending 状态时为 0）
    /// 返回更新的行数
    pub fn update_approval_status_by_tool_call_id(
        &self,
        tool_call_id: &str,
        status: crate::types::ApprovalStatus,
        resolved_at: i64,
    ) -> Result<usize> {
        let conn = self.conn.lock();
        let count = conn.execute(
            r#"
            UPDATE messages
            SET approval_status = ?1, approval_resolved_at = ?2
            WHERE tool_call_id = ?3
            "#,
            params![status.to_string(), resolved_at, tool_call_id],
        )?;
        Ok(count)
    }

    /// 批量更新审批状态
    /// - uuids: 消息 UUID 列表
    /// - status: 审批状态
    /// - resolved_at: 审批解决时间戳（毫秒）
    pub fn batch_update_approval_status(
        &self,
        uuids: &[String],
        status: crate::types::ApprovalStatus,
        resolved_at: i64,
    ) -> Result<usize> {
        if uuids.is_empty() {
            return Ok(0);
        }

        let conn = self.conn.lock();
        let placeholders: String = uuids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            r#"
            UPDATE messages
            SET approval_status = ?1, approval_resolved_at = ?2
            WHERE uuid IN ({})
            "#,
            placeholders
        );

        let status_str = status.to_string();
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(status_str), Box::new(resolved_at)];
        for uuid in uuids {
            params_vec.push(Box::new(uuid.clone()));
        }

        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = conn.prepare(&sql)?;
        let count = stmt.execute(params_refs.as_slice())?;
        Ok(count)
    }

    /// 统计待审批的消息数量
    /// - session_id: 可选的会话 ID，如果提供则只统计该会话的待审批消息
    pub fn count_pending_approvals(&self, session_id: Option<&str>) -> Result<i64> {
        let conn = self.conn.lock();
        let count = if let Some(sid) = session_id {
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE approval_status = 'pending' AND session_id = ?1",
                params![sid],
                |row| row.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE approval_status = 'pending'",
                [],
                |row| row.get(0),
            )?
        };
        Ok(count)
    }

    // ==================== 管理操作 ====================

    /// 统计缺少 cwd 的会话数量
    pub fn count_sessions_without_cwd(&self) -> Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE cwd IS NULL OR cwd = ''",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// 获取所有项目（带 source 字段）
    pub fn get_all_projects_with_source(&self) -> Result<Vec<ProjectWithSource>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, name, path, source FROM projects")?;

        let rows = stmt.query_map([], |row| {
            Ok(ProjectWithSource {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                source: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 更新会话的项目 ID
    pub fn update_sessions_project_id(
        &self,
        from_project_id: i64,
        to_project_id: i64,
    ) -> Result<usize> {
        let conn = self.conn.lock();
        let count = conn.execute(
            "UPDATE sessions SET project_id = ?1 WHERE project_id = ?2",
            params![to_project_id, from_project_id],
        )?;
        Ok(count)
    }

    /// 删除项目
    pub fn delete_project(&self, project_id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM projects WHERE id = ?1", params![project_id])?;
        Ok(())
    }

    /// 去重项目 - 按 path 合并，保留 session 最多的记录
    /// 返回 (合并数量, 删除的项目 ID 列表)
    pub fn deduplicate_projects(&self) -> Result<(usize, Vec<i64>)> {
        let conn = self.conn.lock();

        // 找出所有重复的 path（有多条记录）
        let mut stmt = conn.prepare(
            r#"
            SELECT path, GROUP_CONCAT(id) as ids
            FROM projects
            GROUP BY path
            HAVING COUNT(*) > 1
            "#,
        )?;

        let duplicates: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        drop(stmt);

        let mut merged_count = 0;
        let mut deleted_ids = Vec::new();

        for (path, ids_str) in duplicates {
            let ids: Vec<i64> = ids_str
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();

            if ids.len() < 2 {
                continue;
            }

            // 找出每个 project 的 session 数量
            let mut project_sessions: Vec<(i64, i64)> = Vec::new();
            for &id in &ids {
                let count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sessions WHERE project_id = ?1",
                        params![id],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                project_sessions.push((id, count));
            }

            // 按 session 数量降序排序，保留第一个（最多的）
            project_sessions.sort_by(|a, b| b.1.cmp(&a.1));
            let keep_id = project_sessions[0].0;

            // 合并其他 project 的 sessions 到保留的那个
            for &(id, _) in &project_sessions[1..] {
                conn.execute(
                    "UPDATE sessions SET project_id = ?1 WHERE project_id = ?2",
                    params![keep_id, id],
                )?;

                // 删除重复的 project
                conn.execute("DELETE FROM projects WHERE id = ?1", params![id])?;

                deleted_ids.push(id);
                merged_count += 1;
            }

            tracing::info!(
                "去重项目 path={}: 保留 ID {}, 删除 {:?}",
                path,
                keep_id,
                &project_sessions[1..]
                    .iter()
                    .map(|(id, _)| id)
                    .collect::<Vec<_>>()
            );
        }

        Ok((merged_count, deleted_ids))
    }
}

/// 带 source 的项目信息
#[derive(Debug, Clone)]
pub struct ProjectWithSource {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub source: String,
}

/// 会话输入 (写入用)
#[derive(Debug, Clone, Default)]
pub struct SessionInput {
    pub session_id: String,
    pub project_id: i64,
    // 会话元数据
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub channel: Option<String>,
    pub message_count: Option<i64>,
    // 增量检测字段
    pub file_mtime: Option<i64>,
    pub file_size: Option<i64>,
    // 额外元信息
    pub meta: Option<String>,
}

/// 消息输入 (写入用)
#[derive(Debug, Clone)]
pub struct MessageInput {
    pub uuid: String,
    pub r#type: MessageType,
    pub content_text: String, // 纯对话文本（用于向量化）
    pub content_full: String, // 完整格式化内容（用于 FTS）
    pub timestamp: i64,
    pub sequence: i64,
    pub source: Option<String>,
    pub channel: Option<String>,
    pub model: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_args: Option<String>,
    pub raw: Option<String>,
    pub approval_status: Option<crate::types::ApprovalStatus>, // 审批状态: pending, approved, rejected, timeout
    pub approval_resolved_at: Option<i64>,                     // 审批解决时间戳（毫秒）
}

/// 获取当前时间戳 (毫秒)
fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Drop for SessionDB {
    fn drop(&mut self) {
        #[cfg(feature = "coordination")]
        {
            if let Err(e) = self.release_writer() {
                tracing::warn!("释放 Writer 失败: {}", e);
            }
        }
    }
}
