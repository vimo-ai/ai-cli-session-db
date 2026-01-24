//! C FFI 导出 (Swift 绑定用)
//!
//! 为 MemexKit / VlaudeKit 提供 C ABI 接口
//!
//! FFI 层只做类型转换，业务逻辑统一在 reader 模块实现。

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::DbConfig;
use crate::db::{MessageInput, SessionDB};
use crate::reader::SessionReader;
use crate::{ClaudeAdapter, ConversationAdapter};
use ai_cli_session_collector::MessageType;

/// FFI 统一错误码
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiError {
    Success = 0,
    NullPointer = 1,
    InvalidUtf8 = 2,
    // 数据库相关
    DatabaseError = 3,
    CoordinationError = 4,
    PermissionDenied = 5,
    // Agent Client 相关
    ConnectionFailed = 6,
    NotConnected = 7,
    RequestFailed = 8,
    AgentNotFound = 9,
    RuntimeError = 10,
    // 通用
    Unknown = 99,
}

/// 将 Rust Error 映射到 FfiError
fn map_error(e: crate::error::Error) -> FfiError {
    match e {
        crate::error::Error::PermissionDenied => FfiError::PermissionDenied,
        crate::error::Error::Coordination(_) => FfiError::CoordinationError,
        _ => FfiError::DatabaseError,
    }
}

/// 不透明句柄
pub struct SessionDbHandle {
    db: SessionDB,
}

/// 连接数据库
///
/// # Safety
/// `path` 可以为 null（使用默认路径），或有效的 C 字符串
#[no_mangle]
pub unsafe extern "C" fn session_db_connect(
    path: *const c_char,
    out_handle: *mut *mut SessionDbHandle,
) -> FfiError {
    if out_handle.is_null() {
        return FfiError::NullPointer;
    }

    // path 为 null 时使用默认配置
    let config = if path.is_null() {
        DbConfig::from_env()
    } else {
        match CStr::from_ptr(path).to_str() {
            Ok(s) => DbConfig::local(s),
            Err(_) => return FfiError::InvalidUtf8,
        }
    };

    match SessionDB::connect(config) {
        Ok(db) => {
            let handle = Box::new(SessionDbHandle { db });
            *out_handle = Box::into_raw(handle);
            FfiError::Success
        }
        Err(_) => FfiError::DatabaseError,
    }
}

