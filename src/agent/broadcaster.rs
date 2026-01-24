//! 事件广播器
//!
//! 维护订阅列表，将事件推送给订阅者

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::mpsc;

use crate::protocol::{Event, EventType};

/// 连接 ID
pub type ConnId = u64;

/// 消息发送通道
pub type MessageSender = mpsc::Sender<String>;

/// 事件广播器
pub struct Broadcaster {
    /// 订阅关系：ConnId → 订阅的事件类型
    subscriptions: RwLock<HashMap<ConnId, HashSet<EventType>>>,
    /// 连接通道：ConnId → 发送通道
    senders: RwLock<HashMap<ConnId, MessageSender>>,
    /// 下一个连接 ID
    next_conn_id: RwLock<ConnId>,
}

impl Broadcaster {
    /// 创建新的广播器
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subscriptions: RwLock::new(HashMap::new()),
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
        self.subscriptions.write().insert(conn_id, HashSet::new());

        tracing::debug!("📡 连接注册: conn_id={}", conn_id);
        conn_id
    }

    /// 注销连接
    pub fn unregister(&self, conn_id: ConnId) {
        self.senders.write().remove(&conn_id);
        self.subscriptions.write().remove(&conn_id);
        tracing::debug!("📡 连接注销: conn_id={}", conn_id);
    }

    /// 订阅事件
    pub fn subscribe(&self, conn_id: ConnId, events: Vec<EventType>) {
        if let Some(sub) = self.subscriptions.write().get_mut(&conn_id) {
            for event in &events {
                sub.insert(*event);
            }
            tracing::debug!("📡 订阅事件: conn_id={}, events={:?}", conn_id, events);
        }
    }

    /// 取消订阅
    pub fn unsubscribe(&self, conn_id: ConnId, events: Vec<EventType>) {
        if let Some(sub) = self.subscriptions.write().get_mut(&conn_id) {
            for event in &events {
                sub.remove(event);
            }
            tracing::debug!("📡 取消订阅: conn_id={}, events={:?}", conn_id, events);
        }
    }

    /// 广播事件给所有订阅者
    pub async fn broadcast(&self, event: Event) {
        let event_type = event.event_type();
        let push = event.to_push();

        // 序列化消息（JSONL 格式）
        let message = match serde_json::to_string(&push) {
            Ok(json) => format!("{}\n", json),
            Err(e) => {
                tracing::error!("序列化事件失败: {}", e);
                return;
            }
        };

        // 获取需要推送的连接
        let targets: Vec<(ConnId, MessageSender)> = {
            let subs = self.subscriptions.read();
            let senders = self.senders.read();

            subs.iter()
                .filter(|(_, subscribed)| subscribed.contains(&event_type))
                .filter_map(|(conn_id, _)| {
                    senders.get(conn_id).map(|s| (*conn_id, s.clone()))
                })
                .collect()
        };

        if targets.is_empty() {
            tracing::trace!("📡 无订阅者: event_type={:?}", event_type);
            return;
        }

        tracing::debug!(
            "📡 广播事件: event_type={:?}, 订阅者={}",
            event_type,
            targets.len()
        );

        // 并发发送
        for (conn_id, sender) in targets {
            let msg = message.clone();
            if sender.send(msg).await.is_err() {
                tracing::warn!("📡 发送失败，连接可能已断开: conn_id={}", conn_id);
                // 不在这里清理，由连接处理逻辑负责
            }
        }
    }

    /// 获取当前连接数
    pub fn connection_count(&self) -> usize {
        self.senders.read().len()
    }

    /// 检查是否有活跃连接
    pub fn has_connections(&self) -> bool {
        !self.senders.read().is_empty()
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

impl Default for Broadcaster {
    fn default() -> Self {
        Self {
            subscriptions: RwLock::new(HashMap::new()),
            senders: RwLock::new(HashMap::new()),
            next_conn_id: RwLock::new(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_broadcaster_subscribe_and_broadcast() {
        let broadcaster = Broadcaster::new();

        // 创建两个订阅者
        let (tx1, mut rx1) = mpsc::channel(10);
        let (tx2, mut rx2) = mpsc::channel(10);

        let conn1 = broadcaster.register(tx1);
        let conn2 = broadcaster.register(tx2);

        // conn1 只订阅 NewMessage
        broadcaster.subscribe(conn1, vec![EventType::NewMessage]);

        // conn2 订阅 NewMessage 和 SessionStart
        broadcaster.subscribe(conn2, vec![EventType::NewMessage, EventType::SessionStart]);

        // 广播 NewMessage
        broadcaster
            .broadcast(Event::NewMessages {
                session_id: "test-session".to_string(),
                path: PathBuf::from("/test/path"),
                count: 5,
                message_ids: vec![1, 2, 3, 4, 5],
            })
            .await;

        // 两个订阅者都应该收到
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());

        // 广播 SessionStart
        broadcaster
            .broadcast(Event::SessionStart {
                session_id: "test-session".to_string(),
                project_path: "/test/project".to_string(),
            })
            .await;

        // 只有 conn2 应该收到
        assert!(rx1.try_recv().is_err()); // conn1 没订阅 SessionStart
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn test_connection_count() {
        let broadcaster = Broadcaster::new();

        assert_eq!(broadcaster.connection_count(), 0);

        let (tx1, _rx1) = mpsc::channel(10);
        let conn1 = broadcaster.register(tx1);
        assert_eq!(broadcaster.connection_count(), 1);

        let (tx2, _rx2) = mpsc::channel(10);
        let _conn2 = broadcaster.register(tx2);
        assert_eq!(broadcaster.connection_count(), 2);

        broadcaster.unregister(conn1);
        assert_eq!(broadcaster.connection_count(), 1);
    }
}
