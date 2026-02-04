//! 搜索功能
//!
//! 搜索策略：FTS5 优先，结果不足时 LIKE 补充

use crate::db::SessionDB;
use crate::error::Result;
use crate::types::{SearchOrderBy, SearchResult};
#[allow(unused_imports)]
use rusqlite::params;

/// 转义 LIKE 模式中的通配符（`%` 和 `_`），使用 `\` 作为转义字符
///
/// 配合 SQL 中的 `ESCAPE '\'` 使用
pub fn escape_like_pattern(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

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
pub fn escape_fts5_query(query: &str) -> String {
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
        self.search_fts_with_options(query, limit, None, SearchOrderBy::Score)
    }

    /// FTS5 全文搜索 (可指定项目)
    pub fn search_fts_with_project(
        &self,
        query: &str,
        limit: usize,
        project_id: Option<i64>,
    ) -> Result<Vec<SearchResult>> {
        self.search_fts_with_options(query, limit, project_id, SearchOrderBy::Score)
    }

    /// FTS5 全文搜索 (完整参数版本)
    ///
    /// # Arguments
    /// - `query`: 搜索关键词
    /// - `limit`: 返回数量
    /// - `project_id`: 项目 ID 过滤（可选）
    /// - `order_by`: 排序方式
    pub fn search_fts_with_options(
        &self,
        query: &str,
        limit: usize,
        project_id: Option<i64>,
        order_by: SearchOrderBy,
    ) -> Result<Vec<SearchResult>> {
        self.search_fts_full(query, limit, project_id, order_by, None, None)
    }

    /// FTS5 全文搜索 (完整参数版本，含日期范围)
    ///
    /// 搜索策略：FTS5 优先，结果不足且有 project_id 时 LIKE 补充
    ///
    /// # Arguments
    /// - `query`: 搜索关键词
    /// - `limit`: 返回数量
    /// - `project_id`: 项目 ID 过滤（可选）
    /// - `order_by`: 排序方式
    /// - `start_timestamp`: 开始时间戳（毫秒，可选）
    /// - `end_timestamp`: 结束时间戳（毫秒，可选）
    pub fn search_fts_full(
        &self,
        query: &str,
        limit: usize,
        project_id: Option<i64>,
        order_by: SearchOrderBy,
        start_timestamp: Option<i64>,
        end_timestamp: Option<i64>,
    ) -> Result<Vec<SearchResult>> {
        self.search_fts_full_with_sessions(
            query,
            limit,
            project_id,
            order_by,
            start_timestamp,
            end_timestamp,
            &[],
        )
    }

    /// FTS5 全文搜索 (完整参数版本，含日期范围和 session 过滤)
    ///
    /// # Arguments
    /// - `query`: 搜索关键词
    /// - `limit`: 返回数量
    /// - `project_id`: 项目 ID 过滤（可选）
    /// - `order_by`: 排序方式
    /// - `start_timestamp`: 开始时间戳（毫秒，可选）
    /// - `end_timestamp`: 结束时间戳（毫秒，可选）
    /// - `session_ids`: Session ID 前缀列表（空则不过滤）
    #[allow(clippy::too_many_arguments)]
    pub fn search_fts_full_with_sessions(
        &self,
        query: &str,
        limit: usize,
        project_id: Option<i64>,
        order_by: SearchOrderBy,
        start_timestamp: Option<i64>,
        end_timestamp: Option<i64>,
        session_ids: &[String],
    ) -> Result<Vec<SearchResult>> {
        // 先用 FTS5 搜索
        let fts_results = self.search_fts_internal(
            query,
            limit,
            project_id,
            order_by,
            start_timestamp,
            end_timestamp,
            session_ids,
        )?;

        // FTS 结果足够，直接返回
        if fts_results.len() >= limit {
            return Ok(fts_results);
        }

        // FTS 结果不足且有 project_id，用 LIKE 补充
        if project_id.is_some() && fts_results.len() < limit {
            let existing_ids: Vec<i64> = fts_results.iter().map(|r| r.message_id).collect();
            let remaining = limit - fts_results.len();

            let like_results = self.search_like_fallback(
                query,
                remaining,
                project_id,
                order_by,
                start_timestamp,
                end_timestamp,
                &existing_ids,
                session_ids,
            )?;

            let mut combined = fts_results;
            combined.extend(like_results);
            return Ok(combined);
        }

        Ok(fts_results)
    }

    /// FTS5 内部搜索实现
    #[allow(clippy::too_many_arguments)]
    fn search_fts_internal(
        &self,
        query: &str,
        limit: usize,
        project_id: Option<i64>,
        order_by: SearchOrderBy,
        start_timestamp: Option<i64>,
        end_timestamp: Option<i64>,
        session_ids: &[String],
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock();

        // 转义查询，防止 FTS5 语法错误
        let escaped_query = escape_fts5_query(query);

        // 根据排序方式生成 ORDER BY 子句
        let order_clause = match order_by {
            SearchOrderBy::Score => "ORDER BY score",
            SearchOrderBy::TimeDesc => "ORDER BY m.timestamp DESC",
            SearchOrderBy::TimeAsc => "ORDER BY m.timestamp ASC",
        };

        // 动态构建 WHERE 子句和参数
        let mut where_clauses = vec!["messages_fts MATCH ?1".to_string()];
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(escaped_query) as Box<dyn rusqlite::ToSql>];
        let mut param_idx = 2;

        if let Some(pid) = project_id {
            where_clauses.push(format!("s.project_id = ?{}", param_idx));
            params_vec.push(Box::new(pid));
            param_idx += 1;
        }

        if let Some(start_ts) = start_timestamp {
            where_clauses.push(format!("m.timestamp >= ?{}", param_idx));
            params_vec.push(Box::new(start_ts));
            param_idx += 1;
        }

        if let Some(end_ts) = end_timestamp {
            where_clauses.push(format!("m.timestamp <= ?{}", param_idx));
            params_vec.push(Box::new(end_ts));
            param_idx += 1;
        }

        // Session ID 前缀过滤
        if !session_ids.is_empty() {
            let session_likes: Vec<String> = session_ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("m.session_id LIKE ?{} ESCAPE '\\'", param_idx + i))
                .collect();
            where_clauses.push(format!("({})", session_likes.join(" OR ")));
            for sid in session_ids {
                params_vec.push(Box::new(format!("{}%", escape_like_pattern(sid))));
            }
            param_idx += session_ids.len();
        }

        // LIMIT 参数
        params_vec.push(Box::new(limit as i64));

        let sql = format!(
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
            WHERE {}
            {}
            LIMIT ?{}
            "#,
            where_clauses.join(" AND "),
            order_clause,
            param_idx
        );

        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

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

    /// LIKE 回退搜索（FTS 结果不足时使用）
    ///
    /// 仅在指定 project_id 时使用，因为项目内数据量有限，LIKE 性能可接受
    #[allow(clippy::too_many_arguments)]
    fn search_like_fallback(
        &self,
        query: &str,
        limit: usize,
        project_id: Option<i64>,
        order_by: SearchOrderBy,
        start_timestamp: Option<i64>,
        end_timestamp: Option<i64>,
        exclude_ids: &[i64],
        session_ids: &[String],
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock();

        // 排序子句（LIKE 没有相关性分数，默认按时间）
        let order_clause = match order_by {
            SearchOrderBy::Score => "ORDER BY m.timestamp DESC", // 无分数，退化为时间排序
            SearchOrderBy::TimeDesc => "ORDER BY m.timestamp DESC",
            SearchOrderBy::TimeAsc => "ORDER BY m.timestamp ASC",
        };

        // 构建 WHERE 子句
        let mut where_clauses = vec!["m.content_full LIKE ?1".to_string()];
        let like_pattern = format!("%{}%", query);
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(like_pattern) as Box<dyn rusqlite::ToSql>];
        let mut param_idx = 2;

        if let Some(pid) = project_id {
            where_clauses.push(format!("s.project_id = ?{}", param_idx));
            params_vec.push(Box::new(pid));
            param_idx += 1;
        }

        if let Some(start_ts) = start_timestamp {
            where_clauses.push(format!("m.timestamp >= ?{}", param_idx));
            params_vec.push(Box::new(start_ts));
            param_idx += 1;
        }

        if let Some(end_ts) = end_timestamp {
            where_clauses.push(format!("m.timestamp <= ?{}", param_idx));
            params_vec.push(Box::new(end_ts));
            param_idx += 1;
        }

        // 排除已有的 ID
        if !exclude_ids.is_empty() {
            let placeholders: Vec<String> = exclude_ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", param_idx + i))
                .collect();
            where_clauses.push(format!("m.id NOT IN ({})", placeholders.join(", ")));
            for id in exclude_ids {
                params_vec.push(Box::new(*id));
            }
            param_idx += exclude_ids.len();
        }

        // Session ID 前缀过滤
        if !session_ids.is_empty() {
            let session_likes: Vec<String> = session_ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("m.session_id LIKE ?{} ESCAPE '\\'", param_idx + i))
                .collect();
            where_clauses.push(format!("({})", session_likes.join(" OR ")));
            for sid in session_ids {
                params_vec.push(Box::new(format!("{}%", escape_like_pattern(sid))));
            }
            param_idx += session_ids.len();
        }

        params_vec.push(Box::new(limit as i64));

        let sql = format!(
            r#"
            SELECT
                m.id,
                m.session_id,
                s.project_id,
                p.name as project_name,
                m.type,
                m.content_full,
                substr(m.content_full, 1, 200) as snippet,
                0.0 as score,
                m.timestamp
            FROM messages m
            JOIN sessions s ON m.session_id = s.session_id
            JOIN projects p ON s.project_id = p.id
            WHERE {}
            {}
            LIMIT ?{}
            "#,
            where_clauses.join(" AND "),
            order_clause,
            param_idx
        );

        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

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
        assert_eq!(
            escape_fts5_query("test-case.rs:123"),
            "\"test-case.rs:123\""
        );
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
        assert_eq!(escape_fts5_query("open --env"), "\"open\" OR \"--env\"");
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
