//! 错误类型定义

use thiserror::Error;

/// 库错误类型
#[derive(Error, Debug)]
pub enum Error {
    /// 数据库错误
    #[error("数据库错误: {0}")]
    Database(#[from] rusqlite::Error),

    /// IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 序列化错误
    #[error("序列化错误: {0}")]
    Serialization(#[from] serde_json::Error),

    /// 协调错误
    #[error("Writer 协调错误: {0}")]
    Coordination(String),

    /// 配置错误
    #[error("配置错误: {0}")]
    Config(String),

    /// 连接错误
    #[error("连接错误: {0}")]
    Connection(String),

    /// 权限错误 (Reader 尝试写入)
    #[error("权限错误: 当前角色为 Reader，无法执行写入操作")]
    PermissionDenied,

    /// 其他错误
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

/// Result 类型别名
pub type Result<T> = std::result::Result<T, Error>;
