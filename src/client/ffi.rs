//! AgentClient FFI 接口
//!
//! 为 Swift 层提供 C ABI 接口，用于连接 Agent 并订阅事件

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use super::{connect_or_start_agent, AgentClient, ClientConfig};
use crate::ffi::{ApprovalStatusC, FfiError};
use crate::protocol::{EventType, Push};

/// 事件类型（FFI 友好）
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEventType {
    NewMessage = 0,
    SessionStart = 1,
    SessionEnd = 2,
}

impl From<EventType> for AgentEventType {
    fn from(e: EventType) -> Self {
        match e {
            EventType::NewMessage => AgentEventType::NewMessage,
            EventType::SessionStart => AgentEventType::SessionStart,
            EventType::SessionEnd => AgentEventType::SessionEnd,
        }
    }
}

impl From<AgentEventType> for EventType {
    fn from(e: AgentEventType) -> Self {
        match e {
            AgentEventType::NewMessage => EventType::NewMessage,
            AgentEventType::SessionStart => EventType::SessionStart,
            AgentEventType::SessionEnd => EventType::SessionEnd,
        }
    }
}


/// 推送回调函数类型
///
/// - `event_type`: 事件类型
/// - `data_json`: 事件数据（JSON 格式）
/// - `user_data`: 用户数据指针
pub type AgentPushCallback =
    Option<unsafe extern "C" fn(event_type: AgentEventType, data_json: *const c_char, user_data: *mut std::ffi::c_void)>;

/// 回调信息（用于跨线程传递）
struct CallbackInfo {
    callback: AgentPushCallback,
    user_data: *mut std::ffi::c_void,
}

// 手动实现 Send + Sync（user_data 由调用方保证线程安全）
unsafe impl Send for CallbackInfo {}
unsafe impl Sync for CallbackInfo {}

/// 内部共享状态
struct SharedState {
    client: Mutex<Option<AgentClient>>,
    connected: AtomicBool,
    callback: Mutex<Option<CallbackInfo>>,
    shutdown_tx: Mutex<Option<mpsc::Sender<()>>>,
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
        callback: Mutex::new(None),
        shutdown_tx: Mutex::new(None),
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
        // 先发送 shutdown 信号
        {
            let handle_ref = &*handle;
            if let Some(tx) = handle_ref.state.shutdown_tx.lock().take() {
                let _ = tx.blocking_send(());
            }
        }
        // 然后释放 handle
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

    // 启动事件循环
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
    *handle.state.shutdown_tx.lock() = Some(shutdown_tx);

    // 创建事件接收通道
    let (event_tx, mut event_rx) = mpsc::channel::<String>(100);

    // 从 client 的 push_receiver 转发到 event_tx
    let state = Arc::clone(&handle.state);
    handle.runtime.spawn(async move {
        let mut shutdown_rx = shutdown_rx;

        loop {
            // 获取一条消息
            let line = {
                let mut client_guard = state.client.lock();
                let client = match client_guard.as_mut() {
                    Some(c) => c,
                    None => break,
                };

                // 使用 try_recv 避免持有锁
                client.push_receiver().try_recv().ok()
            };

            if let Some(line) = line {
                if event_tx.send(line).await.is_err() {
                    break;
                }
            } else {
                // 没有消息，检查 shutdown 或短暂休眠
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("Received shutdown signal, exiting event loop");
                        break;
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(10)) => {
                        // 继续轮询
                    }
                }
            }
        }

        state.connected.store(false, Ordering::SeqCst);
    });

    // 处理事件并调用回调
    let state = Arc::clone(&handle.state);
    handle.runtime.spawn(async move {
        while let Some(line) = event_rx.recv().await {
            // 解析 Push
            let push: Push = match serde_json::from_str(&line) {
                Ok(p) => p,
                Err(_) => continue, // 可能是 Response，跳过
            };

            // 构造事件数据
            let (event_type, data_json) = match &push {
                Push::NewMessages { session_id, path, count, message_ids } => {
                    let json = serde_json::json!({
                        "session_id": session_id,
                        "path": path,
                        "count": count,
                        "message_ids": message_ids,
                    });
                    (AgentEventType::NewMessage, json.to_string())
                }
                Push::SessionStart { session_id, project_path } => {
                    let json = serde_json::json!({
                        "session_id": session_id,
                        "project_path": project_path,
                    });
                    (AgentEventType::SessionStart, json.to_string())
                }
                Push::SessionEnd { session_id } => {
                    let json = serde_json::json!({
                        "session_id": session_id,
                    });
                    (AgentEventType::SessionEnd, json.to_string())
                }
            };

            // 获取回调并调用
            let callback_info = state.callback.lock().as_ref().map(|c| (c.callback, c.user_data));

            if let Some((Some(callback), user_data)) = callback_info {
                if let Ok(c_str) = CString::new(data_json) {
                    unsafe {
                        callback(event_type, c_str.as_ptr(), user_data);
                    }
                }
            }
        }
    });

    FfiError::Success
}

/// 订阅事件
///
/// # Safety
/// - `handle` 必须是有效句柄
/// - `events` 和 `events_count` 必须有效
#[no_mangle]
pub unsafe extern "C" fn agent_client_subscribe(
    handle: *mut AgentClientHandle,
    events: *const AgentEventType,
    events_count: usize,
) -> FfiError {
    if handle.is_null() {
        return FfiError::NullPointer;
    }

    if events.is_null() && events_count > 0 {
        return FfiError::NullPointer;
    }

    let handle = &*handle;

    if !handle.state.connected.load(Ordering::SeqCst) {
        return FfiError::NotConnected;
    }

    // 转换事件类型
    let event_types: Vec<EventType> = if events_count == 0 {
        vec![]
    } else {
        let slice = std::slice::from_raw_parts(events, events_count);
        slice.iter().map(|e| (*e).into()).collect()
    };

    // 发送订阅请求
    let mut client_guard = handle.state.client.lock();
    let client = match client_guard.as_mut() {
        Some(c) => c,
        None => return FfiError::NotConnected,
    };

    match handle.runtime.block_on(client.subscribe(event_types)) {
        Ok(_) => FfiError::Success,
        Err(e) => {
            tracing::error!("Subscription failed: {}", e);
            FfiError::RequestFailed
        }
    }
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

/// 设置推送回调
///
/// # Safety
/// - `handle` 必须是有效句柄
/// - `callback` 和 `user_data` 在 handle 生命周期内必须有效
#[no_mangle]
pub unsafe extern "C" fn agent_client_set_push_callback(
    handle: *mut AgentClientHandle,
    callback: AgentPushCallback,
    user_data: *mut std::ffi::c_void,
) {
    if handle.is_null() {
        return;
    }

    let handle = &*handle;
    *handle.state.callback.lock() = Some(CallbackInfo { callback, user_data });
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

    // 1. 先清除回调，防止在 shutdown 过程中调用已释放的 user_data
    *handle.state.callback.lock() = None;

    // 2. 发送 shutdown 信号
    if let Some(tx) = handle.state.shutdown_tx.lock().take() {
        let _ = tx.blocking_send(());
    }

    // 3. 清理连接状态
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
