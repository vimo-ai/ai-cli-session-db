//! 测试 SessionReader 功能

use claude_session_db::reader::{compute_session_path, SessionReader};
use std::path::PathBuf;

fn main() {
    let projects_path = PathBuf::from(std::env::var("HOME").unwrap()).join(".claude/projects");

    println!("=== 测试 SessionReader ===\n");

    let reader = SessionReader::new(projects_path.clone());

    // 测试 get_session_path
    println!("1. 测试 get_session_path\n");

    // 找一个已知存在的 session ID
    let test_session_id = "c6d788e3-870c-4124-b5b2-77ebf44f66fb"; // 当前会话

    match reader.get_session_path(test_session_id) {
        Some(path) => {
            println!("✅ 找到会话路径:");
            println!("   session_id: {}", test_session_id);
            println!("   path: {}", path);

            // 验证文件确实存在
            if std::path::Path::new(&path).exists() {
                println!("   文件存在: ✓");
            } else {
                println!("   文件存在: ✗");
            }
        }
        None => {
            println!("❌ 未找到会话: {}", test_session_id);
        }
    }

    // 测试 compute_session_path
    println!("\n2. 测试 compute_session_path\n");

    let encoded_dir = "-Users-higuaifan-Desktop-vimo-ETerm";
    let session_id = "c6d788e3-870c-4124-b5b2-77ebf44f66fb";

    let computed = compute_session_path(&projects_path, encoded_dir, session_id);
    println!("   encoded_dir: {}", encoded_dir);
    println!("   session_id:  {}", session_id);
    println!("   computed:    {}", computed.display());

    if computed.exists() {
        println!("   文件存在: ✓");
    } else {
        println!("   文件存在: ✗");
    }

    // 测试列出项目
    println!("\n3. 测试 list_projects\n");

    let mut reader = SessionReader::new(projects_path.clone());
    let projects = reader.list_projects(Some(5));

    println!("前 5 个项目:");
    for project in projects {
        println!("  - {}", project.name);
        println!("    path: {}", project.path);
        println!("    encoded: {}", project.encoded_name);
        println!("    sessions: {}", project.session_count);
    }
}
