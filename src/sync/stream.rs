//! WebSocket streaming client for sync

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite};

use super::{SyncAck, SyncBatch, SyncCursor, SyncFrame};

pub struct SyncStream {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl SyncStream {
    pub async fn connect(server_url: &str) -> Result<Self> {
        let ws_url = http_to_ws_url(server_url);
        let (ws, _response) = connect_async(&ws_url)
            .await
            .with_context(|| format!("ws connect failed: {ws_url}"))?;
        Ok(Self { ws })
    }

    pub async fn authenticate(&mut self, device_id: &str, api_key: &str) -> Result<()> {
        let frame = SyncFrame::Auth {
            device_id: device_id.to_string(),
            api_key: api_key.to_string(),
        };
        self.send_frame(&frame).await?;

        let ack = self.recv_ack().await?;
        match ack {
            SyncAck::AuthOk { server_version } => {
                tracing::info!("ws auth ok, server version: {server_version}");
                Ok(())
            }
            SyncAck::AuthFail { message } => {
                anyhow::bail!("ws auth failed: {message}")
            }
            other => {
                anyhow::bail!("unexpected auth response: {other:?}")
            }
        }
    }

    pub async fn push(
        &mut self,
        batch: &SyncBatch,
        cursors: &std::collections::HashMap<String, SyncCursor>,
    ) -> Result<SyncAck> {
        let frame = SyncFrame::Push {
            batch: batch.clone(),
            cursors: cursors.clone(),
        };
        self.send_frame(&frame).await?;
        self.recv_ack().await
    }

    pub async fn heartbeat(&mut self, last_db_updated_at: i64) -> Result<()> {
        let frame = SyncFrame::Heartbeat { last_db_updated_at };
        self.send_frame(&frame).await?;
        let _ack = self.recv_ack().await?;
        Ok(())
    }

    async fn send_frame(&mut self, frame: &SyncFrame) -> Result<()> {
        let json = serde_json::to_string(frame)?;
        self.ws
            .send(tungstenite::Message::Text(json.into()))
            .await
            .context("ws send failed")
    }

    async fn recv_ack(&mut self) -> Result<SyncAck> {
        loop {
            let msg = self
                .ws
                .next()
                .await
                .ok_or_else(|| anyhow::anyhow!("ws connection closed"))?
                .context("ws recv error")?;

            match msg {
                tungstenite::Message::Text(text) => {
                    return serde_json::from_str(&text).context("parse SyncAck failed");
                }
                tungstenite::Message::Close(_) => {
                    anyhow::bail!("ws connection closed by server");
                }
                _ => continue,
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

fn http_to_ws_url(server: &str) -> String {
    let base = server.trim_end_matches('/');
    let ws_base = if base.starts_with("https://") {
        base.replacen("https://", "wss://", 1)
    } else if base.starts_with("http://") {
        base.replacen("http://", "ws://", 1)
    } else {
        format!("ws://{base}")
    };
    format!("{ws_base}/api/sync/stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_to_ws_url() {
        assert_eq!(
            http_to_ws_url("http://localhost:10013"),
            "ws://localhost:10013/api/sync/stream"
        );
        assert_eq!(
            http_to_ws_url("https://nas.example.com:10013/"),
            "wss://nas.example.com:10013/api/sync/stream"
        );
        assert_eq!(
            http_to_ws_url("192.168.1.100:10013"),
            "ws://192.168.1.100:10013/api/sync/stream"
        );
    }
}
