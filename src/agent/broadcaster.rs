//! 连接管理器
//!
//! 维护活跃连接的通道，用于发送响应消息

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::mpsc;

/// 连接 ID
pub type ConnId = u64;

/// 消息发送通道
pub type MessageSender = mpsc::Sender<String>;

/// 连接管理器
pub struct ConnectionManager {
    /// 连接通道：ConnId → 发送通道
    senders: RwLock<HashMap<ConnId, MessageSender>>,
    /// 下一个连接 ID
    next_conn_id: RwLock<ConnId>,
}

impl ConnectionManager {
    /// 创建新的连接管理器
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            senders: RwLock::new(HashMap::new()),
            next_conn_id: RwLock::new(1),
        })
    }

    /// 注册新连接，返回连接 ID
    pub fn register(&self, sender: MessageSender) -> ConnId {
        let mut next_id = self.next_conn_id.write();
        let conn_id = *next_id;
        *next_id += 1;

        self.senders.write().insert(conn_id, sender);

        tracing::debug!("📡 Connection registered: conn_id={}", conn_id);
        conn_id
    }

    /// 注销连接
    pub fn unregister(&self, conn_id: ConnId) {
        self.senders.write().remove(&conn_id);
        tracing::debug!("📡 Connection unregistered: conn_id={}", conn_id);
    }

    /// 获取当前连接数
    pub fn connection_count(&self) -> usize {
        self.senders.read().len()
    }

    /// 清理已断开的连接，返回剩余连接数
    pub fn cleanup_closed(&self) -> usize {
        let mut senders = self.senders.write();
        let before = senders.len();
        senders.retain(|id, sender| {
            let alive = !sender.is_closed();
            if !alive {
                tracing::debug!("📡 Cleaning up dead connection: conn_id={}", id);
            }
            alive
        });
        let after = senders.len();
        if before != after {
            tracing::info!("📡 Cleaned {} dead connections, {} remaining", before - after, after);
        }
        after
    }

    /// 检查是否有活跃连接（同时清理死连接）
    pub fn has_connections(&self) -> bool {
        self.cleanup_closed() > 0
    }

    /// 发送消息到指定连接
    pub async fn send_to(&self, conn_id: ConnId, message: String) -> bool {
        // 先获取 sender 的 clone，然后释放锁
        let sender = {
            let senders = self.senders.read();
            senders.get(&conn_id).cloned()
        };

        if let Some(sender) = sender {
            sender.send(message).await.is_ok()
        } else {
            false
        }
    }

    /// 尝试发送消息到指定连接（非阻塞）
    pub fn try_send_to(&self, conn_id: ConnId, message: String) -> bool {
        let sender = {
            let senders = self.senders.read();
            senders.get(&conn_id).cloned()
        };

        if let Some(sender) = sender {
            sender.try_send(message).is_ok()
        } else {
            false
        }
    }
}

impl Default for ConnectionManager {
    fn default() -> Self {
        Self {
            senders: RwLock::new(HashMap::new()),
            next_conn_id: RwLock::new(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_count() {
        let manager = ConnectionManager::new();

        assert_eq!(manager.connection_count(), 0);

        let (tx1, _rx1) = mpsc::channel(10);
        let conn1 = manager.register(tx1);
        assert_eq!(manager.connection_count(), 1);

        let (tx2, _rx2) = mpsc::channel(10);
        let _conn2 = manager.register(tx2);
        assert_eq!(manager.connection_count(), 2);

        manager.unregister(conn1);
        assert_eq!(manager.connection_count(), 1);
    }
}
