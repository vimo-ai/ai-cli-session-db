//! 集成测试

use ai_cli_session_db::db::MessageInput;
use ai_cli_session_db::*;
use tempfile::TempDir;

/// 创建临时数据库
fn setup_db() -> (SessionDB, TempDir) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let config = DbConfig::local(&db_path);
    let db = SessionDB::connect(config).unwrap();
    (db, tmp)
}

// ==================== DB 连接测试 ====================

mod connection_tests {
    use super::*;

    #[test]
    fn test_connect_creates_db_file() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("subdir").join("test.db");

        // 目录不存在
        assert!(!db_path.parent().unwrap().exists());

        let config = DbConfig::local(&db_path);
        let _db = SessionDB::connect(config).unwrap();

        // 连接后文件应该存在
        assert!(db_path.exists());
    }

    #[test]
    fn test_connect_existing_db() {
        let (db1, tmp) = setup_db();
        drop(db1);

        // 重新连接同一个数据库
        let db_path = tmp.path().join("test.db");
        let config = DbConfig::local(&db_path);
        let db2 = SessionDB::connect(config).unwrap();

        // 应该能正常工作
        let stats = db2.get_stats().unwrap();
        assert_eq!(stats.project_count, 0);
    }

    #[test]
    fn test_default_config_from_env() {
        // 不设置环境变量时应该有默认值
        let config = DbConfig::from_env();
        assert!(config.path().is_some());
    }
}

// ==================== Project 测试 ====================

mod project_tests {
    use super::*;

    #[test]
    fn test_create_project() {
        let (db, _tmp) = setup_db();

        let id = db
            .get_or_create_project("test-project", "/path/to/project", "claude")
            .unwrap();
        assert!(id > 0);

        let projects = db.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "test-project");
        assert_eq!(projects[0].path, "/path/to/project");
        assert_eq!(projects[0].source, "claude");
    }

    #[test]
    fn test_get_existing_project() {
        let (db, _tmp) = setup_db();

        let id1 = db.get_or_create_project("test", "/path", "claude").unwrap();
        let id2 = db.get_or_create_project("test", "/path", "claude").unwrap();

        // 应该返回同一个 ID
        assert_eq!(id1, id2);

        // 只有一个项目
        let projects = db.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
    }

    #[test]
    fn test_multiple_projects() {
        let (db, _tmp) = setup_db();

        db.get_or_create_project("project1", "/path1", "claude")
            .unwrap();
        db.get_or_create_project("project2", "/path2", "claude")
            .unwrap();
        db.get_or_create_project("project3", "/path3", "codex")
            .unwrap();

        let projects = db.list_projects().unwrap();
        assert_eq!(projects.len(), 3);
    }

    #[test]
    fn test_project_different_path_same_name() {
        let (db, _tmp) = setup_db();

        // 同名但不同路径应该创建两个项目
        let id1 = db
            .get_or_create_project("test", "/path1", "claude")
            .unwrap();
        let id2 = db
            .get_or_create_project("test", "/path2", "claude")
            .unwrap();

        assert_ne!(id1, id2);
    }
}

// ==================== Session 测试 ====================

mod session_tests {
    use super::*;

    #[test]
    fn test_create_session() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        let sessions = db.list_sessions(project_id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session-001");
        assert_eq!(sessions[0].message_count, 0);
    }

    #[test]
    fn test_upsert_session_updates_timestamp() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();

        db.upsert_session("session-001", project_id).unwrap();
        let sessions1 = db.list_sessions(project_id).unwrap();
        let updated1 = sessions1[0].updated_at;

        // 等待一下确保时间戳不同
        std::thread::sleep(std::time::Duration::from_millis(10));

        db.upsert_session("session-001", project_id).unwrap();
        let sessions2 = db.list_sessions(project_id).unwrap();
        let updated2 = sessions2[0].updated_at;

        assert!(updated2 >= updated1);
    }

    #[test]
    fn test_multiple_sessions_per_project() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();

        db.upsert_session("session-001", project_id).unwrap();
        db.upsert_session("session-002", project_id).unwrap();
        db.upsert_session("session-003", project_id).unwrap();

        let sessions = db.list_sessions(project_id).unwrap();
        assert_eq!(sessions.len(), 3);
    }

    #[test]
    fn test_scan_checkpoint() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        // 初始检查点为 None
        let checkpoint = db.get_scan_checkpoint("session-001").unwrap();
        assert!(checkpoint.is_none());

        // 更新检查点
        db.update_session_last_message("session-001", 1234567890)
            .unwrap();

        let checkpoint = db.get_scan_checkpoint("session-001").unwrap();
        assert_eq!(checkpoint, Some(1234567890));
    }
}

// ==================== Message 测试 ====================

mod message_tests {
    use super::*;

