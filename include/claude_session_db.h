#ifndef CLAUDE_SESSION_DB_H
#define CLAUDE_SESSION_DB_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include "stdint.h"
#include "stdbool.h"
#include "stddef.h"

/**
 * FFI 友好的错误码
 */
typedef enum SessionDbError {
    Success = 0,
    NullPointer = 1,
    InvalidUtf8 = 2,
    DatabaseError = 3,
    CoordinationError = 4,
    PermissionDenied = 5,
    Unknown = 99,
} SessionDbError;

/**
 * 不透明句柄
 */
typedef struct SessionDbHandle SessionDbHandle;

/**
 * Project C 结构体
 */
typedef struct Project {
    int64_t id;
    char *name;
    char *path;
    char *source;
    int64_t created_at;
    int64_t updated_at;
} Project;

/**
 * C 数组 wrapper
 */
typedef struct ProjectArray {
    struct Project *data;
    uintptr_t len;
} ProjectArray;

/**
 * Session C 结构体
 */
typedef struct Session {
    int64_t id;
    char *session_id;
    int64_t project_id;
    int64_t message_count;
    int64_t last_message_at;
    int64_t created_at;
    int64_t updated_at;
} Session;

/**
 * C 数组 wrapper
 */
typedef struct SessionArray {
    struct Session *data;
    uintptr_t len;
} SessionArray;

/**
 * Message C 输入结构体
 */
typedef struct MessageInputC {
    const char *uuid;
    int32_t role;
    const char *content;
    int64_t timestamp;
    int64_t sequence;
} MessageInputC;

/**
 * Message C 输出结构体
 */
typedef struct MessageC {
    int64_t id;
    char *session_id;
    char *uuid;
    int32_t role;
    char *content;
    int64_t timestamp;
    int64_t sequence;
} MessageC;

/**
 * C 数组 wrapper
 */
typedef struct MessageArray {
    struct MessageC *data;
    uintptr_t len;
} MessageArray;

/**
 * SearchResult C 结构体
 */
typedef struct SearchResultC {
    int64_t message_id;
    char *session_id;
    int64_t project_id;
    char *project_name;
    char *role;
    char *content;
    char *snippet;
    double score;
    int64_t timestamp;
} SearchResultC;

/**
 * C 数组 wrapper
 */
typedef struct SearchResultArray {
    struct SearchResultC *data;
    uintptr_t len;
} SearchResultArray;

/**
 * IndexableMessage C 结构体
 */
typedef struct IndexableMessageC {
    char *uuid;
    char *role;
    char *content;
    int64_t timestamp;
    int64_t sequence;
} IndexableMessageC;

/**
 * IndexableMessage 数组
 */
typedef struct IndexableMessageArray {
    struct IndexableMessageC *data;
    uintptr_t len;
} IndexableMessageArray;

/**
 * IndexableSession C 结构体
 */
typedef struct IndexableSessionC {
    char *session_id;
    char *project_path;
    char *project_name;
    struct IndexableMessageArray messages;
} IndexableSessionC;

/**
 * 解析结果
 */
typedef struct ParseResult {
    struct IndexableSessionC *session;
    enum SessionDbError error;
} ParseResult;

/**
 * ProjectInfo C 结构体
 */
typedef struct ProjectInfoC {
    char *encoded_name;
    char *path;
    char *name;
    uintptr_t session_count;
    uint64_t last_active;
} ProjectInfoC;

/**
 * ProjectInfo 数组
 */
typedef struct ProjectInfoArray {
    struct ProjectInfoC *data;
    uintptr_t len;
} ProjectInfoArray;

/**
 * SessionMetaC 结构体
 */
typedef struct SessionMetaC {
    char *id;
    char *project_path;
    char *project_name;
    char *encoded_dir_name;
    char *session_path;
    int64_t file_mtime;
} SessionMetaC;

/**
 * SessionMeta 数组
 */
typedef struct SessionMetaArray {
    struct SessionMetaC *data;
    uintptr_t len;
} SessionMetaArray;

/**
 * ParsedMessage C 结构体 (用于 read_messages)
 */
typedef struct ParsedMessageC {
    char *uuid;
    char *session_id;
    int32_t message_type;
    char *content;
    char *timestamp;
} ParsedMessageC;

/**
 * MessagesResult C 结构体
 */
typedef struct MessagesResultC {
    struct ParsedMessageC *messages;
    uintptr_t message_count;
    uintptr_t total;
    bool has_more;
} MessagesResultC;

/**
 * 连接数据库
 *
 * # Safety
 * `path` 必须是有效的 C 字符串
 */
enum SessionDbError session_db_connect(const char *path, struct SessionDbHandle **out_handle);

/**
 * 关闭数据库连接
 *
 * # Safety
 * `handle` 必须是 `session_db_connect` 返回的有效句柄
 */
void session_db_close(struct SessionDbHandle *handle);

/**
 * 注册为 Writer
 *
 * # Safety
 * `handle` 必须是有效句柄
 */
