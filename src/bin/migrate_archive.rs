//! Archive JSONL 迁移工具
//!
//! 从 Archive 目录读取 JSONL 文件，使用新的内容分离逻辑导入到数据库

use ai_cli_session_collector::{ClaudeAdapter, IndexableSession};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        eprintln!("用法: {} <数据库路径> <JSONL目录或文件>...", args[0]);
        eprintln!("例: {} ~/.vimo/db/ai-cli-session-v2.db ~/memex-data/archive/2025/12/", args[0]);
        std::process::exit(1);
    }

    let db_path = &args[1];
    let sources: Vec<&str> = args[2..].iter().map(|s| s.as_str()).collect();

    println!("目标数据库: {}", db_path);

    let mut conn = Connection::open(db_path)?;

    // 获取已有的 uuid 集合（用于跳过已存在的消息）
    let existing_uuids = get_existing_uuids(&conn)?;
    println!("已有消息数: {}", existing_uuids.len());

    // 收集所有 JSONL 文件
    let mut jsonl_files = Vec::new();
    for source in &sources {
        let path = Path::new(source);
        if path.is_dir() {
            collect_jsonl_files(path, &mut jsonl_files)?;
        } else if path.extension().map_or(false, |e| e == "jsonl") {
            jsonl_files.push(path.to_path_buf());
        }
    }

    println!("找到 {} 个 JSONL 文件", jsonl_files.len());

    let mut total_sessions = 0;
    let mut total_messages = 0;
    let mut updated_messages = 0;

    // 开始事务
    let tx = conn.transaction()?;

    for jsonl_path in &jsonl_files {
        match process_jsonl(&tx, jsonl_path, &existing_uuids) {
            Ok((sessions, messages, updated)) => {
                total_sessions += sessions;
                total_messages += messages;
                updated_messages += updated;
            }
            Err(e) => {
                eprintln!("处理失败 {:?}: {}", jsonl_path, e);
            }
        }
    }

    tx.commit()?;

    println!("\n=== 迁移完成 ===");
    println!("处理会话: {}", total_sessions);
    println!("新增消息: {}", total_messages);
    println!("更新消息: {}", updated_messages);

    // 重建 FTS
    println!("\n重建 FTS 索引...");
    conn.execute("INSERT INTO messages_fts(messages_fts) VALUES('rebuild')", [])?;
    println!("完成");

    Ok(())
}

fn get_existing_uuids(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT uuid FROM messages")?;
    let uuids: Result<HashSet<String>, _> = stmt
        .query_map([], |row| row.get(0))?
        .collect();
    Ok(uuids?)
}

fn collect_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files)?;
        } else if path.extension().map_or(false, |e| e == "jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

fn process_jsonl(
    conn: &Connection,
    jsonl_path: &Path,
    existing_uuids: &HashSet<String>,
) -> Result<(usize, usize, usize)> {
    let path_str = jsonl_path.to_string_lossy();

    let session = match ClaudeAdapter::parse_session_from_path(&path_str)? {
        Some(s) => s,
        None => return Ok((0, 0, 0)),
    };

    let mut new_messages = 0;
    let mut updated_messages = 0;

    for msg in &session.messages {
        if existing_uuids.contains(&msg.uuid) {
            // 尝试更新已存在的消息（用完整内容替换）
            let updated = conn.execute(
                "UPDATE messages SET
                    content_text = ?1,
                    content_full = ?2,
                    raw = ?3
                WHERE uuid = ?4 AND (content_text != ?1 OR content_full != ?2)",
                params![
                    &msg.content.text,
                    &msg.content.full,
                    &msg.raw,
                    &msg.uuid,
                ],
            )?;
            if updated > 0 {
                updated_messages += 1;
            }
        } else {
            // 确保 session 存在
            ensure_session(conn, &session)?;

            // 插入新消息
            conn.execute(
                "INSERT OR IGNORE INTO messages
                    (session_id, uuid, type, content_text, content_full, timestamp, sequence, source, channel, model, raw)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    &session.session_id,
                    &msg.uuid,
                    format!("{:?}", msg.msg_type).to_lowercase(),
                    &msg.content.text,
                    &msg.content.full,
                    msg.timestamp,
                    msg.sequence,
                    "claude",
                    &session.meta.channel,
                    &session.meta.model,
                    &msg.raw,
                ],
            )?;
            new_messages += 1;
        }
    }

    Ok((1, new_messages, updated_messages))
}

fn ensure_session(conn: &Connection, session: &IndexableSession) -> Result<()> {
    // 确保 project 存在
    let project_path = session.meta.cwd.as_deref().unwrap_or("unknown");
    let project_name = Path::new(project_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    conn.execute(
        "INSERT OR IGNORE INTO projects (path, name, source) VALUES (?1, ?2, 'claude')",
        params![project_path, project_name],
    )?;

    let project_id: i64 = conn.query_row(
        "SELECT id FROM projects WHERE path = ?1",
        params![project_path],
        |row| row.get(0),
    )?;

    // 确保 session 存在
    conn.execute(
        "INSERT OR IGNORE INTO sessions (session_id, project_id, message_count, cwd, model, channel)
         VALUES (?1, ?2, 0, ?3, ?4, ?5)",
        params![
            &session.session_id,
            project_id,
            session.meta.cwd,
            session.meta.model,
            session.meta.channel,
        ],
    )?;

    Ok(())
}