    fn create_test_messages(count: usize) -> Vec<MessageInput> {
        (0..count)
            .map(|i| MessageInput {
                uuid: format!("uuid-{}", i),
                r#type: if i % 2 == 0 {
                    MessageType::User
                } else {
                    MessageType::Assistant
                },
                content_text: format!("Message content {}", i),
                content_full: format!("Message content {}", i),
                timestamp: 1000000 + i as i64,
                sequence: i as i64,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            })
            .collect()
    }

    #[test]
    fn test_insert_messages() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        let messages = create_test_messages(5);
        let inserted = db.insert_messages("session-001", &messages).unwrap();

        assert_eq!(inserted, 5);

        // 验证 session 的 message_count 更新了
        let sessions = db.list_sessions(project_id).unwrap();
        assert_eq!(sessions[0].message_count, 5);
    }

    #[test]
    fn test_insert_messages_dedup() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        let messages = create_test_messages(3);

        // 第一次插入
        let inserted1 = db.insert_messages("session-001", &messages).unwrap();
        assert_eq!(inserted1, 3);

        // 第二次插入相同的消息（应该被去重）
        let inserted2 = db.insert_messages("session-001", &messages).unwrap();
        assert_eq!(inserted2, 0);

        // 总数还是 3
        let all_messages = db.list_messages("session-001", 100, 0).unwrap();
        assert_eq!(all_messages.len(), 3);
    }

    #[test]
    fn test_list_messages_pagination() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        let messages = create_test_messages(10);
        db.insert_messages("session-001", &messages).unwrap();

        // 分页获取
        let page1 = db.list_messages("session-001", 3, 0).unwrap();
        let page2 = db.list_messages("session-001", 3, 3).unwrap();
        let page3 = db.list_messages("session-001", 3, 6).unwrap();
        let page4 = db.list_messages("session-001", 3, 9).unwrap();

        assert_eq!(page1.len(), 3);
        assert_eq!(page2.len(), 3);
        assert_eq!(page3.len(), 3);
        assert_eq!(page4.len(), 1);

        // 验证顺序
        assert_eq!(page1[0].sequence, 0);
        assert_eq!(page2[0].sequence, 3);
    }

    #[test]
    fn test_message_types() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        let messages = vec![
            MessageInput {
                uuid: "uuid-1".to_string(),
                r#type: MessageType::User,
                content_text: "Hello".to_string(),
                content_full: "Hello".to_string(),
                timestamp: 1000,
                sequence: 0,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
            MessageInput {
                uuid: "uuid-2".to_string(),
                r#type: MessageType::Assistant,
                content_text: "Hi there".to_string(),
                content_full: "Hi there".to_string(),
                timestamp: 1001,
                sequence: 1,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
        ];

        db.insert_messages("session-001", &messages).unwrap();

        let loaded = db.list_messages("session-001", 10, 0).unwrap();
        assert_eq!(loaded[0].r#type, MessageType::User);
        assert_eq!(loaded[1].r#type, MessageType::Assistant);
    }
}

// ==================== 增量扫描测试 ====================

mod incremental_scan_tests {
    use super::*;

    #[test]
    fn test_incremental_scan_first_time() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();

        let messages: Vec<MessageInput> = (0..5)
            .map(|i| MessageInput {
                uuid: format!("uuid-{}", i),
                r#type: MessageType::User,
                content_text: format!("Content {}", i),
                content_full: format!("Content {}", i),
                timestamp: 1000000 + i as i64 * 1000,
                sequence: i as i64,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            })
            .collect();

        // 首次扫描应该全量插入
        let inserted = db
            .scan_session_incremental("session-001", project_id, messages)
            .unwrap();
        assert_eq!(inserted, 5);
    }

    #[test]
    fn test_incremental_scan_with_checkpoint() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();

        // 第一批消息
        let messages1: Vec<MessageInput> = (0..3)
            .map(|i| MessageInput {
                uuid: format!("uuid-{}", i),
                r#type: MessageType::User,
                content_text: format!("Content {}", i),
                content_full: format!("Content {}", i),
                timestamp: 1000000 + i as i64 * 1000,
                sequence: i as i64,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            })
            .collect();

        db.scan_session_incremental("session-001", project_id, messages1)
            .unwrap();

        // 第二批消息（包含旧消息 + 新消息）
        let messages2: Vec<MessageInput> = (0..6)
            .map(|i| MessageInput {
                uuid: format!("uuid-{}", i),
                r#type: MessageType::User,
                content_text: format!("Content {}", i),
                content_full: format!("Content {}", i),
                timestamp: 1000000 + i as i64 * 1000,
                sequence: i as i64,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            })
            .collect();

        // 增量扫描应该只插入新消息
        let inserted = db
            .scan_session_incremental("session-001", project_id, messages2)
            .unwrap();
        assert_eq!(inserted, 3); // uuid-3, uuid-4, uuid-5

        // 总数应该是 6
        let all = db.list_messages("session-001", 100, 0).unwrap();
        assert_eq!(all.len(), 6);
    }

    #[test]
    fn test_incremental_scan_safety_margin() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();

        // 插入初始消息，设置检查点
        let messages1 = vec![MessageInput {
            uuid: "uuid-1".to_string(),
            r#type: MessageType::User,
            content_text: "First".to_string(),
            content_full: "First".to_string(),
            timestamp: 1000000,
            sequence: 0,
            source: None,
            channel: None,
            model: None,
            tool_call_id: None,
            tool_name: None,
            tool_args: None,
            raw: None,
            approval_status: None,
            approval_resolved_at: None,
        }];
        db.scan_session_incremental("session-001", project_id, messages1)
            .unwrap();

        // 在安全边界内的消息应该被重新检查（但因为 UUID 相同会去重）
        let messages2 = vec![
            MessageInput {
                uuid: "uuid-1".to_string(), // 旧消息
                r#type: MessageType::User,
                content_text: "First".to_string(),
                content_full: "First".to_string(),
                timestamp: 1000000,
                sequence: 0,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
            MessageInput {
                uuid: "uuid-2".to_string(), // 新消息，但时间戳在检查点附近
                r#type: MessageType::User,
                content_text: "Second".to_string(),
                content_full: "Second".to_string(),
                timestamp: 1000000 - 30000, // 检查点前 30 秒（在安全边界 60s 内）
                sequence: 1,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
        ];

        let inserted = db
            .scan_session_incremental("session-001", project_id, messages2)
            .unwrap();
        // uuid-1 去重，uuid-2 在安全边界内会被处理但因为时间戳早于 cutoff 所以不会被插入
        // 实际上 cutoff = checkpoint - 60s = 1000000 - 60000 = 940000
        // uuid-2 的 timestamp = 970000 > 940000，所以会被处理并插入
        assert_eq!(inserted, 1);
    }
}

// ==================== 搜索测试 ====================

#[cfg(feature = "search")]
mod search_tests {
    use super::*;

    #[test]
    fn test_fts_search_basic() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        let messages = vec![
            MessageInput {
                uuid: "uuid-1".to_string(),
                r#type: MessageType::User,
                content_text: "How do I implement a binary search tree in Rust?".to_string(),
                content_full: "How do I implement a binary search tree in Rust?".to_string(),
                timestamp: 1000,
                sequence: 0,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
            MessageInput {
                uuid: "uuid-2".to_string(),
                r#type: MessageType::Assistant,
                content_text: "Here's how to implement a binary search tree...".to_string(),
                content_full: "Here's how to implement a binary search tree...".to_string(),
                timestamp: 1001,
                sequence: 1,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
            MessageInput {
                uuid: "uuid-3".to_string(),
                r#type: MessageType::User,
                content_text: "What about hash maps?".to_string(),
                content_full: "What about hash maps?".to_string(),
                timestamp: 1002,
                sequence: 2,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
        ];

        db.insert_messages("session-001", &messages).unwrap();

        // 搜索 "binary"
        let results = db.search_fts("binary", 10).unwrap();
        assert_eq!(results.len(), 2);

        // 搜索 "hash"
        let results = db.search_fts("hash", 10).unwrap();
        assert_eq!(results.len(), 1);

        // 搜索不存在的词
        let results = db.search_fts("nonexistent", 10).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_fts_search_with_project_filter() {
        let (db, _tmp) = setup_db();

        let project1 = db
            .get_or_create_project("project1", "/path1", "claude")
            .unwrap();
        let project2 = db
            .get_or_create_project("project2", "/path2", "claude")
            .unwrap();

        db.upsert_session("session-1", project1).unwrap();
        db.upsert_session("session-2", project2).unwrap();

        db.insert_messages(
            "session-1",
            &[MessageInput {
                uuid: "uuid-1".to_string(),
                r#type: MessageType::User,
                content_text: "Rust programming".to_string(),
                content_full: "Rust programming".to_string(),
                timestamp: 1000,
                sequence: 0,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            }],
        )
        .unwrap();

        db.insert_messages(
            "session-2",
            &[MessageInput {
                uuid: "uuid-2".to_string(),
                r#type: MessageType::User,
                content_text: "Rust programming".to_string(),
                content_full: "Rust programming".to_string(),
                timestamp: 1000,
                sequence: 0,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            }],
        )
        .unwrap();

        // 不带过滤，应该找到 2 条
        let results = db.search_fts("Rust", 10).unwrap();
        assert_eq!(results.len(), 2);

        // 带项目过滤，应该只找到 1 条
        let results = db
            .search_fts_with_project("Rust", 10, Some(project1))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].project_id, project1);
    }

    #[test]
    fn test_fts_search_limit() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        // 插入 10 条包含 "test" 的消息
        let messages: Vec<MessageInput> = (0..10)
            .map(|i| MessageInput {
                uuid: format!("uuid-{}", i),
                r#type: MessageType::User,
                content_text: format!("This is test message {}", i),
                content_full: format!("This is test message {}", i),
                timestamp: 1000 + i as i64,
                sequence: i as i64,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            })
            .collect();

        db.insert_messages("session-001", &messages).unwrap();

        // limit 3
        let results = db.search_fts("test", 3).unwrap();
        assert_eq!(results.len(), 3);

        // limit 100
        let results = db.search_fts("test", 100).unwrap();
        assert_eq!(results.len(), 10);
    }
}

// ==================== 统计测试 ====================

mod stats_tests {
    use super::*;

    #[test]
    fn test_stats_empty_db() {
        let (db, _tmp) = setup_db();

        let stats = db.get_stats().unwrap();
        assert_eq!(stats.project_count, 0);
        assert_eq!(stats.session_count, 0);
        assert_eq!(stats.message_count, 0);
    }

    #[test]
    fn test_stats_with_data() {
        let (db, _tmp) = setup_db();

        // 创建数据
        let p1 = db.get_or_create_project("p1", "/p1", "claude").unwrap();
        let p2 = db.get_or_create_project("p2", "/p2", "claude").unwrap();

        db.upsert_session("s1", p1).unwrap();
        db.upsert_session("s2", p1).unwrap();
        db.upsert_session("s3", p2).unwrap();

        let messages: Vec<MessageInput> = (0..5)
            .map(|i| MessageInput {
                uuid: format!("uuid-{}", i),
                r#type: MessageType::User,
                content_text: "test".to_string(),
                content_full: "test".to_string(),
                timestamp: i as i64,
                sequence: i as i64,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            })
            .collect();

        db.insert_messages("s1", &messages).unwrap();

        let stats = db.get_stats().unwrap();
        assert_eq!(stats.project_count, 2);
        assert_eq!(stats.session_count, 3);
        assert_eq!(stats.message_count, 5);
    }
}

// ==================== 协调测试 (更多边界情况) ====================

#[cfg(feature = "coordination")]
mod coordination_advanced_tests {
    use super::*;
    use ai_cli_session_db::{Role, WriterType};

    #[test]
    fn test_heartbeat_update() {
        let (mut db, _tmp) = setup_db();

        let role = db.register_writer(WriterType::MemexDaemon).unwrap();
        assert_eq!(role, Role::Writer);

        // 心跳应该成功
        db.heartbeat().unwrap();
        db.heartbeat().unwrap();
    }

    #[test]
    fn test_release_and_reregister() {
        let (mut db, _tmp) = setup_db();

        db.register_writer(WriterType::MemexDaemon).unwrap();
        db.release_writer().unwrap();

        // 释放后可以重新注册
        let role = db.register_writer(WriterType::VlaudeDaemon).unwrap();
        assert_eq!(role, Role::Writer);
    }

    #[test]
    fn test_watch_role_change() {
        let (mut db, _tmp) = setup_db();

        db.register_writer(WriterType::MemexDaemon).unwrap();

        let rx = db.watch_role_change().unwrap();
        assert_eq!(*rx.borrow(), Role::Writer);
    }

    #[test]
    fn test_two_dbs_same_file_coordination() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("shared.db");

        // 第一个连接成为 Writer
        let config1 = DbConfig::local(&db_path);
        let mut db1 = SessionDB::connect(config1).unwrap();
        let role1 = db1.register_writer(WriterType::MemexDaemon).unwrap();
        assert_eq!(role1, Role::Writer);

        // 第二个连接（同优先级）应该成为 Reader
        let config2 = DbConfig::local(&db_path);
        let mut db2 = SessionDB::connect(config2).unwrap();
        let role2 = db2.register_writer(WriterType::VlaudeDaemon).unwrap();
        assert_eq!(role2, Role::Reader);

        // db1 释放后，db2 可以接管
        db1.release_writer().unwrap();

        // 检查健康状态
        let health = db2.check_writer_health().unwrap();
        assert_eq!(health, ai_cli_session_db::WriterHealth::Released);
    }

    #[test]
    fn test_higher_priority_preempts() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("shared.db");

        // daemon 先注册
        let config1 = DbConfig::local(&db_path);
        let mut db1 = SessionDB::connect(config1).unwrap();
        let role1 = db1.register_writer(WriterType::MemexDaemon).unwrap();
        assert_eq!(role1, Role::Writer);

        // kit（高优先级）可以抢占
        let config2 = DbConfig::local(&db_path);
        let mut db2 = SessionDB::connect(config2).unwrap();
        let role2 = db2.register_writer(WriterType::MemexKit).unwrap();
        assert_eq!(role2, Role::Writer);

        // db1 的心跳应该失败（被抢占了）
        let heartbeat_result = db1.heartbeat();
        assert!(heartbeat_result.is_err());
    }

    #[test]
    fn test_reader_write_permission_denied() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("shared.db");

        // 第一个连接成为 Writer
        let config1 = DbConfig::local(&db_path);
        let mut db1 = SessionDB::connect(config1).unwrap();
        let role1 = db1.register_writer(WriterType::MemexDaemon).unwrap();
        assert_eq!(role1, Role::Writer);

        // 第二个连接（同优先级）应该成为 Reader
        let config2 = DbConfig::local(&db_path);
        let mut db2 = SessionDB::connect(config2).unwrap();
        let role2 = db2.register_writer(WriterType::VlaudeDaemon).unwrap();
        assert_eq!(role2, Role::Reader);

        // Reader 尝试写入应该返回 PermissionDenied 错误
        let write_result = db2.get_or_create_project("test", "/path", "claude");
        assert!(write_result.is_err());

        // 验证错误类型
        let err = write_result.unwrap_err();
        assert!(
            matches!(err, ai_cli_session_db::Error::PermissionDenied),
            "Expected PermissionDenied error, got {:?}",
            err
        );
    }

    #[test]
    fn test_vlaude_kit_highest_priority() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("shared.db");

        // MemexKit (优先级 2) 先注册
        let config1 = DbConfig::local(&db_path);
        let mut db1 = SessionDB::connect(config1).unwrap();
        let role1 = db1.register_writer(WriterType::MemexKit).unwrap();
        assert_eq!(role1, Role::Writer);

        // VlaudeKit (优先级 3，最高) 可以抢占
        let config2 = DbConfig::local(&db_path);
        let mut db2 = SessionDB::connect(config2).unwrap();
        let role2 = db2.register_writer(WriterType::VlaudeKit).unwrap();
        assert_eq!(role2, Role::Writer);

        // db1 的心跳应该失败（被抢占了）
        let heartbeat_result = db1.heartbeat();
        assert!(heartbeat_result.is_err());
    }

    #[test]
    fn test_writer_can_write() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("shared.db");

        // 注册为 Writer
        let config = DbConfig::local(&db_path);
        let mut db = SessionDB::connect(config).unwrap();
        let role = db.register_writer(WriterType::MemexDaemon).unwrap();
        assert_eq!(role, Role::Writer);

        // Writer 可以写入
        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        assert!(project_id > 0);

        db.upsert_session("session-001", project_id).unwrap();

        let messages = vec![MessageInput {
            uuid: "uuid-001".to_string(),
            r#type: MessageType::User,
            content_text: "Hello".to_string(),
            content_full: "Hello".to_string(),
            timestamp: 1000,
            sequence: 1,
            source: None,
            channel: None,
            model: None,
            tool_call_id: None,
            tool_name: None,
            tool_args: None,
            raw: None,
            approval_status: None,
            approval_resolved_at: None,
        }];

        let inserted = db.insert_messages("session-001", &messages).unwrap();
        assert_eq!(inserted, 1);
    }

    /// 测试降级场景：Writer 被抢占后变成 Reader，写入应该被拒绝
    #[test]
    fn test_demotion_write_permission_denied() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("shared.db");

        // db1 作为 MemexDaemon (优先级 1) 先注册成为 Writer
        let config1 = DbConfig::local(&db_path);
        let mut db1 = SessionDB::connect(config1).unwrap();
        let role1 = db1.register_writer(WriterType::MemexDaemon).unwrap();
        assert_eq!(role1, Role::Writer);

        // db1 作为 Writer 可以写入
        let project_id = db1
            .get_or_create_project("test", "/path", "claude")
            .unwrap();
        assert!(project_id > 0);

        // db2 作为 VlaudeKit (优先级 3，最高) 抢占
        let config2 = DbConfig::local(&db_path);
        let mut db2 = SessionDB::connect(config2).unwrap();
        let role2 = db2.register_writer(WriterType::VlaudeKit).unwrap();
        assert_eq!(role2, Role::Writer);

        // db1 被降级，心跳失败
        let heartbeat_result = db1.heartbeat();
        assert!(heartbeat_result.is_err());

        // 关键测试：db1 被降级后，写入应该返回 PermissionDenied
        let write_result = db1.upsert_session("session-001", project_id);
        assert!(write_result.is_err());
        let err = write_result.unwrap_err();
        assert!(
            matches!(err, ai_cli_session_db::Error::PermissionDenied),
            "Expected PermissionDenied after demotion, got {:?}",
            err
        );

        // db2 作为新 Writer 可以写入
        db2.upsert_session("session-002", project_id).unwrap();
    }

    /// 测试升级场景：Reader 接管成为 Writer 后可以写入
    #[test]
    fn test_promotion_can_write() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("shared.db");

        // db1 先注册成为 Writer
        let config1 = DbConfig::local(&db_path);
        let mut db1 = SessionDB::connect(config1).unwrap();
        let role1 = db1.register_writer(WriterType::MemexDaemon).unwrap();
        assert_eq!(role1, Role::Writer);

        // 创建项目供后续使用
        let project_id = db1
            .get_or_create_project("test", "/path", "claude")
            .unwrap();

        // db2 作为同优先级注册，成为 Reader
        let config2 = DbConfig::local(&db_path);
        let mut db2 = SessionDB::connect(config2).unwrap();
        let role2 = db2.register_writer(WriterType::VlaudeDaemon).unwrap();
        assert_eq!(role2, Role::Reader);

        // db2 作为 Reader 不能写入
        let write_result = db2.upsert_session("session-001", project_id);
        assert!(write_result.is_err());

        // db1 释放 Writer 角色
        db1.release_writer().unwrap();

        // db2 检查 Writer 健康状态
        let health = db2.check_writer_health().unwrap();
        assert_eq!(health, ai_cli_session_db::WriterHealth::Released);

        // db2 尝试接管
        let taken = db2.try_takeover().unwrap();
        assert!(taken, "Takeover should succeed");

        // 关键测试：db2 升级为 Writer 后可以写入
        db2.upsert_session("session-001", project_id).unwrap();

        let messages = vec![MessageInput {
            uuid: "uuid-promoted".to_string(),
            r#type: MessageType::User,
            content_text: "After promotion".to_string(),
            content_full: "After promotion".to_string(),
            timestamp: 2000,
            sequence: 1,
            source: None,
            channel: None,
            model: None,
            tool_call_id: None,
            tool_name: None,
            tool_args: None,
            raw: None,
            approval_status: None,
            approval_resolved_at: None,
        }];

        let inserted = db2.insert_messages("session-001", &messages).unwrap();
        assert_eq!(inserted, 1);
    }
}

