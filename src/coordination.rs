//! Writer 协调逻辑
//!
//! 实现多组件共存时的 Writer 协调机制：
//! - 原子抢占
//! - 心跳维护
//! - 超时接管

use crate::error::{Error, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::watch;

/// 组件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterType {
    /// Memex daemon (优先级 1)
    MemexDaemon,
    /// Vlaude daemon (优先级 1)
    VlaudeDaemon,
    /// MemexKit ETerm 插件 (优先级 2，事件驱动更实时)
    MemexKit,
    /// VlaudeKit ETerm 插件 (优先级 2)
    VlaudeKit,
}

impl WriterType {
    /// 获取优先级 (数字越大优先级越高)
    ///
    /// 优先级说明：
    /// - Daemon (1): 后台服务，优先级最低
    /// - MemexKit (2): ETerm 插件，事件驱动
    /// - VlaudeKit (3): ETerm 插件，有 approval 实时写入需求，优先级最高
    pub fn priority(&self) -> i32 {
        match self {
            WriterType::MemexDaemon | WriterType::VlaudeDaemon => 1,
            WriterType::MemexKit => 2,
            WriterType::VlaudeKit => 3, // 最高优先级，有 approval 实时写入需求
        }
    }

    /// 获取类型名称
    pub fn name(&self) -> &'static str {
        match self {
            WriterType::MemexDaemon => "memex_daemon",
            WriterType::VlaudeDaemon => "vlaude_daemon",
            WriterType::MemexKit => "memex_kit",
            WriterType::VlaudeKit => "vlaude_kit",
        }
    }
}

impl std::fmt::Display for WriterType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// 角色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 负责写入
    Writer,
    /// 只读
    Reader,
}

/// Writer 健康状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterHealth {
    /// 心跳正常
    Alive,
    /// 心跳超时
    Timeout,
    /// 主动释放 (没有 Writer)
    Released,
}

/// 协调配置
#[derive(Debug, Clone)]
pub struct CoordinationConfig {
    /// 心跳周期 (毫秒)
    pub heartbeat_interval_ms: u64,
    /// 超时阈值 (毫秒)
    pub timeout_threshold_ms: u64,
    /// 确认次数 (连续多少次超时才确认)
    pub confirm_count: u32,
}

impl Default for CoordinationConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: 10_000, // 10s
            timeout_threshold_ms: 30_000,  // 30s
            confirm_count: 3,              // 3 次
        }
    }
}

/// Writer 协调器
pub struct Coordinator {
    writer_type: WriterType,
    writer_id: String,
    config: CoordinationConfig,
    role_tx: watch::Sender<Role>,
    role_rx: watch::Receiver<Role>,
}

impl Coordinator {
    /// 创建协调器
    pub fn new(writer_type: WriterType, config: CoordinationConfig) -> Self {
        let writer_id = uuid::Uuid::new_v4().to_string();
        let (role_tx, role_rx) = watch::channel(Role::Reader);

        Self {
            writer_type,
            writer_id,
            config,
            role_tx,
            role_rx,
        }
    }

    /// 获取 writer ID
    pub fn writer_id(&self) -> &str {
        &self.writer_id
    }

    /// 获取角色监听器
    pub fn watch_role(&self) -> watch::Receiver<Role> {
        self.role_rx.clone()
    }

    /// 获取当前角色
    pub fn current_role(&self) -> Role {
        *self.role_rx.borrow()
    }

    /// 尝试注册为 Writer (原子操作)
    ///
    /// 使用 INSERT ... ON CONFLICT 实现原子抢占
    pub fn try_register(&self, conn: &Connection) -> Result<Role> {
        let now = current_time_ms();
        let priority = self.writer_type.priority();
        let writer_type_name = self.writer_type.name();

        // 原子 upsert：只有在以下条件才能抢占
        // 1. 表为空（首次注册）
        // 2. 心跳超时 (>30s)
        // 3. 我优先级更高
        let rows_changed = conn.execute(
            r#"
            INSERT INTO writer_registry (id, writer_type, writer_id, priority, heartbeat, registered_at)
            VALUES (1, ?1, ?2, ?3, ?4, ?4)
            ON CONFLICT(id) DO UPDATE SET
                writer_type = excluded.writer_type,
                writer_id = excluded.writer_id,
                priority = excluded.priority,
                heartbeat = excluded.heartbeat,
                registered_at = excluded.registered_at
            WHERE
                -- 条件1: 心跳超时 (>30s)
                (?4 - writer_registry.heartbeat) > ?5
                -- 条件2: 或者我优先级更高
                OR excluded.priority > writer_registry.priority
            "#,
            params![
                writer_type_name,
                &self.writer_id,
                priority,
                now,
                self.config.timeout_threshold_ms as i64,
            ],
        )?;

        let role = if rows_changed > 0 {
            tracing::info!(
                "成为 Writer: type={}, id={}, priority={}",
                writer_type_name,
                &self.writer_id,
                priority
            );
            Role::Writer
        } else {
            tracing::info!(
                "成为 Reader: type={}, id={} (已有 Writer)",
                writer_type_name,
                &self.writer_id
            );
            Role::Reader
        };

        let _ = self.role_tx.send(role);
        Ok(role)
    }

    /// 更新心跳
    pub fn heartbeat(&self, conn: &Connection) -> Result<()> {
        let now = current_time_ms();

        let rows = conn.execute(
            "UPDATE writer_registry SET heartbeat = ?1 WHERE id = 1 AND writer_id = ?2",
            params![now, &self.writer_id],
        )?;

        if rows == 0 {
            // 我不再是 Writer 了（被抢占了）
            let _ = self.role_tx.send(Role::Reader);
            return Err(Error::Coordination("Writer 已被抢占".into()));
        }

        Ok(())
    }