/// 关闭数据库连接
///
/// # Safety
/// `handle` 必须是 `session_db_connect` 返回的有效句柄
#[no_mangle]
pub unsafe extern "C" fn session_db_close(handle: *mut SessionDbHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// 注册为 Writer
///
/// # Safety
/// `handle` 必须是有效句柄
#[cfg(feature = "coordination")]
#[no_mangle]
pub unsafe extern "C" fn session_db_register_writer(
    handle: *mut SessionDbHandle,
    writer_type: i32,
    out_role: *mut i32,
) -> FfiError {
    use crate::coordination::{Role, WriterType};

    if handle.is_null() || out_role.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &mut *handle;
    let writer_type = match writer_type {
        0 => WriterType::MemexDaemon,
        1 => WriterType::VlaudeDaemon,
        2 => WriterType::MemexKit,
        3 => WriterType::VlaudeKit,
        _ => return FfiError::Unknown,
    };

    match handle.db.register_writer(writer_type) {
        Ok(role) => {
            *out_role = match role {
                Role::Writer => 0,
                Role::Reader => 1,
            };
            FfiError::Success
        }
        Err(_) => FfiError::CoordinationError,
    }
}

/// 心跳
///
/// # Safety
/// `handle` 必须是有效句柄
#[cfg(feature = "coordination")]
#[no_mangle]
pub unsafe extern "C" fn session_db_heartbeat(handle: *mut SessionDbHandle) -> FfiError {
    if handle.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &*handle;
    match handle.db.heartbeat() {
        Ok(_) => FfiError::Success,
        Err(_) => FfiError::CoordinationError,
    }
}

/// 释放 Writer
///
/// # Safety
/// `handle` 必须是有效句柄
#[cfg(feature = "coordination")]
#[no_mangle]
pub unsafe extern "C" fn session_db_release_writer(handle: *mut SessionDbHandle) -> FfiError {
    if handle.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &*handle;
    match handle.db.release_writer() {
        Ok(_) => FfiError::Success,
        Err(_) => FfiError::CoordinationError,
    }
}

/// 检查 Writer 健康状态
///
/// # Safety
/// `handle` 必须是有效句柄
/// `out_health` 输出健康状态: 0=Alive, 1=Timeout, 2=Released
#[cfg(feature = "coordination")]
#[no_mangle]
pub unsafe extern "C" fn session_db_check_writer_health(
    handle: *const SessionDbHandle,
    out_health: *mut i32,
) -> FfiError {
    use crate::coordination::WriterHealth;

    if handle.is_null() || out_health.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &*handle;
    match handle.db.check_writer_health() {
        Ok(health) => {
            *out_health = match health {
                WriterHealth::Alive => 0,
                WriterHealth::Timeout => 1,
                WriterHealth::Released => 2,
            };
            FfiError::Success
        }
        Err(_) => FfiError::CoordinationError,
    }
}

/// 尝试接管 Writer (Reader 在检测到超时后调用)
///
/// # Safety
/// `handle` 必须是有效句柄
/// `out_taken` 输出是否接管成功: 1=成功, 0=失败
#[cfg(feature = "coordination")]
#[no_mangle]
pub unsafe extern "C" fn session_db_try_takeover(
    handle: *mut SessionDbHandle,
    out_taken: *mut i32,
) -> FfiError {
    if handle.is_null() || out_taken.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &mut *handle;
    match handle.db.try_takeover() {
        Ok(taken) => {
            *out_taken = if taken { 1 } else { 0 };
            FfiError::Success
        }
        Err(_) => FfiError::CoordinationError,
    }
}

/// 注册为 Writer 并在成为 Writer 时自动触发全量采集
///
/// 此接口统一了"成为 Writer 时触发采集"的逻辑，供所有组件使用。
///
/// # Safety
/// `handle` 必须是有效句柄
/// `out_role` 输出角色: 0=Writer, 1=Reader
/// `out_result` 如果不为 null，输出采集结果（仅当成为 Writer 时有值）
#[cfg(feature = "coordination")]
#[no_mangle]
pub unsafe extern "C" fn session_db_register_writer_and_collect(
    handle: *mut SessionDbHandle,
    writer_type: i32,
    out_role: *mut i32,
    out_result: *mut *mut CollectResultC,
) -> FfiError {
    use crate::coordination::{Role, WriterType};

    if handle.is_null() || out_role.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &mut *handle;
    let writer_type = match writer_type {
        0 => WriterType::MemexDaemon,
        1 => WriterType::VlaudeDaemon,
        2 => WriterType::MemexKit,
        3 => WriterType::VlaudeKit,
        _ => return FfiError::Unknown,
    };

    match handle.db.register_writer_and_collect(writer_type) {
        Ok((role, collect_result)) => {
            *out_role = match role {
                Role::Writer => 0,
                Role::Reader => 1,
            };

            // 如果提供了 out_result 指针且有采集结果，填充结果
            if !out_result.is_null() {
                if let Some(result) = collect_result {
                    let first_error = if result.errors.is_empty() {
                        std::ptr::null_mut()
                    } else {
                        CString::new(result.errors.join("; "))
                            .map(|s| s.into_raw())
                            .unwrap_or(std::ptr::null_mut())
                    };

                    let c_result = Box::new(CollectResultC {
                        projects_scanned: result.projects_scanned,
                        sessions_scanned: result.sessions_scanned,
                        messages_inserted: result.messages_inserted,
                        error_count: result.errors.len(),
                        first_error,
                    });
                    *out_result = Box::into_raw(c_result);
                } else {
                    *out_result = std::ptr::null_mut();
                }
            }

            FfiError::Success
        }
        Err(_) => FfiError::CoordinationError,
    }
}

/// 获取统计信息
///
/// # Safety
/// `handle` 必须是有效句柄
#[no_mangle]
pub unsafe extern "C" fn session_db_get_stats(
    handle: *const SessionDbHandle,
    out_projects: *mut i64,
    out_sessions: *mut i64,
    out_messages: *mut i64,
) -> FfiError {
    if handle.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &*handle;
    match handle.db.get_stats() {
        Ok(stats) => {
            if !out_projects.is_null() {
                *out_projects = stats.project_count;
            }
            if !out_sessions.is_null() {
                *out_sessions = stats.session_count;
            }
            if !out_messages.is_null() {
                *out_messages = stats.message_count;
            }
            FfiError::Success
        }
        Err(_) => FfiError::DatabaseError,
    }
}

// ==================== Project CRUD ====================

/// 获取或创建 Project
///
/// # Safety
/// `handle`, `name`, `path`, `source` 必须是有效的 C 字符串
#[no_mangle]
pub unsafe extern "C" fn session_db_upsert_project(
    handle: *mut SessionDbHandle,
    name: *const c_char,
    path: *const c_char,
    source: *const c_char,
    out_id: *mut i64,
) -> FfiError {
    if handle.is_null() || name.is_null() || path.is_null() || source.is_null() || out_id.is_null()
    {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let name_str = match CStr::from_ptr(name).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        let source_str = match CStr::from_ptr(source).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };

        match handle
            .db
            .get_or_create_project(name_str, path_str, source_str)
        {
            Ok(id) => Ok(id),
            Err(e) => Err(map_error(e)),
        }
    }));

    match result {
        Ok(Ok(id)) => {
            *out_id = id;
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// C 数组 wrapper
#[repr(C)]
pub struct ProjectArray {
    pub data: *mut Project,
    pub len: usize,
}

/// Project C 结构体
#[repr(C)]
pub struct Project {
    pub id: i64,
    pub name: *mut c_char,
    pub path: *mut c_char,
    pub source: *mut c_char,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 列出所有 Projects
///
/// # Safety
/// `handle` 必须是有效句柄，返回的数组需要调用 `session_db_free_projects` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_list_projects(
    handle: *const SessionDbHandle,
    out_array: *mut *mut ProjectArray,
) -> FfiError {
    if handle.is_null() || out_array.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        match handle.db.list_projects() {
            Ok(projects) => Ok(projects),
            Err(_) => Err(FfiError::DatabaseError),
        }
    }));

    match result {
        Ok(Ok(projects)) => {
            let mut c_projects: Vec<Project> = Vec::new();
            for p in projects {
                let name = match CString::new(p.name) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let path = match CString::new(p.path) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let source = match CString::new(p.source) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };

                c_projects.push(Project {
                    id: p.id,
                    name,
                    path,
                    source,
                    created_at: p.created_at,
                    updated_at: p.updated_at,
                });
            }

            let len = c_projects.len();
            let data = c_projects.as_mut_ptr();
            std::mem::forget(c_projects);

            let array = Box::new(ProjectArray { data, len });
            *out_array = Box::into_raw(array);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// 释放 Projects 数组
///
/// # Safety
/// `array` 必须是 `session_db_list_projects` 返回的有效指针
#[no_mangle]
pub unsafe extern "C" fn session_db_free_projects(array: *mut ProjectArray) {
    if array.is_null() {
        return;
    }

    let array = Box::from_raw(array);
    let projects = Vec::from_raw_parts(array.data, array.len, array.len);
    for p in projects {
        if !p.name.is_null() {
            drop(CString::from_raw(p.name));
        }
        if !p.path.is_null() {
            drop(CString::from_raw(p.path));
        }
        if !p.source.is_null() {
            drop(CString::from_raw(p.source));
        }
    }
}

// ==================== Session CRUD ====================

/// 创建或更新 Session
///
/// # Safety
/// `handle`, `session_id` 必须是有效的 C 字符串
#[no_mangle]
pub unsafe extern "C" fn session_db_upsert_session(
    handle: *mut SessionDbHandle,
    session_id: *const c_char,
    project_id: i64,
) -> FfiError {
    if handle.is_null() || session_id.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let session_id_str = match CStr::from_ptr(session_id).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        match handle.db.upsert_session(session_id_str, project_id) {
            Ok(_) => Ok(()),
            Err(e) => Err(map_error(e)),
        }
    }));

    match result {
        Ok(Ok(_)) => FfiError::Success,
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// Session C 结构体
#[repr(C)]
pub struct Session {
    pub id: i64,
    pub session_id: *mut c_char,
    pub project_id: i64,
    pub message_count: i64,
    pub last_message_at: i64, // -1 表示 NULL
    pub created_at: i64,
    pub updated_at: i64,
}

/// C 数组 wrapper
#[repr(C)]
pub struct SessionArray {
    pub data: *mut Session,
    pub len: usize,
}

/// 列出 Project 的 Sessions
///
/// # Safety
/// `handle` 必须是有效句柄，返回的数组需要调用 `session_db_free_sessions` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_list_sessions(
    handle: *const SessionDbHandle,
    project_id: i64,
    out_array: *mut *mut SessionArray,
) -> FfiError {
    if handle.is_null() || out_array.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        match handle.db.list_sessions(project_id) {
            Ok(sessions) => Ok(sessions),
            Err(_) => Err(FfiError::DatabaseError),
        }
    }));

    match result {
        Ok(Ok(sessions)) => {
            let mut c_sessions: Vec<Session> = Vec::new();
            for s in sessions {
                let session_id = match CString::new(s.session_id) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };

                c_sessions.push(Session {
                    id: s.id,
                    session_id,
                    project_id: s.project_id,
                    message_count: s.message_count,
                    last_message_at: s.last_message_at.unwrap_or(-1),
                    created_at: s.created_at,
                    updated_at: s.updated_at,
                });
            }

            let len = c_sessions.len();
            let data = c_sessions.as_mut_ptr();
            std::mem::forget(c_sessions);

            let array = Box::new(SessionArray { data, len });
            *out_array = Box::into_raw(array);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// 释放 Sessions 数组
///
/// # Safety
/// `array` 必须是 `session_db_list_sessions` 返回的有效指针
#[no_mangle]
pub unsafe extern "C" fn session_db_free_sessions(array: *mut SessionArray) {
    if array.is_null() {
        return;
    }

    let array = Box::from_raw(array);
    let sessions = Vec::from_raw_parts(array.data, array.len, array.len);
    for s in sessions {
        if !s.session_id.is_null() {
            drop(CString::from_raw(s.session_id));
        }
    }
}

/// 获取 session 的扫描检查点
///
/// # Safety
/// `handle`, `session_id` 必须是有效的 C 字符串
#[no_mangle]
pub unsafe extern "C" fn session_db_get_scan_checkpoint(
    handle: *const SessionDbHandle,
    session_id: *const c_char,
    out_timestamp: *mut i64,
) -> FfiError {
    if handle.is_null() || session_id.is_null() || out_timestamp.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let session_id_str = match CStr::from_ptr(session_id).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        match handle.db.get_scan_checkpoint(session_id_str) {
            Ok(ts) => Ok(ts),
            Err(_) => Err(FfiError::DatabaseError),
        }
    }));

    match result {
        Ok(Ok(Some(ts))) => {
            *out_timestamp = ts;
            FfiError::Success
        }
        Ok(Ok(None)) => {
            *out_timestamp = -1; // 使用 -1 表示 NULL
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// 更新 session 的最后消息时间
///
/// # Safety
/// `handle`, `session_id` 必须是有效的 C 字符串
#[no_mangle]
pub unsafe extern "C" fn session_db_update_session_last_message(
    handle: *mut SessionDbHandle,
    session_id: *const c_char,
    timestamp: i64,
) -> FfiError {
    if handle.is_null() || session_id.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let session_id_str = match CStr::from_ptr(session_id).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        match handle
            .db
            .update_session_last_message(session_id_str, timestamp)
        {
            Ok(_) => Ok(()),
            Err(e) => Err(map_error(e)),
        }
    }));

    match result {
        Ok(Ok(_)) => FfiError::Success,
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

// ==================== Message CRUD ====================

/// Message C 输入结构体
#[repr(C)]
pub struct MessageInputC {
    pub uuid: *const c_char,
    pub role: i32, // 0 = Human, 1 = Assistant
    pub content: *const c_char,
    pub timestamp: i64,
    pub sequence: i64,
}

/// 批量插入 Messages
///
/// # Safety
/// `handle`, `session_id`, `messages` 必须是有效指针
#[no_mangle]
pub unsafe extern "C" fn session_db_insert_messages(
    handle: *mut SessionDbHandle,
    session_id: *const c_char,
    messages: *const MessageInputC,
    message_count: usize,
    out_inserted: *mut usize,
) -> FfiError {
    if handle.is_null() || session_id.is_null() || messages.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let session_id_str = match CStr::from_ptr(session_id).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };

        let messages_slice = std::slice::from_raw_parts(messages, message_count);
        let mut rust_messages = Vec::new();

        for msg in messages_slice {
            let uuid = match CStr::from_ptr(msg.uuid).to_str() {
                Ok(s) => s.to_string(),
                Err(_) => return Err(FfiError::InvalidUtf8),
            };
            let content = match CStr::from_ptr(msg.content).to_str() {
                Ok(s) => s.to_string(),
                Err(_) => return Err(FfiError::InvalidUtf8),
            };
            let msg_type = match msg.role {
                0 => MessageType::User,
                1 => MessageType::Assistant,
                2 => MessageType::Tool,
                _ => return Err(FfiError::Unknown),
            };

            // FFI 层简化：单一 content 映射到 content_text 和 content_full
            rust_messages.push(MessageInput {
                uuid,
                r#type: msg_type,
                content_text: content.clone(), // 向量化用
                content_full: content,         // FTS 用
                timestamp: msg.timestamp,
                sequence: msg.sequence,
                source: None,
                channel: None,
                model: None,
                tool_call_id: None,
                tool_name: None,
                tool_args: None,
                raw: None,
                approval_status: None,
                approval_resolved_at: None,
            });
        }

        match handle.db.insert_messages(session_id_str, &rust_messages) {
            Ok(inserted) => Ok(inserted),
            Err(e) => Err(map_error(e)),
        }
    }));

    match result {
        Ok(Ok(inserted)) => {
            if !out_inserted.is_null() {
                *out_inserted = inserted;
            }
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// Message C 输出结构体
#[repr(C)]
pub struct MessageC {
    pub id: i64,
    pub session_id: *mut c_char,
    pub uuid: *mut c_char,
    pub role: i32, // 0 = Human, 1 = Assistant
    pub content: *mut c_char,
    pub timestamp: i64,
    pub sequence: i64,
    pub raw: *mut c_char, // 原始 JSONL 数据，用于解析 contentBlocks
}

/// C 数组 wrapper
#[repr(C)]
pub struct MessageArray {
    pub data: *mut MessageC,
    pub len: usize,
}

/// 列出 Session 的 Messages
///
/// # Safety
/// `handle`, `session_id` 必须是有效指针，返回的数组需要调用 `session_db_free_messages` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_list_messages(
    handle: *const SessionDbHandle,
    session_id: *const c_char,
    limit: usize,
    offset: usize,
    out_array: *mut *mut MessageArray,
) -> FfiError {
    if handle.is_null() || session_id.is_null() || out_array.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let session_id_str = match CStr::from_ptr(session_id).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        match handle.db.list_messages(session_id_str, limit, offset) {
            Ok(messages) => Ok(messages),
            Err(_) => Err(FfiError::DatabaseError),
        }
    }));

    match result {
        Ok(Ok(messages)) => {
            let mut c_messages: Vec<MessageC> = Vec::new();
            for m in messages {
                let session_id = match CString::new(m.session_id) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let uuid = match CString::new(m.uuid) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                // FFI 输出使用 content_full
                let content = match CString::new(m.content_full) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };

                let role = match m.r#type {
                    MessageType::User => 0,
                    MessageType::Assistant => 1,
                    MessageType::Tool => 2,
                };

                let raw = match m.raw {
                    Some(r) => match CString::new(r) {
                        Ok(s) => s.into_raw(),
                        Err(_) => std::ptr::null_mut(),
                    },
                    None => std::ptr::null_mut(),
                };

                c_messages.push(MessageC {
                    id: m.id,
                    session_id,
                    uuid,
                    role,
                    content,
                    timestamp: m.timestamp,
                    sequence: m.sequence,
                    raw,
                });
            }

            let len = c_messages.len();
            let data = c_messages.as_mut_ptr();
            std::mem::forget(c_messages);

            let array = Box::new(MessageArray { data, len });
            *out_array = Box::into_raw(array);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// 释放 Messages 数组
