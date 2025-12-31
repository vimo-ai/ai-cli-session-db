//! 搜索功能

use crate::db::SessionDB;
use crate::error::Result;
use crate::types::SearchResult;
#[allow(unused_imports)]
use rusqlite::params;

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
                    Box::new(query.to_string()) as Box<dyn rusqlite::ToSql>,
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
                    Box::new(query.to_string()) as Box<dyn rusqlite::ToSql>,
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