enum SessionDbError session_db_register_writer(struct SessionDbHandle *handle,
                                               int32_t writer_type,
                                               int32_t *out_role);

/**
 * 心跳
 *
 * # Safety
 * `handle` 必须是有效句柄
 */
enum SessionDbError session_db_heartbeat(struct SessionDbHandle *handle);

/**
 * 释放 Writer
 *
 * # Safety
 * `handle` 必须是有效句柄
 */
enum SessionDbError session_db_release_writer(struct SessionDbHandle *handle);

/**
 * 检查 Writer 健康状态
 *
 * # Safety
 * `handle` 必须是有效句柄
 * `out_health` 输出健康状态: 0=Alive, 1=Timeout, 2=Released
 */
enum SessionDbError session_db_check_writer_health(const struct SessionDbHandle *handle,
                                                   int32_t *out_health);

/**
 * 尝试接管 Writer (Reader 在检测到超时后调用)
 *
 * # Safety
 * `handle` 必须是有效句柄
 * `out_taken` 输出是否接管成功: 1=成功, 0=失败
 */
enum SessionDbError session_db_try_takeover(struct SessionDbHandle *handle, int32_t *out_taken);

/**
 * 获取统计信息
 *
 * # Safety
 * `handle` 必须是有效句柄
 */
enum SessionDbError session_db_get_stats(const struct SessionDbHandle *handle,
                                         int64_t *out_projects,
                                         int64_t *out_sessions,
                                         int64_t *out_messages);

/**
 * 获取或创建 Project
 *
 * # Safety
 * `handle`, `name`, `path`, `source` 必须是有效的 C 字符串
 */
enum SessionDbError session_db_upsert_project(struct SessionDbHandle *handle,
                                              const char *name,
                                              const char *path,
                                              const char *source,
                                              int64_t *out_id);

/**
 * 列出所有 Projects
 *
 * # Safety
 * `handle` 必须是有效句柄，返回的数组需要调用 `session_db_free_projects` 释放
 */
enum SessionDbError session_db_list_projects(const struct SessionDbHandle *handle,
                                             struct ProjectArray **out_array);

/**
 * 释放 Projects 数组
 *
 * # Safety
 * `array` 必须是 `session_db_list_projects` 返回的有效指针
 */
void session_db_free_projects(struct ProjectArray *array);

/**
 * 创建或更新 Session
 *
 * # Safety
 * `handle`, `session_id` 必须是有效的 C 字符串
 */
enum SessionDbError session_db_upsert_session(struct SessionDbHandle *handle,
                                              const char *session_id,
                                              int64_t project_id);

/**
 * 列出 Project 的 Sessions
 *
 * # Safety
 * `handle` 必须是有效句柄，返回的数组需要调用 `session_db_free_sessions` 释放
 */
enum SessionDbError session_db_list_sessions(const struct SessionDbHandle *handle,
                                             int64_t project_id,
                                             struct SessionArray **out_array);

/**
 * 释放 Sessions 数组
 *
 * # Safety
 * `array` 必须是 `session_db_list_sessions` 返回的有效指针
 */
void session_db_free_sessions(struct SessionArray *array);

/**
 * 获取 session 的扫描检查点
 *
 * # Safety
 * `handle`, `session_id` 必须是有效的 C 字符串
 */
enum SessionDbError session_db_get_scan_checkpoint(const struct SessionDbHandle *handle,
                                                   const char *session_id,
                                                   int64_t *out_timestamp);

/**
 * 更新 session 的最后消息时间
 *
 * # Safety
 * `handle`, `session_id` 必须是有效的 C 字符串
 */
enum SessionDbError session_db_update_session_last_message(struct SessionDbHandle *handle,
                                                           const char *session_id,
                                                           int64_t timestamp);

/**
 * 批量插入 Messages
 *
 * # Safety
 * `handle`, `session_id`, `messages` 必须是有效指针
 */
enum SessionDbError session_db_insert_messages(struct SessionDbHandle *handle,
                                               const char *session_id,
                                               const struct MessageInputC *messages,
                                               uintptr_t message_count,
                                               uintptr_t *out_inserted);

/**
 * 列出 Session 的 Messages
 *
 * # Safety
 * `handle`, `session_id` 必须是有效指针，返回的数组需要调用 `session_db_free_messages` 释放
 */
enum SessionDbError session_db_list_messages(const struct SessionDbHandle *handle,
                                             const char *session_id,
                                             uintptr_t limit,
                                             uintptr_t offset,
                                             struct MessageArray **out_array);

/**
 * 释放 Messages 数组
 *
 * # Safety
 * `array` 必须是 `session_db_list_messages` 返回的有效指针
 */
void session_db_free_messages(struct MessageArray *array);

/**
 * FTS5 全文搜索
 *
 * # Safety
 * `handle`, `query` 必须是有效指针，返回的数组需要调用 `session_db_free_search_results` 释放
 */
enum SessionDbError session_db_search_fts(const struct SessionDbHandle *handle,
                                          const char *query,
                                          uintptr_t limit,
                                          struct SearchResultArray **out_array);