///
/// # Safety
/// `array` 必须是 `session_db_list_messages` 返回的有效指针
#[no_mangle]
pub unsafe extern "C" fn session_db_free_messages(array: *mut MessageArray) {
    if array.is_null() {
        return;
    }

    let array = Box::from_raw(array);
    let messages = Vec::from_raw_parts(array.data, array.len, array.len);
    for m in messages {
        if !m.session_id.is_null() {
            drop(CString::from_raw(m.session_id));
        }
        if !m.uuid.is_null() {
            drop(CString::from_raw(m.uuid));
        }
        if !m.content.is_null() {
            drop(CString::from_raw(m.content));
        }
        if !m.raw.is_null() {
            drop(CString::from_raw(m.raw));
        }
    }
}

// ==================== 搜索 (FTS5) ====================

/// SearchResult C 结构体
#[repr(C)]
pub struct SearchResultC {
    pub message_id: i64,
    pub session_id: *mut c_char,
    pub project_id: i64,
    pub project_name: *mut c_char,
    pub role: *mut c_char,
    pub content: *mut c_char,
    pub snippet: *mut c_char,
    pub score: f64,
    pub timestamp: i64, // -1 表示 NULL
}

/// C 数组 wrapper
#[repr(C)]
pub struct SearchResultArray {
    pub data: *mut SearchResultC,
    pub len: usize,
}

/// 转义 FTS5 查询特殊字符
fn escape_fts5_query(query: &str) -> String {
    // FTS5 特殊字符: " * ( ) AND OR NOT
    // 简单策略: 用双引号包裹整个查询字符串，并转义内部的双引号
    let escaped = query.replace('"', "\"\"");
    format!("\"{}\"", escaped)
}

/// 搜索排序方式 C 枚举
/// 0 = Score (相关性), 1 = TimeDesc (时间倒序), 2 = TimeAsc (时间正序)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SearchOrderByC {
    Score = 0,
    TimeDesc = 1,
    TimeAsc = 2,
}

impl From<SearchOrderByC> for crate::types::SearchOrderBy {
    fn from(order: SearchOrderByC) -> Self {
        match order {
            SearchOrderByC::Score => crate::types::SearchOrderBy::Score,
            SearchOrderByC::TimeDesc => crate::types::SearchOrderBy::TimeDesc,
            SearchOrderByC::TimeAsc => crate::types::SearchOrderBy::TimeAsc,
        }
    }
}

