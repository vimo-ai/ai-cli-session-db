//! Agent 集成测试

#[cfg(feature = "agent")]
mod tests {
    use ai_cli_session_db::agent::{Agent, AgentConfig};
    use ai_cli_session_db::protocol::{EventType, HookEvent, Push, Request, Response};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::time::sleep;

    /// 创建测试配置
    fn test_config() -> AgentConfig {
        let temp_dir = tempdir().unwrap();
        AgentConfig {
            data_dir: temp_dir.into_path(),
            idle_timeout_secs: 5,
        }
    }

    #[tokio::test]
    async fn test_agent_start_and_connect() {
        let config = test_config();
        let socket_path = config.socket_path();

        // 启动 Agent
        let agent = Arc::new(Agent::new(config.clone()).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                agent.run().await.unwrap();
            })
        };

        // 等待 Agent 启动
        sleep(Duration::from_millis(500)).await;

        // 连接 Agent
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // 发送握手
        let handshake = Request::Handshake {
            component: "test".to_string(),
            version: "1.0.0".to_string(),
        };
        let handshake_json = serde_json::to_string(&handshake).unwrap();
        writer
            .write_all(format!("{}\n", handshake_json).as_bytes())
            .await
            .unwrap();

        // 读取响应
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();

        match response {
            Response::HandshakeOk { agent_version } => {
                assert!(!agent_version.is_empty());
            }
            _ => panic!("Expected HandshakeOk"),
        }

        // 关闭连接
        drop(writer);
        drop(reader);

        // 停止 Agent
        agent_handle.abort();
    }

    #[tokio::test]
    async fn test_agent_subscribe() {
        let config = test_config();
        let socket_path = config.socket_path();

        // 启动 Agent
        let agent = Arc::new(Agent::new(config.clone()).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                agent.run().await.unwrap();
            })
        };

        sleep(Duration::from_millis(500)).await;

        // 连接并握手
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // 握手
        let handshake = Request::Handshake {
            component: "test".to_string(),
            version: "1.0.0".to_string(),
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&handshake).unwrap()).as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        // 订阅事件
        line.clear();
        let subscribe = Request::Subscribe {
            events: vec![EventType::NewMessage],
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
            .await
            .unwrap();

        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();

        assert!(matches!(response, Response::Ok));

        // 停止
        agent_handle.abort();
    }

    #[tokio::test]
    async fn test_protocol_serialization() {
        // 测试 Request 序列化
        let handshake = Request::Handshake {
            component: "test".to_string(),
            version: "1.0.0".to_string(),
        };
        let json = serde_json::to_string(&handshake).unwrap();
        assert!(json.contains("Handshake"));

        // 测试 Response 序列化
        let response = Response::HandshakeOk {
            agent_version: "0.1.0".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("HandshakeOk"));

        // 测试反序列化
        let parsed: Response = serde_json::from_str(&json).unwrap();
        match parsed {
            Response::HandshakeOk { agent_version } => {
                assert_eq!(agent_version, "0.1.0");
            }
            _ => panic!("Expected HandshakeOk"),
        }
    }

    #[tokio::test]
    async fn test_hook_event_request() {
        let config = test_config();
        let socket_path = config.socket_path();

        // 启动 Agent
        let agent = Arc::new(Agent::new(config.clone()).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                agent.run().await.unwrap();
            })
        };

        sleep(Duration::from_millis(500)).await;

        // 连接并握手
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // 握手
        let handshake = Request::Handshake {
            component: "test".to_string(),
            version: "1.0.0".to_string(),
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&handshake).unwrap()).as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        // 发送 HookEvent
        line.clear();
        let hook_event = HookEvent {
            event_type: "SessionStart".to_string(),
            session_id: "test-session-123".to_string(),
            transcript_path: None, // 不存在的路径，不触发 Collection
            cwd: Some("/test/project".to_string()),
            prompt: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            notification_type: None,
            message: None,
            context: Some(serde_json::json!({"terminal_id": 42})),
        };
        let request = Request::HookEvent(hook_event);
        writer
            .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
            .await
            .unwrap();

        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();

        // HookEvent 应返回 Ok
        assert!(matches!(response, Response::Ok));

        // 停止
        agent_handle.abort();
    }

    #[tokio::test]
    async fn test_hook_event_broadcast() {
        let config = test_config();
        let socket_path = config.socket_path();

        // 启动 Agent
        let agent = Arc::new(Agent::new(config.clone()).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                agent.run().await.unwrap();
            })
        };

        sleep(Duration::from_millis(500)).await;

        // 连接并握手
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // 握手
        let handshake = Request::Handshake {
            component: "test".to_string(),
            version: "1.0.0".to_string(),
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&handshake).unwrap()).as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        // 订阅 HookEvent
        line.clear();
        let subscribe = Request::Subscribe {
            events: vec![EventType::HookEvent],
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
            .await
            .unwrap();

        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert!(matches!(response, Response::Ok));

        // 发送 HookEvent
        line.clear();
        let hook_event = HookEvent {
            event_type: "UserPromptSubmit".to_string(),
            session_id: "test-session-456".to_string(),
            transcript_path: None,
            cwd: Some("/test/project".to_string()),
            prompt: Some("Hello, Claude!".to_string()),
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            notification_type: None,
            message: None,
            context: Some(serde_json::json!({"terminal_id": 123})),
        };
        let request = Request::HookEvent(hook_event.clone());
        writer
            .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
            .await
            .unwrap();

        // 读取消息直到收到 Push::HookEvent（Response 和 Push 顺序不确定）
        let mut found_hook_event = false;
        for _ in 0..5 {
            line.clear();
            reader.read_line(&mut line).await.unwrap();

            // 尝试解析为 Push
            if let Ok(push) = serde_json::from_str::<Push>(&line) {
                match push {
                    Push::HookEvent(received) => {
                        assert_eq!(received.event_type, "UserPromptSubmit");
                        assert_eq!(received.session_id, "test-session-456");
                        assert_eq!(received.prompt, Some("Hello, Claude!".to_string()));
                        found_hook_event = true;
                        break;
                    }
                    _ => continue,
                }
            }
            // 如果不是 Push，可能是 Response，继续读取
        }

        assert!(found_hook_event, "Should have received Push::HookEvent");

        // 停止
        agent_handle.abort();
    }

    #[tokio::test]
    async fn test_hook_event_serialization() {
        // 测试从 claude_hook.sh 发送的 JSON 格式
        let json = r#"{
            "type": "HookEvent",
            "event_type": "PermissionRequest",
            "session_id": "abc-123",
            "transcript_path": "/path/to/transcript.jsonl",
            "cwd": "/Users/test/project",
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
            "tool_use_id": "tool-456"
        }"#;

        let request: Request = serde_json::from_str(json).unwrap();
        match request {
            Request::HookEvent(event) => {
                assert_eq!(event.event_type, "PermissionRequest");
                assert_eq!(event.session_id, "abc-123");
                assert_eq!(event.tool_name, Some("Bash".to_string()));
                assert_eq!(event.tool_use_id, Some("tool-456".to_string()));
                assert!(event.tool_input.is_some());
            }
            _ => panic!("Expected HookEvent"),
        }
    }
}