// ==================== 边界情况测试 ====================

mod edge_case_tests {
    use super::*;

    #[test]
    fn test_empty_content_message() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        let messages = vec![MessageInput {
            uuid: "uuid-empty".to_string(),
            r#type: MessageType::User,
            content_text: "".to_string(), // 空内容
            content_full: "".to_string(),
            timestamp: 1000,
            sequence: 0,
            source: None,
            channel: None,
            model: None,
            tool_call_id: None,
            tool_name: None,
            tool_args: None,
            raw: None,
            approval_status: None,
            approval_resolved_at: None,
        }];

        let inserted = db.insert_messages("session-001", &messages).unwrap();
        assert_eq!(inserted, 1);

        let loaded = db.list_messages("session-001", 10, 0).unwrap();
        assert_eq!(loaded[0].content_text, "");
    }

    #[test]
    fn test_unicode_content() {
        let (db, _tmp) = setup_db();

        let project_id = db
            .get_or_create_project("测试项目", "/路径/中文", "claude")
            .unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        let messages = vec![MessageInput {
            uuid: "uuid-unicode".to_string(),
            r#type: MessageType::User,
            content_text: "你好世界 🌍 مرحبا العالم 🎉 こんにちは".to_string(),
            content_full: "你好世界 🌍 مرحبا العالم 🎉 こんにちは".to_string(),
            timestamp: 1000,
            sequence: 0,
            source: None,
            channel: None,
            model: None,
            tool_call_id: None,
            tool_name: None,
            tool_args: None,
            raw: None,
            approval_status: None,
            approval_resolved_at: None,
        }];

        db.insert_messages("session-001", &messages).unwrap();

        let loaded = db.list_messages("session-001", 10, 0).unwrap();
        assert_eq!(
            loaded[0].content_text,
            "你好世界 🌍 مرحبا العالم 🎉 こんにちは"
        );

        // 项目名也应该正确
        let projects = db.list_projects().unwrap();
        assert_eq!(projects[0].name, "测试项目");
    }

    #[test]
    fn test_very_long_content() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        // 创建 100KB 的内容
        let long_content = "x".repeat(100 * 1024);

        let messages = vec![MessageInput {
            uuid: "uuid-long".to_string(),
            r#type: MessageType::Assistant,
            content_text: long_content.clone(),
            content_full: long_content.clone(),
            timestamp: 1000,
            sequence: 0,
            source: None,
            channel: None,
            model: None,
            tool_call_id: None,
            tool_name: None,
            tool_args: None,
            raw: None,
            approval_status: None,
            approval_resolved_at: None,
        }];

        db.insert_messages("session-001", &messages).unwrap();

        let loaded = db.list_messages("session-001", 10, 0).unwrap();
        assert_eq!(loaded[0].content_text.len(), 100 * 1024);
    }

    #[test]
    fn test_special_characters_in_path() {
        let (db, _tmp) = setup_db();

        // 路径包含特殊字符
        let weird_path = "/path/with spaces/and-dashes/and_underscores/and.dots";
        let project_id = db
            .get_or_create_project("test", weird_path, "claude")
            .unwrap();

        let projects = db.list_projects().unwrap();
        assert_eq!(projects[0].path, weird_path);

        // 再次获取应该返回同一个 ID
        let id2 = db
            .get_or_create_project("test", weird_path, "claude")
            .unwrap();
        assert_eq!(project_id, id2);
    }

    #[test]
    fn test_session_without_messages() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("empty-session", project_id).unwrap();

        let messages = db.list_messages("empty-session", 100, 0).unwrap();
        assert!(messages.is_empty());

        let sessions = db.list_sessions(project_id).unwrap();
        assert_eq!(sessions[0].message_count, 0);
    }

    #[test]
    fn test_nonexistent_session_messages() {
        let (db, _tmp) = setup_db();

        // 查询不存在的 session
        let messages = db.list_messages("nonexistent", 100, 0).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn test_large_batch_insert() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        // 插入 1000 条消息
        let messages: Vec<MessageInput> = (0..1000)
            .map(|i| MessageInput {
                uuid: format!("uuid-{}", i),
                r#type: if i % 2 == 0 {
                    MessageType::User
                } else {
                    MessageType::Assistant
                },
                content_text: format!("Message {}", i),
                content_full: format!("Message {}", i),
                timestamp: i as i64,
                sequence: i as i64,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            })
            .collect();

        let inserted = db.insert_messages("session-001", &messages).unwrap();
        assert_eq!(inserted, 1000);

        let stats = db.get_stats().unwrap();
        assert_eq!(stats.message_count, 1000);
    }

    #[test]
    fn test_timestamp_ordering() {
        let (db, _tmp) = setup_db();

        let project_id = db.get_or_create_project("test", "/path", "claude").unwrap();
        db.upsert_session("session-001", project_id).unwrap();

        // 乱序插入
        let messages = vec![
            MessageInput {
                uuid: "uuid-3".to_string(),
                r#type: MessageType::User,
                content_text: "Third".to_string(),
                content_full: "Third".to_string(),
                timestamp: 3000,
                sequence: 2,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
            MessageInput {
                uuid: "uuid-1".to_string(),
                r#type: MessageType::User,
                content_text: "First".to_string(),
                content_full: "First".to_string(),
                timestamp: 1000,
                sequence: 0,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
            MessageInput {
                uuid: "uuid-2".to_string(),
                r#type: MessageType::User,
                content_text: "Second".to_string(),
                content_full: "Second".to_string(),
                timestamp: 2000,
                sequence: 1,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            },
        ];

        db.insert_messages("session-001", &messages).unwrap();

        // 按 sequence 排序返回
        let loaded = db.list_messages("session-001", 10, 0).unwrap();
        assert_eq!(loaded[0].content_text, "First");
        assert_eq!(loaded[1].content_text, "Second");
        assert_eq!(loaded[2].content_text, "Third");
    }
}

// ==================== Writer 转换函数测试 ====================

#[cfg(feature = "writer")]
mod writer_conversion_tests {
    use ai_cli_session_collector::{MessageType, ParsedContent, ParsedMessage, Source};
    use ai_cli_session_db::writer::{convert_message, convert_messages};

    fn create_parsed_message(uuid: &str, msg_type: MessageType, content: &str) -> ParsedMessage {
        ParsedMessage {
            uuid: uuid.to_string(),
            session_id: "session-001".to_string(),
            message_type: msg_type,
            content: ParsedContent {
                text: content.to_string(),
                full: content.to_string(),
            },
            timestamp: Some("2024-01-15T10:30:00Z".to_string()),
            source: Source::Claude,
            channel: Some("code".to_string()),
            model: None,
            tool_call_id: None,
            tool_name: None,
            tool_args: None,
            raw: None,
            cwd: None,
            stop_reason: None,
        }
    }

    #[test]
    fn test_convert_user_message() {
        let parsed = create_parsed_message("uuid-1", MessageType::User, "Hello");
        let input = convert_message(&parsed, 0);

        assert_eq!(input.uuid, "uuid-1");
        assert_eq!(input.r#type, MessageType::User);
        assert_eq!(input.content_text, "Hello");
        assert_eq!(input.sequence, 0);
        // timestamp 应该被解析为毫秒
        assert!(input.timestamp > 0);
    }

    #[test]
    fn test_convert_assistant_message() {
        let parsed = create_parsed_message("uuid-2", MessageType::Assistant, "Hi there");
        let input = convert_message(&parsed, 1);

        assert_eq!(input.r#type, MessageType::Assistant);
    }

    #[test]
    fn test_convert_tool_message() {
        let parsed = create_parsed_message("uuid-3", MessageType::Tool, "Tool output");
        let input = convert_message(&parsed, 2);

        // Tool 消息现在保留为 Tool 类型
        assert_eq!(input.r#type, MessageType::Tool);
    }

    #[test]
    fn test_convert_messages_batch() {
        let messages = vec![
            create_parsed_message("uuid-1", MessageType::User, "Q1"),
            create_parsed_message("uuid-2", MessageType::Assistant, "A1"),
            create_parsed_message("uuid-3", MessageType::User, "Q2"),
        ];

        let inputs = convert_messages(&messages);

        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].sequence, 0);
        assert_eq!(inputs[1].sequence, 1);
        assert_eq!(inputs[2].sequence, 2);
    }

    #[test]
    fn test_convert_message_without_timestamp() {
        let mut parsed = create_parsed_message("uuid-1", MessageType::User, "Hello");
        parsed.timestamp = None;

        let input = convert_message(&parsed, 0);
        assert_eq!(input.timestamp, 0); // 默认为 0
    }

    #[test]
    fn test_convert_message_invalid_timestamp() {
        let mut parsed = create_parsed_message("uuid-1", MessageType::User, "Hello");
        parsed.timestamp = Some("invalid-date".to_string());

        let input = convert_message(&parsed, 0);
        assert_eq!(input.timestamp, 0); // 解析失败默认为 0
    }
}

// ==================== Agent + Client 集成测试 ====================

#[cfg(all(feature = "agent", feature = "client"))]
mod agent_client_tests {
    use ai_cli_session_db::agent::{Agent, AgentConfig};
    use ai_cli_session_db::protocol::{EventType, QueryType, Request, Response};
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::time::sleep;

    /// 创建测试配置
    fn test_agent_config() -> (AgentConfig, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let config = AgentConfig {
            data_dir: temp_dir.path().to_path_buf(),
            idle_timeout_secs: 60,
        };
        (config, temp_dir)
    }

    #[tokio::test]
    async fn test_agent_client_full_handshake() {
        let (agent_config, _tmp) = test_agent_config();
        let socket_path = agent_config.socket_path();

        // 启动 Agent
        let agent = Arc::new(Agent::new(agent_config).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                let _ = agent.run().await;
            })
        };

        sleep(Duration::from_millis(500)).await;

        // 连接并握手
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let handshake = Request::Handshake {
            component: "integration-test".to_string(),
            version: "1.0.0".to_string(),
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&handshake).unwrap()).as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();

        match response {
            Response::HandshakeOk { agent_version } => {
                assert!(!agent_version.is_empty());
            }
            _ => panic!("Expected HandshakeOk"),
        }

        agent_handle.abort();
    }

    #[tokio::test]
    async fn test_agent_client_subscribe_and_heartbeat() {
        let (agent_config, _tmp) = test_agent_config();
        let socket_path = agent_config.socket_path();

        let agent = Arc::new(Agent::new(agent_config).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                let _ = agent.run().await;
            })
        };

        sleep(Duration::from_millis(500)).await;

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

        // 订阅
        line.clear();
        let subscribe = Request::Subscribe {
            events: vec![EventType::NewMessage, EventType::SessionStart],
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
            .await
            .unwrap();

        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert!(matches!(response, Response::Ok));

        // 心跳
        line.clear();
        let heartbeat = Request::Heartbeat;
        writer
            .write_all(format!("{}\n", serde_json::to_string(&heartbeat).unwrap()).as_bytes())
            .await
            .unwrap();

        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert!(matches!(response, Response::Ok));

        agent_handle.abort();
    }

    #[tokio::test]
    async fn test_agent_client_query_status() {
        let (agent_config, _tmp) = test_agent_config();
        let socket_path = agent_config.socket_path();

        let agent = Arc::new(Agent::new(agent_config).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                let _ = agent.run().await;
            })
        };

        sleep(Duration::from_millis(500)).await;

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

        // 查询状态
        line.clear();
        let query = Request::Query {
            query_type: QueryType::Status,
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&query).unwrap()).as_bytes())
            .await
            .unwrap();

        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();

        match response {
            Response::QueryResult { data } => {
                assert!(data.get("agent_version").is_some());
                assert!(data.get("connections").is_some());
            }
            _ => panic!("Expected QueryResult"),
        }

        agent_handle.abort();
    }

    #[tokio::test]
    async fn test_multiple_clients_concurrent() {
        let (agent_config, _tmp) = test_agent_config();
        let socket_path = agent_config.socket_path();

        let agent = Arc::new(Agent::new(agent_config).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                let _ = agent.run().await;
            })
        };

        sleep(Duration::from_millis(500)).await;

        // 创建多个并发客户端
        let mut handles = vec![];

        for i in 0..3 {
            let socket_path = socket_path.clone();
            let handle = tokio::spawn(async move {
                let stream = UnixStream::connect(&socket_path).await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);

                // 握手
                let handshake = Request::Handshake {
                    component: format!("client-{}", i),
                    version: "1.0.0".to_string(),
                };
                writer
                    .write_all(format!("{}\n", serde_json::to_string(&handshake).unwrap()).as_bytes())
                    .await
                    .unwrap();

                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();

                let response: Response = serde_json::from_str(&line).unwrap();
                assert!(matches!(response, Response::HandshakeOk { .. }));

                sleep(Duration::from_millis(100)).await;
                i
            });
            handles.push(handle);
        }

        // 等待所有客户端
        for handle in handles {
            let client_id = handle.await.unwrap();
            assert!(client_id < 3);
        }

        agent_handle.abort();
    }

    #[tokio::test]
    async fn test_unsubscribe() {
        let (agent_config, _tmp) = test_agent_config();
        let socket_path = agent_config.socket_path();

        let agent = Arc::new(Agent::new(agent_config).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                let _ = agent.run().await;
            })
        };

        sleep(Duration::from_millis(500)).await;

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

        // 订阅
        line.clear();
        let subscribe = Request::Subscribe {
            events: vec![EventType::NewMessage],
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
            .await
            .unwrap();

        reader.read_line(&mut line).await.unwrap();
        assert!(matches!(serde_json::from_str::<Response>(&line).unwrap(), Response::Ok));

        // 取消订阅
        line.clear();
        let unsubscribe = Request::Unsubscribe {
            events: vec![EventType::NewMessage],
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&unsubscribe).unwrap()).as_bytes())
            .await
            .unwrap();

        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert!(matches!(response, Response::Ok));

        agent_handle.abort();
    }

    #[tokio::test]
    async fn test_connection_count_query() {
        let (agent_config, _tmp) = test_agent_config();
        let socket_path = agent_config.socket_path();

        let agent = Arc::new(Agent::new(agent_config).unwrap());
        let agent_handle = {
            let agent = agent.clone();
            tokio::spawn(async move {
                let _ = agent.run().await;
            })
        };

        sleep(Duration::from_millis(500)).await;

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

        // 查询连接数
        line.clear();
        let query = Request::Query {
            query_type: QueryType::ConnectionCount,
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&query).unwrap()).as_bytes())
            .await
            .unwrap();

        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();

        match response {
            Response::QueryResult { data } => {
                let count = data.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
                assert!(count >= 1); // 至少有当前连接
            }
            _ => panic!("Expected QueryResult"),
        }

        agent_handle.abort();
    }
}