/// FTS5 全文搜索
///
/// # Safety
/// `handle`, `query` 必须是有效指针，返回的数组需要调用 `session_db_free_search_results` 释放
#[cfg(feature = "fts")]
#[no_mangle]
pub unsafe extern "C" fn session_db_search_fts(
    handle: *const SessionDbHandle,
    query: *const c_char,
    limit: usize,
    out_array: *mut *mut SearchResultArray,
) -> FfiError {
    if handle.is_null() || query.is_null() || out_array.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let query_str = match CStr::from_ptr(query).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        let escaped_query = escape_fts5_query(query_str);
        match handle.db.search_fts(&escaped_query, limit) {
            Ok(results) => Ok(results),
            Err(_) => Err(FfiError::DatabaseError),
        }
    }));

    match result {
        Ok(Ok(results)) => {
            let mut c_results: Vec<SearchResultC> = Vec::new();
            for r in results {
                let session_id = match CString::new(r.session_id) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let project_name = match CString::new(r.project_name) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let role = match CString::new(r.r#type.clone()) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                // 搜索结果使用 content_full
                let content = match CString::new(r.content_full) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let snippet = match CString::new(r.snippet) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };

                c_results.push(SearchResultC {
                    message_id: r.message_id,
                    session_id,
                    project_id: r.project_id,
                    project_name,
                    role,
                    content,
                    snippet,
                    score: r.score,
                    timestamp: r.timestamp.unwrap_or(-1),
                });
            }

            let len = c_results.len();
            let data = c_results.as_mut_ptr();
            std::mem::forget(c_results);

            let array = Box::new(SearchResultArray { data, len });
            *out_array = Box::into_raw(array);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// FTS5 全文搜索 (限定 Project)
///
/// # Safety
/// `handle`, `query` 必须是有效指针，返回的数组需要调用 `session_db_free_search_results` 释放
#[cfg(feature = "fts")]
#[no_mangle]
pub unsafe extern "C" fn session_db_search_fts_with_project(
    handle: *const SessionDbHandle,
    query: *const c_char,
    limit: usize,
    project_id: i64,
    out_array: *mut *mut SearchResultArray,
) -> FfiError {
    if handle.is_null() || query.is_null() || out_array.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let query_str = match CStr::from_ptr(query).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        let escaped_query = escape_fts5_query(query_str);
        let pid = if project_id >= 0 {
            Some(project_id)
        } else {
            None
        };
        match handle
            .db
            .search_fts_with_project(&escaped_query, limit, pid)
        {
            Ok(results) => Ok(results),
            Err(_) => Err(FfiError::DatabaseError),
        }
    }));

    match result {
        Ok(Ok(results)) => {
            let mut c_results: Vec<SearchResultC> = Vec::new();
            for r in results {
                let session_id = match CString::new(r.session_id) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let project_name = match CString::new(r.project_name) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let role = match CString::new(r.r#type.clone()) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                // 搜索结果使用 content_full
                let content = match CString::new(r.content_full) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let snippet = match CString::new(r.snippet) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };

                c_results.push(SearchResultC {
                    message_id: r.message_id,
                    session_id,
                    project_id: r.project_id,
                    project_name,
                    role,
                    content,
                    snippet,
                    score: r.score,
                    timestamp: r.timestamp.unwrap_or(-1),
                });
            }

            let len = c_results.len();
            let data = c_results.as_mut_ptr();
            std::mem::forget(c_results);

            let array = Box::new(SearchResultArray { data, len });
            *out_array = Box::into_raw(array);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// 释放 SearchResults 数组
///
/// # Safety
/// `array` 必须是 `session_db_search_fts*` 返回的有效指针
#[no_mangle]
pub unsafe extern "C" fn session_db_free_search_results(array: *mut SearchResultArray) {
    if array.is_null() {
        return;
    }

    let array = Box::from_raw(array);
    let results = Vec::from_raw_parts(array.data, array.len, array.len);
    for r in results {
        if !r.session_id.is_null() {
            drop(CString::from_raw(r.session_id));
        }
        if !r.project_name.is_null() {
            drop(CString::from_raw(r.project_name));
        }
        if !r.role.is_null() {
            drop(CString::from_raw(r.role));
        }
        if !r.content.is_null() {
            drop(CString::from_raw(r.content));
        }
        if !r.snippet.is_null() {
            drop(CString::from_raw(r.snippet));
        }
    }
}

/// FTS 全文搜索（完整参数版本，支持项目过滤、排序和日期范围）
///
/// # 参数
/// - `handle`: 数据库句柄
/// - `query`: 搜索关键词
/// - `limit`: 返回数量
/// - `project_id`: 项目 ID（-1 表示不过滤）
/// - `order_by`: 排序方式（0=Score, 1=TimeDesc, 2=TimeAsc）
/// - `start_timestamp`: 开始时间戳（毫秒，-1 表示不过滤）
/// - `end_timestamp`: 结束时间戳（毫秒，-1 表示不过滤）
/// - `out_array`: 输出搜索结果数组
///
/// # Safety
/// `handle`, `query` 必须是有效指针，返回的数组需要调用 `session_db_free_search_results` 释放
#[cfg(feature = "fts")]
#[no_mangle]
pub unsafe extern "C" fn session_db_search_fts_full(
    handle: *const SessionDbHandle,
    query: *const c_char,
    limit: usize,
    project_id: i64,
    order_by: SearchOrderByC,
    start_timestamp: i64,
    end_timestamp: i64,
    out_array: *mut *mut SearchResultArray,
) -> FfiError {
    if handle.is_null() || query.is_null() || out_array.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let query_str = match CStr::from_ptr(query).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        let escaped_query = escape_fts5_query(query_str);
        let pid = if project_id >= 0 {
            Some(project_id)
        } else {
            None
        };
        let start_ts = if start_timestamp >= 0 {
            Some(start_timestamp)
        } else {
            None
        };
        let end_ts = if end_timestamp >= 0 {
            Some(end_timestamp)
        } else {
            None
        };
        let order: crate::types::SearchOrderBy = order_by.into();
        match handle
            .db
            .search_fts_full(&escaped_query, limit, pid, order, start_ts, end_ts)
        {
            Ok(results) => Ok(results),
            Err(_) => Err(FfiError::DatabaseError),
        }
    }));

    match result {
        Ok(Ok(results)) => {
            let mut c_results: Vec<SearchResultC> = Vec::new();
            for r in results {
                let session_id = match CString::new(r.session_id) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let project_name = match CString::new(r.project_name) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let role = match CString::new(r.r#type.clone()) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let content = match CString::new(r.content_full) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let snippet = match CString::new(r.snippet) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };

                c_results.push(SearchResultC {
                    message_id: r.message_id,
                    session_id,
                    project_id: r.project_id,
                    project_name,
                    role,
                    content,
                    snippet,
                    score: r.score,
                    timestamp: r.timestamp.unwrap_or(-1),
                });
            }

            let len = c_results.len();
            let data = c_results.as_mut_ptr();
            std::mem::forget(c_results);

            let array = Box::new(SearchResultArray { data, len });
            *out_array = Box::into_raw(array);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// FTS 全文搜索（完整参数版本，支持项目过滤和排序）
///
/// # 参数
/// - `handle`: 数据库句柄
/// - `query`: 搜索关键词
/// - `limit`: 返回数量
/// - `project_id`: 项目 ID（-1 表示不过滤）
/// - `order_by`: 排序方式（0=Score, 1=TimeDesc, 2=TimeAsc）
/// - `out_array`: 输出搜索结果数组
///
/// # Safety
/// `handle`, `query` 必须是有效指针，返回的数组需要调用 `session_db_free_search_results` 释放
#[cfg(feature = "fts")]
#[no_mangle]
pub unsafe extern "C" fn session_db_search_fts_with_options(
    handle: *const SessionDbHandle,
    query: *const c_char,
    limit: usize,
    project_id: i64,
    order_by: SearchOrderByC,
    out_array: *mut *mut SearchResultArray,
) -> FfiError {
    if handle.is_null() || query.is_null() || out_array.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let query_str = match CStr::from_ptr(query).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };
        let escaped_query = escape_fts5_query(query_str);
        let pid = if project_id >= 0 {
            Some(project_id)
        } else {
            None
        };
        let order: crate::types::SearchOrderBy = order_by.into();
        match handle
            .db
            .search_fts_with_options(&escaped_query, limit, pid, order)
        {
            Ok(results) => Ok(results),
            Err(_) => Err(FfiError::DatabaseError),
        }
    }));

    match result {
        Ok(Ok(results)) => {
            let mut c_results: Vec<SearchResultC> = Vec::new();
            for r in results {
                let session_id = match CString::new(r.session_id) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let project_name = match CString::new(r.project_name) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let role = match CString::new(r.r#type.clone()) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let content = match CString::new(r.content_full) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };
                let snippet = match CString::new(r.snippet) {
                    Ok(s) => s.into_raw(),
                    Err(_) => return FfiError::InvalidUtf8,
                };

                c_results.push(SearchResultC {
                    message_id: r.message_id,
                    session_id,
                    project_id: r.project_id,
                    project_name,
                    role,
                    content,
                    snippet,
                    score: r.score,
                    timestamp: r.timestamp.unwrap_or(-1),
                });
            }

            let len = c_results.len();
            let data = c_results.as_mut_ptr();
            std::mem::forget(c_results);

            let array = Box::new(SearchResultArray { data, len });
            *out_array = Box::into_raw(array);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

// ==================== 审批操作 ====================

/// 审批状态 C 枚举
/// 0 = Pending, 1 = Approved, 2 = Rejected, 3 = Timeout
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ApprovalStatusC {
    Pending = 0,
    Approved = 1,
    Rejected = 2,
    Timeout = 3,
}

impl From<ApprovalStatusC> for crate::types::ApprovalStatus {
    fn from(status: ApprovalStatusC) -> Self {
        match status {
            ApprovalStatusC::Pending => crate::types::ApprovalStatus::Pending,
            ApprovalStatusC::Approved => crate::types::ApprovalStatus::Approved,
            ApprovalStatusC::Rejected => crate::types::ApprovalStatus::Rejected,
            ApprovalStatusC::Timeout => crate::types::ApprovalStatus::Timeout,
        }
    }
}

/// 通过 tool_call_id 更新审批状态
///
/// # 参数
/// - `handle`: 数据库句柄
/// - `tool_call_id`: 工具调用 ID
/// - `status`: 审批状态 (0=Pending, 1=Approved, 2=Rejected, 3=Timeout)
/// - `resolved_at`: 审批解决时间戳（毫秒，pending 状态时为 0）
/// - `out_updated`: 输出更新的行数
///
/// # Safety
/// - `handle` 必须是有效句柄
/// - `tool_call_id` 必须是有效的 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn session_db_update_approval_status_by_tool_call_id(
    handle: *mut SessionDbHandle,
    tool_call_id: *const c_char,
    status: ApprovalStatusC,
    resolved_at: i64,
    out_updated: *mut usize,
) -> FfiError {
    if handle.is_null() || tool_call_id.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let handle = &*handle;
        let tool_call_id_str = match CStr::from_ptr(tool_call_id).to_str() {
            Ok(s) => s,
            Err(_) => return Err(FfiError::InvalidUtf8),
        };

        let rust_status: crate::types::ApprovalStatus = status.into();

        match handle.db.update_approval_status_by_tool_call_id(
            tool_call_id_str,
            rust_status,
            resolved_at,
        ) {
            Ok(count) => Ok(count),
            Err(e) => Err(map_error(e)),
        }
    }));

    match result {
        Ok(Ok(count)) => {
            if !out_updated.is_null() {
                *out_updated = count;
            }
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

// ==================== 辅助函数 ====================

/// 释放 C 字符串
///
/// # Safety
/// `s` 必须是由本库创建的 C 字符串
#[no_mangle]
pub unsafe extern "C" fn session_db_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

// ==================== JSONL 解析 (统一入口) ====================

/// IndexableMessage C 结构体
#[repr(C)]
pub struct IndexableMessageC {
    pub uuid: *mut c_char,
    pub role: *mut c_char,
    pub content: *mut c_char,
    pub timestamp: i64,
    pub sequence: i64,
}

/// IndexableMessage 数组
#[repr(C)]
pub struct IndexableMessageArray {
    pub data: *mut IndexableMessageC,
    pub len: usize,
}

/// IndexableSession C 结构体
#[repr(C)]
pub struct IndexableSessionC {
    pub session_id: *mut c_char,
    pub project_path: *mut c_char,
    pub project_name: *mut c_char,
    pub messages: IndexableMessageArray,
}

/// 解析结果
#[repr(C)]
pub struct ParseResult {
    pub session: *mut IndexableSessionC,
    pub error: FfiError,
}

/// 解析 JSONL 会话文件
///
/// # 参数
/// - `jsonl_path`: JSONL 文件完整路径
///
/// # 返回
/// - 成功: session 非空, error = Success
/// - 文件为空: session 为空, error = Success
/// - 失败: session 为空, error = 对应错误码
///
/// # Safety
/// - `jsonl_path` 必须是有效的 UTF-8 C 字符串
/// - 返回的 ParseResult.session 需要调用 `session_db_free_parse_result` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_parse_jsonl(jsonl_path: *const c_char) -> ParseResult {
    use crate::ClaudeAdapter;

    if jsonl_path.is_null() {
        return ParseResult {
            session: std::ptr::null_mut(),
            error: FfiError::NullPointer,
        };
    }

    let path_str = match CStr::from_ptr(jsonl_path).to_str() {
        Ok(s) => s,
        Err(_) => {
            return ParseResult {
                session: std::ptr::null_mut(),
                error: FfiError::InvalidUtf8,
            };
        }
    };

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        ClaudeAdapter::parse_session_from_path(path_str)
    }));

    match result {
        Ok(Ok(Some(session))) => {
            // 转换消息
            let mut c_messages: Vec<IndexableMessageC> = Vec::new();
            for msg in session.messages {
                let uuid = match CString::new(msg.uuid) {
                    Ok(s) => s.into_raw(),
                    Err(_) => {
                        // 清理已分配的内存
                        for m in c_messages {
                            if !m.uuid.is_null() {
                                drop(CString::from_raw(m.uuid));
                            }
                            if !m.role.is_null() {
                                drop(CString::from_raw(m.role));
                            }
                            if !m.content.is_null() {
                                drop(CString::from_raw(m.content));
                            }
                        }
                        return ParseResult {
                            session: std::ptr::null_mut(),
                            error: FfiError::InvalidUtf8,
                        };
                    }
                };
                let role = match CString::new(msg.role) {
                    Ok(s) => s.into_raw(),
                    Err(_) => {
                        drop(CString::from_raw(uuid));
                        for m in c_messages {
                            if !m.uuid.is_null() {
                                drop(CString::from_raw(m.uuid));
                            }
                            if !m.role.is_null() {
                                drop(CString::from_raw(m.role));
                            }
                            if !m.content.is_null() {
                                drop(CString::from_raw(m.content));
                            }
                        }
                        return ParseResult {
                            session: std::ptr::null_mut(),
                            error: FfiError::InvalidUtf8,
                        };
                    }
                };
                // IndexableMessage.content 是 ParsedContent，FFI 使用 full
                let content = match CString::new(msg.content.full) {
                    Ok(s) => s.into_raw(),
                    Err(_) => {
                        drop(CString::from_raw(uuid));
                        drop(CString::from_raw(role));
                        for m in c_messages {
                            if !m.uuid.is_null() {
                                drop(CString::from_raw(m.uuid));
                            }
                            if !m.role.is_null() {
                                drop(CString::from_raw(m.role));
                            }
                            if !m.content.is_null() {
                                drop(CString::from_raw(m.content));
                            }
                        }
                        return ParseResult {
                            session: std::ptr::null_mut(),
                            error: FfiError::InvalidUtf8,
                        };
                    }
                };

                c_messages.push(IndexableMessageC {
                    uuid,
                    role,
                    content,
                    timestamp: msg.timestamp,
                    sequence: msg.sequence,
                });
            }

            // 转换 session
            let session_id = match CString::new(session.session_id) {
                Ok(s) => s.into_raw(),
                Err(_) => {
                    for m in c_messages {
                        if !m.uuid.is_null() {
                            drop(CString::from_raw(m.uuid));
                        }
                        if !m.role.is_null() {
                            drop(CString::from_raw(m.role));
                        }
                        if !m.content.is_null() {
                            drop(CString::from_raw(m.content));
                        }
                    }
                    return ParseResult {
                        session: std::ptr::null_mut(),
                        error: FfiError::InvalidUtf8,
                    };
                }
            };
            let project_path = match CString::new(session.project_path) {
                Ok(s) => s.into_raw(),
                Err(_) => {
                    drop(CString::from_raw(session_id));
                    for m in c_messages {
                        if !m.uuid.is_null() {
                            drop(CString::from_raw(m.uuid));
                        }
                        if !m.role.is_null() {
                            drop(CString::from_raw(m.role));
                        }
                        if !m.content.is_null() {
                            drop(CString::from_raw(m.content));
                        }
                    }
                    return ParseResult {
                        session: std::ptr::null_mut(),
                        error: FfiError::InvalidUtf8,
                    };
                }
            };
            let project_name = match CString::new(session.project_name) {
                Ok(s) => s.into_raw(),
                Err(_) => {
                    drop(CString::from_raw(session_id));
                    drop(CString::from_raw(project_path));
                    for m in c_messages {
                        if !m.uuid.is_null() {
                            drop(CString::from_raw(m.uuid));
                        }
                        if !m.role.is_null() {
                            drop(CString::from_raw(m.role));
                        }
                        if !m.content.is_null() {
                            drop(CString::from_raw(m.content));
                        }
                    }
                    return ParseResult {
                        session: std::ptr::null_mut(),
                        error: FfiError::InvalidUtf8,
                    };
                }
            };

            let len = c_messages.len();
            let data = if len > 0 {
                let ptr = c_messages.as_mut_ptr();
                std::mem::forget(c_messages);
                ptr
            } else {
                std::ptr::null_mut()
            };

            let c_session = Box::new(IndexableSessionC {
                session_id,
                project_path,
                project_name,
                messages: IndexableMessageArray { data, len },
            });

            ParseResult {
                session: Box::into_raw(c_session),
                error: FfiError::Success,
            }
        }
        Ok(Ok(None)) => {
            // 文件为空或无有效消息
            ParseResult {
                session: std::ptr::null_mut(),
                error: FfiError::Success,
            }
        }
        Ok(Err(_)) => ParseResult {
            session: std::ptr::null_mut(),
            error: FfiError::DatabaseError,
        },
        Err(_) => ParseResult {
            session: std::ptr::null_mut(),
            error: FfiError::Unknown,
        },
    }
}

