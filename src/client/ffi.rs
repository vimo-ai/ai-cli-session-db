//! AgentClient FFI 接口
//!
//! 为 Swift 层提供 C ABI 接口，用于连接 Agent 和发送 RPC 请求

use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::runtime::Runtime;

use super::{connect_or_start_agent, AgentClient, ClientConfig};
use crate::ffi::{ApprovalStatusC, FfiError};

/// 内部共享状态
struct SharedState {
    client: Mutex<Option<AgentClient>>,
    connected: AtomicBool,
}

/// 不透明句柄
pub struct AgentClientHandle {
    config: ClientConfig,
    runtime: Runtime,
    state: Arc<SharedState>,
}

/// 创建 AgentClient 句柄
///
/// # Safety
/// - `component` 必须是有效的 UTF-8 C 字符串
/// - `data_dir` 可为 null（使用默认 ~/.vimo）
/// - `agent_source_dir` 可为 null（Agent 源目录，用于首次部署）
/// - `out_handle` 不能为 null
#[no_mangle]
pub unsafe extern "C" fn agent_client_create(
    component: *const c_char,
    data_dir: *const c_char,
    agent_source_dir: *const c_char,
    out_handle: *mut *mut AgentClientHandle,
) -> FfiError {
    if component.is_null() || out_handle.is_null() {
        return FfiError::NullPointer;
    }

    let component = match CStr::from_ptr(component).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return FfiError::InvalidUtf8,
    };

    let data_dir = if data_dir.is_null() {
        None
    } else {
        match CStr::from_ptr(data_dir).to_str() {
            Ok(s) => Some(PathBuf::from(s)),
            Err(_) => return FfiError::InvalidUtf8,
        }
    };

    let agent_source_dir = if agent_source_dir.is_null() {
        None
    } else {
        match CStr::from_ptr(agent_source_dir).to_str() {
            Ok(s) => Some(PathBuf::from(s)),
            Err(_) => return FfiError::InvalidUtf8,
        }
    };

    // 创建配置
    let mut config = ClientConfig::new(&component);
    if let Some(dir) = data_dir {
        config.data_dir = dir;
    }
    if let Some(dir) = agent_source_dir {
        config.agent_source_dir = Some(dir);
    }

    // 创建 tokio runtime
    let runtime = match Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return FfiError::RuntimeError,
    };

    let state = Arc::new(SharedState {
        client: Mutex::new(None),
        connected: AtomicBool::new(false),
    });

    let handle = Box::new(AgentClientHandle {
        config,
        runtime,
        state,
    });

    *out_handle = Box::into_raw(handle);
    FfiError::Success
}

/// 销毁 AgentClient 句柄
///
/// # Safety
/// - `handle` 必须是 `agent_client_create` 返回的有效句柄
#[no_mangle]
pub unsafe extern "C" fn agent_client_destroy(handle: *mut AgentClientHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// 连接到 Agent
///
/// 如果 Agent 未运行，会自动启动
///
/// # Safety
/// - `handle` 必须是有效句柄
#[no_mangle]
pub unsafe extern "C" fn agent_client_connect(handle: *mut AgentClientHandle) -> FfiError {
    if handle.is_null() {
        return FfiError::NullPointer;
    }

    let handle = &*handle;

    if handle.state.connected.load(Ordering::SeqCst) {
        return FfiError::Success;
    }

    // 连接 Agent
    let config = handle.config.clone();
    let client = match handle.runtime.block_on(connect_or_start_agent(config)) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to connect to Agent: {}", e);
            if e.to_string().contains("Agent binary not found") {
                return FfiError::AgentNotFound;
            }
            return FfiError::ConnectionFailed;
        }
    };

    *handle.state.client.lock() = Some(client);
    handle.state.connected.store(true, Ordering::SeqCst);

    FfiError::Success
}

/// 通知文件变化
///
/// # Safety
/// - `handle` 必须是有效句柄
/// - `path` 必须是有效的 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn agent_client_notify_file_change(
    handle: *mut AgentClientHandle,
    path: *const c_char,
) -> FfiError {
    if handle.is_null() || path.is_null() {
        return FfiError::NullPointer;
    }

    let path = match CStr::from_ptr(path).to_str() {
        Ok(s) => PathBuf::from(s),
        Err(_) => return FfiError::InvalidUtf8,
    };

    let handle = &*handle;

    if !handle.state.connected.load(Ordering::SeqCst) {
        return FfiError::NotConnected;
    }

    let mut client_guard = handle.state.client.lock();
    let client = match client_guard.as_mut() {
        Some(c) => c,
        None => return FfiError::NotConnected,
    };

    match handle.runtime.block_on(client.notify_file_change(path)) {
        Ok(_) => FfiError::Success,
        Err(e) => {
            tracing::error!("Failed to notify file change: {}", e);
            FfiError::RequestFailed
        }
    }
}

/// 写入审批结果
///
/// # Safety
/// - `handle` 必须是有效句柄
/// - `tool_call_id` 必须是有效的 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn agent_client_write_approve_result(
    handle: *mut AgentClientHandle,
    tool_call_id: *const c_char,
    status: ApprovalStatusC,
    resolved_at: i64,
) -> FfiError {
    if handle.is_null() || tool_call_id.is_null() {
        return FfiError::NullPointer;
    }

    let tool_call_id = match CStr::from_ptr(tool_call_id).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return FfiError::InvalidUtf8,
    };

    let handle = &*handle;

    if !handle.state.connected.load(Ordering::SeqCst) {
        return FfiError::NotConnected;
    }

    let mut client_guard = handle.state.client.lock();
    let client = match client_guard.as_mut() {
        Some(c) => c,
        None => return FfiError::NotConnected,
    };

    match handle.runtime.block_on(client.write_approve_result(tool_call_id, status.into(), resolved_at)) {
        Ok(_) => FfiError::Success,
        Err(e) => {
            tracing::error!("Failed to write approval result: {}", e);
            FfiError::RequestFailed
        }
    }
}

/// 检查是否已连接
///
/// # Safety
/// - `handle` 必须是有效句柄
#[no_mangle]
pub unsafe extern "C" fn agent_client_is_connected(handle: *const AgentClientHandle) -> bool {
    if handle.is_null() {
        return false;
    }

    let handle = &*handle;
    handle.state.connected.load(Ordering::SeqCst)
}

/// 断开连接
///
/// # Safety
/// - `handle` 必须是有效句柄
#[no_mangle]
pub unsafe extern "C" fn agent_client_disconnect(handle: *mut AgentClientHandle) {
    if handle.is_null() {
        return;
    }

    let handle = &*handle;

    // 清理连接状态
    *handle.state.client.lock() = None;
    handle.state.connected.store(false, Ordering::SeqCst);
}

/// 获取版本号
///
/// # Safety
/// 返回静态字符串，无需释放
#[no_mangle]
pub extern "C" fn agent_client_version() -> *const c_char {
    static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    VERSION.as_ptr() as *const c_char
}