    /// 释放 Writer (正常退出时调用)
    pub fn release(&self, conn: &Connection) -> Result<()> {
        let rows = conn.execute(
            "DELETE FROM writer_registry WHERE id = 1 AND writer_id = ?1",
            params![&self.writer_id],
        )?;

        if rows > 0 {
            tracing::info!("释放 Writer: id={}", &self.writer_id);
        }

        let _ = self.role_tx.send(Role::Reader);
        Ok(())
    }

    /// 检查 Writer 健康状态 (Reader 调用)
    pub fn check_writer_health(&self, conn: &Connection) -> Result<WriterHealth> {
        let now = current_time_ms();

        let result: Option<(i64, String)> = conn
            .query_row(
                "SELECT heartbeat, writer_id FROM writer_registry WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        match result {
            None => Ok(WriterHealth::Released),
            Some((heartbeat, _writer_id)) => {
                let elapsed = now - heartbeat;
                if elapsed > self.config.timeout_threshold_ms as i64 {
                    Ok(WriterHealth::Timeout)
                } else {
                    Ok(WriterHealth::Alive)
                }
            }
        }
    }

    /// 尝试接管 Writer (Reader 在检测到超时后调用)
    pub fn try_takeover(&self, conn: &Connection) -> Result<bool> {
        let now = current_time_ms();
        let priority = self.writer_type.priority();
        let writer_type_name = self.writer_type.name();

        // 情况1：心跳超时时接管
        let rows = conn.execute(
            r#"
            UPDATE writer_registry SET
                writer_type = ?1,
                writer_id = ?2,
                priority = ?3,
                heartbeat = ?4,
                registered_at = ?4
            WHERE id = 1
              AND (?4 - heartbeat) > ?5
            "#,
            params![
                writer_type_name,
                &self.writer_id,
                priority,
                now,
                self.config.timeout_threshold_ms as i64,
            ],
        )?;

        if rows > 0 {
            tracing::info!(
                "接管超时 Writer: type={}, id={}",
                writer_type_name,
                &self.writer_id
            );
            let _ = self.role_tx.send(Role::Writer);
            return Ok(true);
        }

        // 情况2：writer_registry 为空（Writer 已 release），直接插入
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM writer_registry WHERE id = 1",
            [],
            |row| row.get(0),
        )?;

        if count == 0 {
            conn.execute(
                r#"
                INSERT INTO writer_registry (id, writer_type, writer_id, priority, heartbeat, registered_at)
                VALUES (1, ?1, ?2, ?3, ?4, ?4)
                "#,
                params![writer_type_name, &self.writer_id, priority, now],
            )?;
            tracing::info!(
                "接管空位 Writer: type={}, id={}",
                writer_type_name,
                &self.writer_id
            );
            let _ = self.role_tx.send(Role::Writer);
            return Ok(true);
        }

        Ok(false)
    }

    /// 获取当前 Writer 信息
    pub fn get_current_writer(&self, conn: &Connection) -> Result<Option<WriterInfo>> {
        let result: Option<(String, String, i32, i64, i64)> = conn
            .query_row(
                "SELECT writer_type, writer_id, priority, heartbeat, registered_at FROM writer_registry WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;

        Ok(result.map(
            |(writer_type, writer_id, priority, heartbeat, registered_at)| WriterInfo {
                writer_type,
                writer_id,
                priority,
                heartbeat,
                registered_at,
            },
        ))
    }
}

/// Writer 信息
#[derive(Debug, Clone)]
pub struct WriterInfo {
    pub writer_type: String,
    pub writer_id: String,
    pub priority: i32,
    pub heartbeat: i64,
    pub registered_at: i64,
}

/// 获取当前时间戳 (毫秒)
fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::COORDINATION_SCHEMA_SQL;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(COORDINATION_SCHEMA_SQL).unwrap();
        conn
    }

    #[test]
    fn test_first_register_becomes_writer() {
        let conn = setup_db();
        let coord = Coordinator::new(WriterType::MemexDaemon, CoordinationConfig::default());

        let role = coord.try_register(&conn).unwrap();
        assert_eq!(role, Role::Writer);
    }

    #[test]
    fn test_second_register_same_priority_becomes_reader() {
        let conn = setup_db();

        let coord1 = Coordinator::new(WriterType::MemexDaemon, CoordinationConfig::default());
        let coord2 = Coordinator::new(WriterType::VlaudeDaemon, CoordinationConfig::default());

        let role1 = coord1.try_register(&conn).unwrap();
        let role2 = coord2.try_register(&conn).unwrap();

        assert_eq!(role1, Role::Writer);
        assert_eq!(role2, Role::Reader);
    }

    #[test]
    fn test_higher_priority_takes_over() {
        let conn = setup_db();

        let daemon = Coordinator::new(WriterType::MemexDaemon, CoordinationConfig::default());
        let kit = Coordinator::new(WriterType::MemexKit, CoordinationConfig::default());

        let role1 = daemon.try_register(&conn).unwrap();
        assert_eq!(role1, Role::Writer);

        // 高优先级抢占
        let role2 = kit.try_register(&conn).unwrap();
        assert_eq!(role2, Role::Writer);
    }

    #[test]
    fn test_release_and_takeover() {
        let conn = setup_db();

        let coord1 = Coordinator::new(WriterType::MemexDaemon, CoordinationConfig::default());
        let coord2 = Coordinator::new(WriterType::VlaudeDaemon, CoordinationConfig::default());

        coord1.try_register(&conn).unwrap();
        coord2.try_register(&conn).unwrap();

        // coord1 释放
        coord1.release(&conn).unwrap();

        // coord2 检测到 Released
        let health = coord2.check_writer_health(&conn).unwrap();
        assert_eq!(health, WriterHealth::Released);
    }
}