/**
 * FTS5 全文搜索 (限定 Project)
 *
 * # Safety
 * `handle`, `query` 必须是有效指针，返回的数组需要调用 `session_db_free_search_results` 释放
 */
enum SessionDbError session_db_search_fts_with_project(const struct SessionDbHandle *handle,
                                                       const char *query,
                                                       uintptr_t limit,
                                                       int64_t project_id,
                                                       struct SearchResultArray **out_array);

/**
 * 释放 SearchResults 数组
 *
 * # Safety
 * `array` 必须是 `session_db_search_fts*` 返回的有效指针
 */
void session_db_free_search_results(struct SearchResultArray *array);

/**
 * 释放 C 字符串
 *
 * # Safety
 * `s` 必须是由本库创建的 C 字符串
 */
void session_db_free_string(char *s);

/**
 * 解析 JSONL 会话文件
 *
 * # 参数
 * - `jsonl_path`: JSONL 文件完整路径
 *
 * # 返回
 * - 成功: session 非空, error = Success
 * - 文件为空: session 为空, error = Success
 * - 失败: session 为空, error = 对应错误码
 *
 * # Safety
 * - `jsonl_path` 必须是有效的 UTF-8 C 字符串
 * - 返回的 ParseResult.session 需要调用 `session_db_free_parse_result` 释放
 */
struct ParseResult session_db_parse_jsonl(const char *jsonl_path);

/**
 * 释放解析结果
 *
 * # Safety
 * `result` 必须是 `session_db_parse_jsonl` 返回的有效指针
 */
void session_db_free_parse_result(struct IndexableSessionC *session);

/**
 * 编码项目路径为 Claude 目录名
 * /Users/xxx/project → -Users-xxx-project
 *
 * # Safety
 * - `path` 必须是有效的 UTF-8 C 字符串
 * - 返回的字符串需要调用 `session_db_free_string` 释放
 */
char *session_db_encode_path(const char *path);

/**
 * 解码 Claude 目录名为项目路径
 * -Users-xxx-project → /Users/xxx/project
 *
 * # Safety
 * - `encoded` 必须是有效的 UTF-8 C 字符串
 * - 返回的字符串需要调用 `session_db_free_string` 释放
 */
char *session_db_decode_path(const char *encoded);

/**
 * 列出所有项目（从文件系统）
 *
 * # 参数
 * - `projects_path`: Claude projects 目录路径，null 使用默认路径 (~/.claude/projects)
 * - `limit`: 最大返回数量，0 表示不限制
 *
 * # Safety
 * - 返回的数组需要调用 `session_db_free_project_list` 释放
 */
enum SessionDbError session_db_list_file_projects(const char *projects_path,
                                                  uint32_t limit,
                                                  struct ProjectInfoArray **out_array);

/**
 * 释放项目列表
 */
void session_db_free_project_list(struct ProjectInfoArray *array);

/**
 * 列出会话
 *
 * # 参数
 * - `projects_path`: Claude projects 目录路径，null 使用默认路径
 * - `project_path`: 可选，过滤特定项目的会话
 *
 * # Safety
 * - 返回的数组需要调用 `session_db_free_session_meta_list` 释放
 */
enum SessionDbError session_db_list_session_metas(const char *projects_path,
                                                  const char *project_path,
                                                  struct SessionMetaArray **out_array);

/**
 * 释放会话列表
 */
void session_db_free_session_meta_list(struct SessionMetaArray *array);

/**
 * 查找最新会话
 *
 * # 参数
 * - `projects_path`: Claude projects 目录路径，null 使用默认路径
 * - `project_path`: 项目路径
 * - `within_seconds`: 时间范围（秒），0 表示不限制
 *
 * # 返回
 * - 成功且找到：返回 SessionMetaC 指针
 * - 成功但未找到：返回 null，error = Success
 *
 * # Safety
 * - 返回的指针需要调用 `session_db_free_session_meta` 释放
 */
enum SessionDbError session_db_find_latest_session(const char *projects_path,
                                                   const char *project_path,
                                                   uint64_t within_seconds,
                                                   struct SessionMetaC **out_session);

/**
 * 释放单个 SessionMeta
 */
void session_db_free_session_meta(struct SessionMetaC *session);

/**
 * 读取会话消息（支持分页）
 *
 * # 参数
 * - `session_path`: 会话文件完整路径
 * - `limit`: 每页消息数
 * - `offset`: 偏移量
 * - `order_asc`: true 升序，false 降序
 *
 * # Safety
 * - 返回的结果需要调用 `session_db_free_messages_result` 释放
 */
enum SessionDbError session_db_read_session_messages(const char *session_path,
                                                     uintptr_t limit,
                                                     uintptr_t offset,
                                                     bool order_asc,
                                                     struct MessagesResultC **out_result);

/**
 * 释放消息结果
 */
void session_db_free_messages_result(struct MessagesResultC *result);

#endif  /* CLAUDE_SESSION_DB_H */
