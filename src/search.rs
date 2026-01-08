//! 搜索功能

use crate::db::SessionDB;
use crate::error::Result;
use crate::types::SearchResult;
#[allow(unused_imports)]
use rusqlite::params;

/// 转义 FTS5 查询中的特殊字符
///
/// FTS5 特殊字符包括：
/// - `-` (NOT 操作符)
/// - `.` (列指定符)
/// - `*` (前缀匹配)
/// - `"` (短语分隔符)
/// - `(`, `)` (分组)
/// - `^` (权重提升)
/// - `+` (必需)
/// - `:` (列指定符)
///
/// 对每个词单独用双引号包裹，用 OR 连接，实现"匹配任一关键词"的搜索
fn escape_fts5_query(query: &str) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|word| {
            // 内部双引号需要转义（两个双引号表示一个字面双引号）
            let escaped = word.replace('"', "\"\"");
            format!("\"{}\"", escaped)
        })
        .collect();

    if terms.is_empty() {
        return String::new();
    }

    if terms.len() == 1 {
        return terms.into_iter().next().unwrap();
    }

    // 多个词用 OR 连接
    terms.join(" OR ")
}

impl SessionDB {
    /// FTS5 全文搜索
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        self.search_fts_with_project(query, limit, None)
    }

    /// FTS5 全文搜索 (可指定项目)
    pub fn search_fts_with_project(
        &self,
        query: &str,
        limit: usize,
        project_id: Option<i64>,
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock();

        // 转义查询，防止 FTS5 语法错误
        let escaped_query = escape_fts5_query(query);

        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(pid) = project_id
        {
            (
                r#"
                SELECT
                    m.id,
                    m.session_id,
                    s.project_id,
                    p.name as project_name,
                    m.type,
                    m.content_full,
                    snippet(messages_fts, 0, '<mark>', '</mark>', '...', 64) as snippet,
                    bm25(messages_fts) as score,
                    m.timestamp
                FROM messages_fts
                JOIN messages m ON messages_fts.rowid = m.id
                JOIN sessions s ON m.session_id = s.session_id
                JOIN projects p ON s.project_id = p.id
                WHERE messages_fts MATCH ?1
                  AND s.project_id = ?2
                ORDER BY score
                LIMIT ?3
                "#,
                vec![
                    Box::new(escaped_query.clone()) as Box<dyn rusqlite::ToSql>,
                    Box::new(pid),
                    Box::new(limit as i64),
                ],
            )
        } else {
            (
                r#"
                SELECT
                    m.id,
                    m.session_id,
                    s.project_id,
                    p.name as project_name,
                    m.type,
                    m.content_full,
                    snippet(messages_fts, 0, '<mark>', '</mark>', '...', 64) as snippet,
                    bm25(messages_fts) as score,
                    m.timestamp
                FROM messages_fts
                JOIN messages m ON messages_fts.rowid = m.id
                JOIN sessions s ON m.session_id = s.session_id
                JOIN projects p ON s.project_id = p.id
                WHERE messages_fts MATCH ?1
                ORDER BY score
                LIMIT ?2
                "#,
                vec![
                    Box::new(escaped_query) as Box<dyn rusqlite::ToSql>,
                    Box::new(limit as i64),
                ],
            )
        };

        let mut stmt = conn.prepare(sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(SearchResult {
                message_id: row.get(0)?,
                session_id: row.get(1)?,
                project_id: row.get(2)?,
                project_name: row.get(3)?,
                r#type: row.get(4)?,
                content_full: row.get(5)?,
                snippet: row.get(6)?,
                score: row.get(7)?,
                timestamp: row.get(8)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_fts5_query_single_word() {
        // 单个词：直接用双引号包裹
        assert_eq!(escape_fts5_query("hello"), "\"hello\"");
        assert_eq!(escape_fts5_query("ETerm.app"), "\"ETerm.app\"");
        assert_eq!(escape_fts5_query("test-case.rs:123"), "\"test-case.rs:123\"");
    }

    #[test]
    fn test_escape_fts5_query_multiple_words() {
        // 多个词：每个词单独包裹，用 OR 连接
        assert_eq!(
            escape_fts5_query("memvid 单文件"),
            "\"memvid\" OR \"单文件\""
        );
        assert_eq!(
            escape_fts5_query("ETerm 启动 环境变量"),
            "\"ETerm\" OR \"启动\" OR \"环境变量\""
        );
        assert_eq!(
            escape_fts5_query("open --env"),
            "\"open\" OR \"--env\""
        );
    }

    #[test]
    fn test_escape_fts5_query_with_quotes() {
        // 包含双引号的词：内部双引号需要转义
        assert_eq!(escape_fts5_query("say\"hi\""), "\"say\"\"hi\"\"\"");
        assert_eq!(
            escape_fts5_query("hello \"world\""),
            "\"hello\" OR \"\"\"world\"\"\""
        );
    }

    #[test]
    fn test_escape_fts5_query_empty() {
        // 空查询
        assert_eq!(escape_fts5_query(""), "");
        assert_eq!(escape_fts5_query("   "), "");
    }

    #[test]
    fn test_escape_fts5_query_whitespace_handling() {
        // 多余空格应该被忽略
        assert_eq!(
            escape_fts5_query("  hello   world  "),
            "\"hello\" OR \"world\""
        );
    }
}