/// 释放解析结果
///
/// # Safety
/// `result` 必须是 `session_db_parse_jsonl` 返回的有效指针
#[no_mangle]
pub unsafe extern "C" fn session_db_free_parse_result(session: *mut IndexableSessionC) {
    if session.is_null() {
        return;
    }

    let session = Box::from_raw(session);

    // 释放字符串
    if !session.session_id.is_null() {
        drop(CString::from_raw(session.session_id));
    }
    if !session.project_path.is_null() {
        drop(CString::from_raw(session.project_path));
    }
    if !session.project_name.is_null() {
        drop(CString::from_raw(session.project_name));
    }

    // 释放消息数组
    if !session.messages.data.is_null() && session.messages.len > 0 {
        let messages = Vec::from_raw_parts(
            session.messages.data,
            session.messages.len,
            session.messages.len,
        );
        for m in messages {
            if !m.uuid.is_null() {
                drop(CString::from_raw(m.uuid));
            }
            if !m.role.is_null() {
                drop(CString::from_raw(m.role));
            }
            if !m.content.is_null() {
                drop(CString::from_raw(m.content));
            }
        }
    }
}

// ==================== 路径查询 ====================

/// 获取会话文件路径
///
/// 通过 session_id 查询完整的文件路径。
/// 这是 `encode_path` 的替代方案，从缓存/数据库获取而不是计算。
///
/// # 参数
/// - `projects_path`: Claude projects 目录路径，null 使用默认路径
/// - `session_id`: 会话 ID
///
/// # 返回
/// - 成功：返回完整的 session 文件路径
/// - 失败（未找到）：返回 null
///
/// # Safety
/// - 返回的字符串需要调用 `session_db_free_string` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_get_session_path(
    projects_path: *const c_char,
    session_id: *const c_char,
) -> *mut c_char {
    if session_id.is_null() {
        return std::ptr::null_mut();
    }

    let session_id_str = match CStr::from_ptr(session_id).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    let path = if projects_path.is_null() {
        let home = match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => return std::ptr::null_mut(),
        };
        PathBuf::from(home).join(".claude/projects")
    } else {
        let path_str = match CStr::from_ptr(projects_path).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        PathBuf::from(path_str)
    };

    let reader = SessionReader::new(path);
    match reader.get_session_path(session_id_str) {
        Some(session_path) => match CString::new(session_path) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

/// 获取项目的编码目录名
///
/// 通过 project_path 查询对应的编码目录名。
///
/// # 参数
/// - `projects_path`: Claude projects 目录路径，null 使用默认路径
/// - `project_path`: 项目路径
///
/// # 返回
/// - 成功：返回编码后的目录名
/// - 失败（未找到）：返回 null
///
/// # Safety
/// - 返回的字符串需要调用 `session_db_free_string` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_get_encoded_dir_name(
    projects_path: *const c_char,
    project_path: *const c_char,
) -> *mut c_char {
    if project_path.is_null() {
        return std::ptr::null_mut();
    }

    let project_path_str = match CStr::from_ptr(project_path).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    let path = if projects_path.is_null() {
        let home = match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => return std::ptr::null_mut(),
        };
        PathBuf::from(home).join(".claude/projects")
    } else {
        let path_str = match CStr::from_ptr(projects_path).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        PathBuf::from(path_str)
    };

    let mut reader = SessionReader::new(path);
    match reader.get_encoded_dir_name(project_path_str) {
        Some(encoded) => match CString::new(encoded) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

/// 计算会话文件路径
///
/// 根据 encoded_dir_name 和 session_id 直接计算路径，无需搜索。
/// 路径规则: `{projects_path}/{encoded_dir_name}/{session_id}.jsonl`
///
/// # 参数
/// - `projects_path`: Claude projects 目录路径，null 使用默认路径
/// - `encoded_dir_name`: 项目的编码目录名
/// - `session_id`: 会话 ID
///
/// # 返回
/// - 成功：返回计算出的路径（不检查文件是否存在）
/// - 失败：返回 null
///
/// # Safety
/// - 返回的字符串需要调用 `session_db_free_string` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_compute_session_path(
    projects_path: *const c_char,
    encoded_dir_name: *const c_char,
    session_id: *const c_char,
) -> *mut c_char {
    if encoded_dir_name.is_null() || session_id.is_null() {
        return std::ptr::null_mut();
    }

    let encoded_dir_str = match CStr::from_ptr(encoded_dir_name).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    let session_id_str = match CStr::from_ptr(session_id).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    let path = if projects_path.is_null() {
        let home = match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => return std::ptr::null_mut(),
        };
        PathBuf::from(home).join(".claude/projects")
    } else {
        let path_str = match CStr::from_ptr(projects_path).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        PathBuf::from(path_str)
    };

    let session_path = crate::reader::compute_session_path(&path, encoded_dir_str, session_id_str);
    match CString::new(session_path.to_string_lossy().to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ==================== 项目/会话列表 ====================

/// ProjectInfo C 结构体
#[repr(C)]
pub struct ProjectInfoC {
    pub encoded_name: *mut c_char,
    pub path: *mut c_char,
    pub name: *mut c_char,
    pub session_count: usize,
    pub last_active: u64, // 0 表示无
}

/// ProjectInfo 数组
#[repr(C)]
pub struct ProjectInfoArray {
    pub data: *mut ProjectInfoC,
    pub len: usize,
}

/// 列出所有项目（从文件系统）
///
/// 会话数量不包含 agent session。
///
/// # 参数
/// - `projects_path`: Claude projects 目录路径，null 使用默认路径 (~/.claude/projects)
/// - `limit`: 最大返回数量，0 表示不限制
///
/// # Safety
/// - 返回的数组需要调用 `session_db_free_project_list` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_list_file_projects(
    projects_path: *const c_char,
    limit: u32,
    out_array: *mut *mut ProjectInfoArray,
) -> FfiError {
    if out_array.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        // 获取 projects 目录路径
        let path = if projects_path.is_null() {
            let home = std::env::var("HOME").map_err(|_| FfiError::Unknown)?;
            PathBuf::from(home).join(".claude/projects")
        } else {
            let path_str = CStr::from_ptr(projects_path)
                .to_str()
                .map_err(|_| FfiError::InvalidUtf8)?;
            PathBuf::from(path_str)
        };

        // 使用 SessionReader 统一的业务逻辑
        let mut reader = SessionReader::new(path);
        let limit_opt = if limit > 0 {
            Some(limit as usize)
        } else {
            None
        };
        let projects = reader.list_projects(limit_opt);

        Ok(projects)
    }));

    match result {
        Ok(Ok(projects)) => {
            let mut c_projects: Vec<ProjectInfoC> = Vec::new();
            for p in projects {
                let encoded_name_c = match CString::new(p.encoded_name) {
                    Ok(s) => s.into_raw(),
                    Err(_) => continue,
                };
                let path_c = match CString::new(p.path) {
                    Ok(s) => s.into_raw(),
                    Err(_) => {
                        drop(CString::from_raw(encoded_name_c));
                        continue;
                    }
                };
                let name_c = match CString::new(p.name) {
                    Ok(s) => s.into_raw(),
                    Err(_) => {
                        drop(CString::from_raw(encoded_name_c));
                        drop(CString::from_raw(path_c));
                        continue;
                    }
                };

                c_projects.push(ProjectInfoC {
                    encoded_name: encoded_name_c,
                    path: path_c,
                    name: name_c,
                    session_count: p.session_count,
                    last_active: p.last_active.unwrap_or(0),
                });
            }

            let len = c_projects.len();
            let data = if len > 0 {
                let ptr = c_projects.as_mut_ptr();
                std::mem::forget(c_projects);
                ptr
            } else {
                std::ptr::null_mut()
            };

            let array = Box::new(ProjectInfoArray { data, len });
            *out_array = Box::into_raw(array);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// 释放项目列表
///
/// # Safety
/// - `array` 必须来自 `session_db_list_projects` 返回的数据
/// - 同一指针只能释放一次
#[no_mangle]
pub unsafe extern "C" fn session_db_free_project_list(array: *mut ProjectInfoArray) {
    if array.is_null() {
        return;
    }

    let array = Box::from_raw(array);
    if !array.data.is_null() && array.len > 0 {
        let projects = Vec::from_raw_parts(array.data, array.len, array.len);
        for p in projects {
            if !p.encoded_name.is_null() {
                drop(CString::from_raw(p.encoded_name));
            }
            if !p.path.is_null() {
                drop(CString::from_raw(p.path));
            }
            if !p.name.is_null() {
                drop(CString::from_raw(p.name));
            }
        }
    }
}

/// SessionMetaC 结构体
#[repr(C)]
pub struct SessionMetaC {
    pub id: *mut c_char,
    pub project_path: *mut c_char,
    pub project_name: *mut c_char,
    pub encoded_dir_name: *mut c_char,
    pub session_path: *mut c_char,
    pub file_mtime: i64,    // -1 表示无
    pub message_count: i64, // -1 表示无
}

/// SessionMeta 数组
#[repr(C)]
pub struct SessionMetaArray {
    pub data: *mut SessionMetaC,
    pub len: usize,
}

/// 列出会话
///
/// 默认过滤 agent session (agent-xxx)。
///
/// # 参数
/// - `projects_path`: Claude projects 目录路径，null 使用默认路径
/// - `project_path`: 可选，过滤特定项目的会话
///
/// # Safety
/// - 返回的数组需要调用 `session_db_free_session_meta_list` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_list_session_metas(
    projects_path: *const c_char,
    project_path: *const c_char,
    out_array: *mut *mut SessionMetaArray,
) -> FfiError {
    if out_array.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        // 获取 projects 目录路径
        let path = if projects_path.is_null() {
            let home = std::env::var("HOME").map_err(|_| FfiError::Unknown)?;
            PathBuf::from(home).join(".claude/projects")
        } else {
            let path_str = CStr::from_ptr(projects_path)
                .to_str()
                .map_err(|_| FfiError::InvalidUtf8)?;
            PathBuf::from(path_str)
        };

        let filter_project: Option<&str> = if project_path.is_null() {
            None
        } else {
            Some(
                CStr::from_ptr(project_path)
                    .to_str()
                    .map_err(|_| FfiError::InvalidUtf8)?,
            )
        };

        // 使用 SessionReader 统一的业务逻辑（默认过滤 agent session）
        let mut reader = SessionReader::new(path);
        let sessions = reader.list_sessions(filter_project, false); // include_agents = false

        Ok(sessions)
    }));

    match result {
        Ok(Ok(sessions)) => {
            let mut c_sessions: Vec<SessionMetaC> = Vec::new();
            for s in sessions {
                let id_c = match CString::new(s.id) {
                    Ok(c) => c.into_raw(),
                    Err(_) => continue,
                };
                let project_path_c = match CString::new(s.project_path) {
                    Ok(c) => c.into_raw(),
                    Err(_) => {
                        drop(CString::from_raw(id_c));
                        continue;
                    }
                };
                let project_name_c = match s.project_name {
                    Some(n) => match CString::new(n) {
                        Ok(c) => c.into_raw(),
                        Err(_) => std::ptr::null_mut(),
                    },
                    None => std::ptr::null_mut(),
                };
                let encoded_dir_name_c = match s.encoded_dir_name {
                    Some(n) => match CString::new(n) {
                        Ok(c) => c.into_raw(),
                        Err(_) => std::ptr::null_mut(),
                    },
                    None => std::ptr::null_mut(),
                };
                let session_path_c = match s.session_path {
                    Some(p) => match CString::new(p) {
                        Ok(c) => c.into_raw(),
                        Err(_) => std::ptr::null_mut(),
                    },
                    None => std::ptr::null_mut(),
                };

                c_sessions.push(SessionMetaC {
                    id: id_c,
                    project_path: project_path_c,
                    project_name: project_name_c,
                    encoded_dir_name: encoded_dir_name_c,
                    session_path: session_path_c,
                    file_mtime: s.file_mtime.map(|t| t as i64).unwrap_or(-1),
                    message_count: s.message_count.map(|c| c as i64).unwrap_or(-1),
                });
            }

            let len = c_sessions.len();
            let data = if len > 0 {
                let ptr = c_sessions.as_mut_ptr();
                std::mem::forget(c_sessions);
                ptr
            } else {
                std::ptr::null_mut()
            };

            let array = Box::new(SessionMetaArray { data, len });
            *out_array = Box::into_raw(array);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// 释放会话列表
///
/// # Safety
/// - `array` 必须来自 `session_db_list_sessions` 返回的数据
/// - 同一指针只能释放一次
#[no_mangle]
pub unsafe extern "C" fn session_db_free_session_meta_list(array: *mut SessionMetaArray) {
    if array.is_null() {
        return;
    }

    let array = Box::from_raw(array);
    if !array.data.is_null() && array.len > 0 {
        let sessions = Vec::from_raw_parts(array.data, array.len, array.len);
        for s in sessions {
            if !s.id.is_null() {
                drop(CString::from_raw(s.id));
            }
            if !s.project_path.is_null() {
                drop(CString::from_raw(s.project_path));
            }
            if !s.project_name.is_null() {
                drop(CString::from_raw(s.project_name));
            }
            if !s.encoded_dir_name.is_null() {
                drop(CString::from_raw(s.encoded_dir_name));
            }
            if !s.session_path.is_null() {
                drop(CString::from_raw(s.session_path));
            }
        }
    }
}

/// 查找最新会话
///
/// # 参数
/// - `projects_path`: Claude projects 目录路径，null 使用默认路径
/// - `project_path`: 项目路径
/// - `within_seconds`: 时间范围（秒），0 表示不限制
///
/// # 返回
/// - 成功且找到：返回 SessionMetaC 指针
/// - 成功但未找到：返回 null，error = Success
///
/// # Safety
/// - 返回的指针需要调用 `session_db_free_session_meta` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_find_latest_session(
    projects_path: *const c_char,
    project_path: *const c_char,
    within_seconds: u64,
    out_session: *mut *mut SessionMetaC,
) -> FfiError {
    if project_path.is_null() || out_session.is_null() {
        return FfiError::NullPointer;
    }

    // 先获取会话列表
    let mut session_array: *mut SessionMetaArray = std::ptr::null_mut();
    let err = session_db_list_session_metas(projects_path, project_path, &mut session_array);
    if err != FfiError::Success {
        return err;
    }

    if session_array.is_null() || (*session_array).len == 0 {
        *out_session = std::ptr::null_mut();
        if !session_array.is_null() {
            session_db_free_session_meta_list(session_array);
        }
        return FfiError::Success;
    }

    let array = &*session_array;
    let first = &*array.data;

    // 检查时间范围
    if within_seconds > 0 && first.file_mtime > 0 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let diff_ms = now - first.file_mtime;
        if diff_ms > (within_seconds as i64 * 1000) {
            *out_session = std::ptr::null_mut();
            session_db_free_session_meta_list(session_array);
            return FfiError::Success;
        }
    }

    // 复制第一个会话
    let copy = Box::new(SessionMetaC {
        id: if first.id.is_null() {
            std::ptr::null_mut()
        } else {
            CString::new(CStr::from_ptr(first.id).to_str().unwrap_or(""))
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut())
        },
        project_path: if first.project_path.is_null() {
            std::ptr::null_mut()
        } else {
            CString::new(CStr::from_ptr(first.project_path).to_str().unwrap_or(""))
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut())
        },
        project_name: if first.project_name.is_null() {
            std::ptr::null_mut()
        } else {
            CString::new(CStr::from_ptr(first.project_name).to_str().unwrap_or(""))
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut())
        },
        encoded_dir_name: if first.encoded_dir_name.is_null() {
            std::ptr::null_mut()
        } else {
            CString::new(
                CStr::from_ptr(first.encoded_dir_name)
                    .to_str()
                    .unwrap_or(""),
            )
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut())
        },
        session_path: if first.session_path.is_null() {
            std::ptr::null_mut()
        } else {
            CString::new(CStr::from_ptr(first.session_path).to_str().unwrap_or(""))
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut())
        },
        file_mtime: first.file_mtime,
        message_count: first.message_count,
    });

    *out_session = Box::into_raw(copy);
    session_db_free_session_meta_list(session_array);
    FfiError::Success
}

