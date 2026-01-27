//! 请求处理器
//!
//! 处理来自客户端的各类请求

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::broadcaster::{Broadcaster, ConnId};
use super::watcher::FileWatcher;
use crate::protocol::{Event, HookEvent, QueryType, Request, Response};
use crate::SessionDB;

/// Agent 版本号（跟随 crate 版本）
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 请求处理器
pub struct Handler {
    /// 数据库连接
    db: Arc<SessionDB>,
    /// 广播器
    broadcaster: Arc<Broadcaster>,
    /// 文件监听器
    watcher: Arc<FileWatcher>,
}

impl Handler {
    /// 创建处理器
    pub fn new(db: Arc<SessionDB>, broadcaster: Arc<Broadcaster>, watcher: Arc<FileWatcher>) -> Self {
        Self {
            db,
            broadcaster,
            watcher,
        }
    }

    /// 处理请求
    pub async fn handle(&self, conn_id: ConnId, request: Request) -> Response {
        match request {
            Request::Handshake { component, version } => {
                tracing::info!(
                    "🤝 握手: conn_id={}, component={}, version={}",
                    conn_id,
                    component,
                    version
                );
                Response::HandshakeOk {
                    agent_version: AGENT_VERSION.to_string(),
                }
            }

            Request::NotifyFileChange { path } => {
                self.handle_file_change(path).await
            }

            Request::Subscribe { events } => {
                self.broadcaster.subscribe(conn_id, events);
                Response::Ok
            }

            Request::Unsubscribe { events } => {
                self.broadcaster.unsubscribe(conn_id, events);
                Response::Ok
            }

            Request::WriteIndexResult {
                session_id,
                indexed_message_ids,
            } => {
                self.handle_write_index_result(&session_id, &indexed_message_ids)
            }

            Request::WriteCompactResult {
                session_id,
                talk_id,
                summary_l2,
                summary_l3,
            } => {
                self.handle_write_compact_result(&session_id, &talk_id, &summary_l2, summary_l3.as_deref())
            }

            Request::WriteApproveResult {
                tool_call_id,
                status,
                resolved_at,
            } => {
                self.handle_write_approve_result(&tool_call_id, status, resolved_at)
            }

            Request::Heartbeat => Response::Ok,

            Request::Query { query_type } => {
                self.handle_query(query_type)
            }

            Request::HookEvent(hook_event) => {
                self.handle_hook_event(hook_event).await
            }
        }
    }

    /// 处理文件变化通知
    async fn handle_file_change(&self, path: PathBuf) -> Response {
        tracing::debug!("📝 Received file change notification: {:?}", path);

        // 触发即时 collection
        if let Err(e) = self.watcher.trigger_collect(&path).await {
            tracing::error!("Failed to process file change: {}", e);
            return Response::Error {
                code: 500,
                message: format!("Collection failed: {}", e),
            };
        }

        Response::Ok
    }

    /// 处理写入 Index 结果
    fn handle_write_index_result(&self, session_id: &str, indexed_message_ids: &[i64]) -> Response {
        tracing::debug!(
            "📊 写入 Index 结果: session_id={}, count={}",
            session_id,
            indexed_message_ids.len()
        );

        // 更新消息的 indexed 状态
        match self.db.mark_messages_indexed(indexed_message_ids) {
            Ok(_) => Response::Ok,
            Err(e) => {
                tracing::error!("Failed to write index result: {}", e);
                Response::Error {
                    code: 500,
                    message: format!("Failed to mark messages indexed: {}", e),
                }
            }
        }
    }

    /// 处理写入 Compact 结果
    fn handle_write_compact_result(
        &self,
        session_id: &str,
        talk_id: &str,
        summary_l2: &str,
        summary_l3: Option<&str>,
    ) -> Response {
        tracing::debug!(
            "📝 写入 Compact 结果: session_id={}, talk_id={}",
            session_id,
            talk_id
        );

        // 写入 Talk 摘要
        match self.db.upsert_talk_summary(session_id, talk_id, summary_l2, summary_l3) {
            Ok(_) => Response::Ok,
            Err(e) => {
                tracing::error!("Failed to write compact result: {}", e);
                Response::Error {
                    code: 500,
                    message: format!("Failed to write compact result: {}", e),
                }
            }
        }
    }

    /// 处理写入 Approve 结果
    fn handle_write_approve_result(
        &self,
        tool_call_id: &str,
        status: crate::protocol::ApprovalStatus,
        resolved_at: i64,
    ) -> Response {
        tracing::debug!(
            "✅ 写入 Approve 结果: tool_call_id={}, status={:?}",
            tool_call_id,
            status
        );

        let db_status = match status {
            crate::protocol::ApprovalStatus::Pending => crate::types::ApprovalStatus::Pending,
            crate::protocol::ApprovalStatus::Approved => crate::types::ApprovalStatus::Approved,
            crate::protocol::ApprovalStatus::Rejected => crate::types::ApprovalStatus::Rejected,
            crate::protocol::ApprovalStatus::Timeout => crate::types::ApprovalStatus::Timeout,
        };

        match self.db.update_approval_status_by_tool_call_id(tool_call_id, db_status, resolved_at) {
            Ok(_) => Response::Ok,
            Err(e) => {
                tracing::error!("Failed to write approval result: {}", e);
                Response::Error {
                    code: 500,
                    message: format!("Failed to update approval status: {}", e),
                }
            }
        }
    }

    /// 处理查询
    fn handle_query(&self, query_type: QueryType) -> Response {
        match query_type {
            QueryType::Status => {
                let status = serde_json::json!({
                    "agent_version": AGENT_VERSION,
                    "connections": self.broadcaster.connection_count(),
                });
                Response::QueryResult { data: status }
            }
            QueryType::ConnectionCount => {
                let count = self.broadcaster.connection_count();
                Response::QueryResult {
                    data: serde_json::json!({ "count": count }),
                }
            }
        }
    }

    /// 处理 Hook 事件
    ///
    /// 1. 如果有 transcript_path，触发即时 Collection
    /// 2. 广播 HookEvent 给订阅者（用于 UI 即时反馈）
    async fn handle_hook_event(&self, event: HookEvent) -> Response {
        tracing::debug!(
            "🪝 HookEvent: type={}, session_id={}",
            event.event_type,
            event.session_id
        );

        // 如果有 transcript_path，触发即时 Collection
        if let Some(ref path_str) = event.transcript_path {
            let path = Path::new(path_str);
            if path.exists() {
                if let Err(e) = self.watcher.trigger_collect(path).await {
                    tracing::warn!("HookEvent collection failed: {}", e);
                    // 不返回错误，继续广播事件
                }
            } else {
                tracing::debug!("HookEvent transcript_path not found: {}", path_str);
            }
        }

        // 广播 HookEvent 给订阅者
        self.broadcaster.broadcast(Event::HookEvent(event)).await;

        Response::Ok
    }
}
