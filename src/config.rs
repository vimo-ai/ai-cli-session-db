//! 数据库配置

use std::path::PathBuf;

/// 数据库连接配置
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// 连接 URL
    /// - 本地: "sqlite:///path/to/db.sqlite" 或直接路径
    /// - 远程: "libsql://host:port" (未来支持)
    /// - Turso: "libsql://xxx.turso.io?authToken=xxx" (未来支持)
    pub url: String,

    /// 连接模式
    pub mode: ConnectionMode,
}

/// 连接模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionMode {
    /// 本地 SQLite 文件
    Local,
    /// 远程 libSQL (未来支持)
    Remote,
}

impl DbConfig {
    /// 创建本地 SQLite 配置
    pub fn local<P: Into<PathBuf>>(path: P) -> Self {
        let path = path.into();
        Self {
            url: path.display().to_string(),
            mode: ConnectionMode::Local,
        }
    }

    /// 从环境变量或默认路径创建配置
    pub fn from_env() -> Self {
        if let Ok(url) = std::env::var("CLAUDE_SESSION_DB_URL") {
            if url.starts_with("libsql://") {
                return Self {
                    url,
                    mode: ConnectionMode::Remote,
                };
            }
            return Self::local(url);
        }

        // 默认路径: ~/.vimo/db/ai-cli-session.db
        let default_path = dirs::home_dir()
            .map(|h| h.join(".vimo").join("db").join("ai-cli-session.db"))
            .unwrap_or_else(|| PathBuf::from("ai-cli-session.db"));

        Self::local(default_path)
    }

    /// 获取数据库文件路径 (仅本地模式)
    pub fn path(&self) -> Option<PathBuf> {
        match self.mode {
            ConnectionMode::Local => Some(PathBuf::from(&self.url)),
            ConnectionMode::Remote => None,
        }
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        Self::from_env()
    }
}