/// 释放单个 SessionMeta
///
/// # Safety
/// - `session` 必须来自本库返回的 `SessionMetaC` 指针
/// - 同一指针只能释放一次
#[no_mangle]
pub unsafe extern "C" fn session_db_free_session_meta(session: *mut SessionMetaC) {
    if session.is_null() {
        return;
    }

    let s = Box::from_raw(session);
    if !s.id.is_null() {
        drop(CString::from_raw(s.id));
    }
    if !s.project_path.is_null() {
        drop(CString::from_raw(s.project_path));
    }
    if !s.project_name.is_null() {
        drop(CString::from_raw(s.project_name));
    }
    if !s.encoded_dir_name.is_null() {
        drop(CString::from_raw(s.encoded_dir_name));
    }
    if !s.session_path.is_null() {
        drop(CString::from_raw(s.session_path));
    }
}

// ==================== 读取消息 ====================

/// ParsedMessage C 结构体 (用于 read_messages)
#[repr(C)]
pub struct ParsedMessageC {
    pub uuid: *mut c_char,
    pub session_id: *mut c_char,
    pub message_type: i32, // 0 = User, 1 = Assistant
    pub content: *mut c_char,
    pub timestamp: *mut c_char,
}

/// MessagesResult C 结构体
#[repr(C)]
pub struct MessagesResultC {
    pub messages: *mut ParsedMessageC,
    pub message_count: usize,
    pub total: usize,
    pub has_more: bool,
}

