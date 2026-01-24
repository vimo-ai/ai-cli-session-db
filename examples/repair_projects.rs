//! 修复 projects 表中错误的 path 数据
//!
//! 问题：部分 project 的 path 存储了编码形式（如 -Users-xxx）而非真实路径
//! 修复：通过 encoded_dir_name 找到 JSONL 文件，提取 cwd 作为真实路径
//!
//! 特殊情况：如果正确路径已存在（重复记录），合并 sessions 后删除错误记录

use ai_cli_session_collector::ClaudeAdapter;
use ai_cli_session_collector::ConversationAdapter;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    let home = std::env::var("HOME").expect("HOME not set");
    let db_path = PathBuf::from(&home).join(".vimo/db/ai-cli-session.db");
    let projects_path = PathBuf::from(&home).join(".claude/projects");

    println!("=== 修复 projects 表数据 ===\n");
    println!("数据库: {}", db_path.display());
    println!("Projects 目录: {}\n", projects_path.display());

    // 连接数据库
    let conn = Connection::open(&db_path).expect("无法打开数据库");

    // 1. 查找所有异常数据（path 以 - 开头）
    let mut stmt = conn
        .prepare("SELECT id, path, encoded_dir_name FROM projects WHERE path LIKE '-%'")
        .expect("SQL 错误");

    let bad_projects: Vec<(i64, String, Option<String>)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .expect("查询失败")
        .filter_map(|r| r.ok())
        .collect();

    println!("找到 {} 条异常数据\n", bad_projects.len());

    if bad_projects.is_empty() {
        println!("✅ 无需修复");
        return;
    }

    // 2. 使用 ClaudeAdapter 获取所有会话，建立 encoded_dir -> real_path 映射
    println!("正在扫描 JSONL 文件获取真实路径...\n");
    let adapter = ClaudeAdapter::with_path(projects_path.clone());
    let sessions = adapter.list_sessions().expect("无法列出会话");

    // 建立 encoded_dir_name -> project_path 映射
    let mut path_map: HashMap<String, String> = HashMap::new();
    for session in &sessions {
        if let Some(encoded) = &session.encoded_dir_name {
            if !session.project_path.is_empty() && !session.project_path.starts_with('-') {
                path_map.insert(encoded.clone(), session.project_path.clone());
            }
        }
    }

    println!("建立了 {} 条路径映射\n", path_map.len());

    // 3. 修复数据
    let mut fixed = 0;
    let mut merged = 0;
    let mut not_found = 0;

    for (bad_id, bad_path, encoded_opt) in &bad_projects {
        // encoded_dir_name 可能为 None，此时用 bad_path 作为 encoded_dir_name
        let encoded = encoded_opt.as_ref().unwrap_or(bad_path);

        if let Some(real_path) = path_map.get(encoded) {
            // 检查正确路径是否已存在
            let existing_id: Option<i64> = conn
                .query_row(
                    "SELECT id FROM projects WHERE path = ?1 AND id != ?2",
                    rusqlite::params![real_path, bad_id],
                    |row| row.get(0),
                )
                .ok();

            if let Some(good_id) = existing_id {
                // 存在重复记录，需要合并
                println!("合并 ID {} -> ID {}: {}", bad_id, good_id, real_path);

                // 将 sessions 从错误项目迁移到正确项目
                conn.execute(
                    "UPDATE sessions SET project_id = ?1 WHERE project_id = ?2",
                    rusqlite::params![good_id, bad_id],
                )
                .expect("迁移 sessions 失败");

                // 删除错误的 project 记录
                conn.execute(
                    "DELETE FROM projects WHERE id = ?1",
                    rusqlite::params![bad_id],
                )
                .expect("删除失败");

                merged += 1;
            } else {
                // 没有重复，直接更新 path
                println!("修复 ID {}: {} -> {}", bad_id, bad_path, real_path);

                conn.execute(
                    "UPDATE projects SET path = ?1 WHERE id = ?2",
                    rusqlite::params![real_path, bad_id],
                )
                .expect("更新失败");

                fixed += 1;
            }
        } else {
            println!("⚠️ ID {} 未找到映射: {}", bad_id, encoded);
            not_found += 1;
        }
    }

    println!("\n=== 修复完成 ===");
    println!("直接修复: {}", fixed);
    println!("合并删除: {}", merged);
    println!("未找到:   {}", not_found);

    // 4. 验证
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM projects WHERE path LIKE '-%'",
            [],
            |row| row.get(0),
        )
        .expect("查询失败");

    println!("剩余异常: {}", remaining);

    if remaining == 0 {
        println!("\n✅ 所有数据已修复！");
    } else {
        println!(
            "\n⚠️ 仍有 {} 条数据无法自动修复（可能对应的 JSONL 已删除）",
            remaining
        );
    }
}