/// 读取会话消息（支持分页）
///
/// # 参数
/// - `session_path`: 会话文件完整路径
/// - `limit`: 每页消息数
/// - `offset`: 偏移量
/// - `order_asc`: true 升序，false 降序
///
/// # Safety
/// - 返回的结果需要调用 `session_db_free_messages_result` 释放
#[no_mangle]
pub unsafe extern "C" fn session_db_read_session_messages(
    session_path: *const c_char,
    limit: usize,
    offset: usize,
    order_asc: bool,
    out_result: *mut *mut MessagesResultC,
) -> FfiError {
    if session_path.is_null() || out_result.is_null() {
        return FfiError::NullPointer;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let path_str = CStr::from_ptr(session_path)
            .to_str()
            .map_err(|_| FfiError::InvalidUtf8)?;

        // 提取 session_id
        let session_id = std::path::Path::new(path_str)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        // 构造临时 SessionMeta
        let meta = crate::SessionMeta {
            id: session_id.clone(),
            source: crate::Source::Claude,
            channel: Some("code".to_string()),
            project_path: String::new(),
            project_name: None,
            encoded_dir_name: None,
            session_path: Some(path_str.to_string()),
            file_mtime: None,
            file_size: None,
            message_count: None,
            cwd: None,
            model: None,
            meta: None,
            created_at: None,
            updated_at: None,
        };

        // 使用默认路径创建 adapter
        let home = std::env::var("HOME").map_err(|_| FfiError::Unknown)?;
        let projects_path = PathBuf::from(home).join(".claude/projects");
        let adapter = ClaudeAdapter::with_path(projects_path);

        let parse_result = adapter
            .parse_session(&meta)
            .map_err(|_| FfiError::DatabaseError)?;

        let Some(parse_result) = parse_result else {
            return Ok((Vec::new(), 0));
        };

        let mut all_messages = parse_result.messages;
        let total = all_messages.len();

        // 排序
        if !order_asc {
            all_messages.reverse();
        }

        // 分页（limit=0 表示无限制）
        let start = offset.min(total);
        let actual_limit = if limit == 0 { usize::MAX } else { limit };
        let end = start.saturating_add(actual_limit).min(total);
        let messages: Vec<_> = all_messages[start..end].to_vec();

        Ok((messages, total))
    }));

    match result {
        Ok(Ok((messages, total))) => {
            let mut c_messages: Vec<ParsedMessageC> = Vec::new();
            for m in messages {
                let uuid_c = match CString::new(m.uuid) {
                    Ok(c) => c.into_raw(),
                    Err(_) => continue,
                };
                let session_id_c = match CString::new(m.session_id) {
                    Ok(c) => c.into_raw(),
                    Err(_) => {
                        drop(CString::from_raw(uuid_c));
                        continue;
                    }
                };
                // ParsedMessage.content 是 ParsedContent，FFI 使用 full
                let content_c = match CString::new(m.content.full) {
                    Ok(c) => c.into_raw(),
                    Err(_) => {
                        drop(CString::from_raw(uuid_c));
                        drop(CString::from_raw(session_id_c));
                        continue;
                    }
                };
                let timestamp_c = match m.timestamp {
                    Some(t) => match CString::new(t) {
                        Ok(c) => c.into_raw(),
                        Err(_) => std::ptr::null_mut(),
                    },
                    None => std::ptr::null_mut(),
                };

                let msg_type = match m.message_type {
                    ai_cli_session_collector::MessageType::User => 0,
                    ai_cli_session_collector::MessageType::Assistant => 1,
                    ai_cli_session_collector::MessageType::Tool => 2,
                };

                c_messages.push(ParsedMessageC {
                    uuid: uuid_c,
                    session_id: session_id_c,
                    message_type: msg_type,
                    content: content_c,
                    timestamp: timestamp_c,
                });
            }

            let message_count = c_messages.len();
            let has_more = offset + message_count < total;

            let messages_ptr = if message_count > 0 {
                let ptr = c_messages.as_mut_ptr();
                std::mem::forget(c_messages);
                ptr
            } else {
                std::ptr::null_mut()
            };

            let result_c = Box::new(MessagesResultC {
                messages: messages_ptr,
                message_count,
                total,
                has_more,
            });

            *out_result = Box::into_raw(result_c);
            FfiError::Success
        }
        Ok(Err(e)) => e,
        Err(_) => FfiError::Unknown,
    }
}

/// 释放消息结果
///
/// # Safety
/// - `result` 必须来自 `session_db_read_messages` 返回的数据
/// - 同一指针只能释放一次
#[no_mangle]
pub unsafe extern "C" fn session_db_free_messages_result(result: *mut MessagesResultC) {
    if result.is_null() {
        return;
    }

    let r = Box::from_raw(result);
    if !r.messages.is_null() && r.message_count > 0 {
        let messages = Vec::from_raw_parts(r.messages, r.message_count, r.message_count);
        for m in messages {
            if !m.uuid.is_null() {
                drop(CString::from_raw(m.uuid));
            }
            if !m.session_id.is_null() {
                drop(CString::from_raw(m.session_id));
            }
            if !m.content.is_null() {
                drop(CString::from_raw(m.content));
            }
            if !m.timestamp.is_null() {
                drop(CString::from_raw(m.timestamp));
            }
        }
    }
}

// ==================== Collect API ====================

/// 采集结果（FFI 版本）
#[repr(C)]
pub struct CollectResultC {
    pub projects_scanned: usize,
    pub sessions_scanned: usize,
    pub messages_inserted: usize,
    pub error_count: usize,
    /// 第一个错误信息（如果有）
    pub first_error: *mut c_char,
}

/// 执行全量采集
///
/// 扫描所有 CLI 会话文件（Claude、OpenCode、Codex 等），增量写入数据库。
///
/// # Safety
/// `handle` 必须是有效句柄，`out_result` 必须是有效指针
#[no_mangle]
pub unsafe extern "C" fn session_db_collect(
    handle: *mut SessionDbHandle,
    out_result: *mut *mut CollectResultC,
) -> FfiError {
    if handle.is_null() || out_result.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &*handle;

    // 检查是否为 Writer
    if !handle.db.is_writer() {
        return FfiError::PermissionDenied;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        use crate::collector::Collector;

        let collector = Collector::new(&handle.db);
        collector.collect_all()
    }));

    match result {
        Ok(Ok(collect_result)) => {
            let first_error = if let Some(err) = collect_result.errors.first() {
                CString::new(err.as_str())
                    .map(|s| s.into_raw())
                    .unwrap_or(std::ptr::null_mut())
            } else {
                std::ptr::null_mut()
            };

            let c_result = Box::new(CollectResultC {
                projects_scanned: collect_result.projects_scanned,
                sessions_scanned: collect_result.sessions_scanned,
                messages_inserted: collect_result.messages_inserted,
                error_count: collect_result.errors.len(),
                first_error,
            });
            *out_result = Box::into_raw(c_result);
            FfiError::Success
        }
        Ok(Err(_)) => FfiError::DatabaseError,
        Err(_) => FfiError::Unknown,
    }
}

/// 按路径采集单个会话
///
/// # Safety
/// `handle` 必须是有效句柄，`path` 必须是有效 C 字符串
#[no_mangle]
pub unsafe extern "C" fn session_db_collect_by_path(
    handle: *mut SessionDbHandle,
    path: *const c_char,
    out_result: *mut *mut CollectResultC,
) -> FfiError {
    if handle.is_null() || path.is_null() || out_result.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &*handle;

    // 检查是否为 Writer
    if !handle.db.is_writer() {
        return FfiError::PermissionDenied;
    }

    let path_str = match CStr::from_ptr(path).to_str() {
        Ok(s) => s,
        Err(_) => return FfiError::InvalidUtf8,
    };

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        use crate::collector::Collector;

        let collector = Collector::new(&handle.db);
        collector.collect_by_path(path_str)
    }));

    match result {
        Ok(Ok(collect_result)) => {
            let first_error = if let Some(err) = collect_result.errors.first() {
                CString::new(err.as_str())
                    .map(|s| s.into_raw())
                    .unwrap_or(std::ptr::null_mut())
            } else {
                std::ptr::null_mut()
            };

            let c_result = Box::new(CollectResultC {
                projects_scanned: collect_result.projects_scanned,
                sessions_scanned: collect_result.sessions_scanned,
                messages_inserted: collect_result.messages_inserted,
                error_count: collect_result.errors.len(),
                first_error,
            });
            *out_result = Box::into_raw(c_result);
            FfiError::Success
        }
        Ok(Err(_)) => FfiError::DatabaseError,
        Err(_) => FfiError::Unknown,
    }
}

/// 释放采集结果
///
/// # Safety
/// - `result` 必须来自 `session_db_collect` 返回的数据
/// - 同一指针只能释放一次
#[no_mangle]
pub unsafe extern "C" fn session_db_free_collect_result(result: *mut CollectResultC) {
    if result.is_null() {
        return;
    }

    let r = Box::from_raw(result);
    if !r.first_error.is_null() {
        drop(CString::from_raw(r.first_error));
    }
}
