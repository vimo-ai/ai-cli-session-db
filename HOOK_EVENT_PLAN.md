# HookEvent 事件中心架构实施计划

## 背景

让 vimo-agent 成为事件中心，支持 memex 独立运行时的即时 collection。

```
claude_hook.sh → vimo-agent (事件中心) → 广播给订阅者
                                        ├── ETerm (AICliKit)
                                        ├── memex
                                        └── vlaude
```

## 事件分层

| 层级 | 类型 | 特点 | 示例 |
|------|------|------|------|
| L1 | NewMessages | 持久化，触发索引 | 新消息写入数据库 |
| L2 | HookEvent | 瞬时通知，UI 反馈 | SessionStart, Stop, PermissionRequest |

## Phase 1: 协议扩展 + 双写

### ai-cli-session-db ✅ 已完成
- [x] `protocol.rs`: HookEvent struct, Request/Push/Event/EventType 扩展
- [x] `handler.rs`: handle_hook_event (触发 collection + 广播)
- [x] `client/ffi.rs`: AgentEventType::HookEvent = 3
- [x] `ai_cli_session_db.h`: C header 导出
- [x] 测试用例: 7 个协议测试 + 4 个集成测试

### ETerm (claude_hook.sh) ✅ 已完成
- [x] 双写架构: 同时通知 vimo-agent 和 ETerm Socket
- [x] build_agent_hook_event(): jq 安全构造 JSON
- [x] 支持所有事件类型: SessionStart, UserPromptSubmit, SessionEnd, Stop, PermissionRequest, Notification

### Phase 1.1: context 字段扩展 ✅ 已完成

#### 问题分析：terminal_id 链路断裂

当前 ETerm Socket 直连方案：
```
ETerm 启动终端
    │
    ├─ env::set_var("ETERM_TERMINAL_ID", terminal_id)  // terminal_pool.rs
    │
    └─ spawn shell (子进程继承环境变量)
          └─ 用户运行 claude
                └─ hook 触发
                      └─ claude_hook.sh 读取 $ETERM_TERMINAL_ID
                            └─ 发送到 ETerm Socket (带 terminal_id) ✅
```

走 vimo-agent 的问题：
```
claude_hook.sh
    │
    ├─ 读取 $ETERM_TERMINAL_ID = 123  ← 能读到
    │
    └─ 发送 HookEvent 到 vimo-agent
          │
          │  HookEvent { session_id, ... }  ← 没有 terminal_id 字段！
          │
          └─ 广播给 AICliKit  ← 不知道该更新哪个 Tab
```

**核心问题**：HookEvent 协议没有 terminal_id 字段，信息在 vimo-agent 这一跳丢失。

#### 设计决策：context 扩展字段

**不采用**：直接在 HookEvent 加 `terminal_id: Option<i32>`
- 原因：terminal_id 是 ETerm 特有概念，不应侵入 vimo-agent 核心协议

**采用**：增加通用 `context` 字段，消费者自定义数据
- vimo-agent 只透传，不解析
- ETerm 在 context 里放 terminal_id
- 其他消费者可放自己的数据

#### 协议变更

```rust
pub struct HookEvent {
    pub event_type: String,
    pub session_id: String,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub prompt: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub tool_use_id: Option<String>,
    pub notification_type: Option<String>,
    pub message: Option<String>,

    /// 事件上下文（来源特定数据，vimo-agent 透传不解析）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,  // 新增
}
```

#### 实施任务

**ai-cli-session-db**:
- [x] `protocol.rs`: HookEvent 增加 context 字段
- [x] 更新测试用例（新增 3 个 context 相关测试）

**claude_hook.sh**:
- [x] `build_agent_hook_event()`: 构造 context，ETerm 环境下带 terminal_id

```bash
# ETerm 环境下构造 context
if [ -n "$ETERM_TERMINAL_ID" ]; then
    context="{\"terminal_id\": $ETERM_TERMINAL_ID}"
else
    context="null"
fi
```

**示例 JSON**:
```json
{
  "type": "HookEvent",
  "event_type": "SessionStart",
  "session_id": "abc-123",
  "transcript_path": "/path/to/file.jsonl",
  "context": {
    "terminal_id": 5
  }
}
```

## Phase 2: AICliKit 订阅 vimo-agent ✅ 已完成

### 目标
AICliKit 连接 vimo-agent，订阅 HookEvent，替代当前的 ClaudeSocketServer。

### 实施内容

1. **AICliKit 引入 AgentClientBridge** ✅
   - 新增 `AgentClientBridge.swift`（380+ 行）
   - 支持 HookEvent 事件类型订阅
   - 从 FFI 回调解码 AgentHookEvent

2. **ClaudeProvider 改造** ✅
   - 使用 AgentClientBridge 订阅 HookEvent
   - 从 context 提取 terminal_id
   - 映射事件类型（PascalCase → AICliEventType）

3. **FFI 依赖配置** ✅
   - `Libs/SharedDB/`: header, modulemap, dylib
   - `Package.swift`: SharedDbFFI 依赖

### 事件类型映射

| vimo-agent (PascalCase) | AICliEventType |
|-------------------------|----------------|
| SessionStart | .sessionStart |
| UserPromptSubmit | .userInput |
| Stop | .responseComplete |
| SessionEnd | .sessionEnd |
| Notification | .waitingInput |
| PermissionRequest | .permissionRequest |

## Phase 3: 清理旧路径 ✅ 已完成

### 目标
移除 ETerm Socket 相关代码，简化架构。

### 实施内容

**claude_hook.sh** ✅:
- [x] 移除 `notify_eterm()` 函数
- [x] 移除 ETerm Socket 双写逻辑
- [x] 保留 vimo-agent 通知作为唯一路径

**ETermApp.swift** ✅:
- [x] 移除 `ClaudeSocketServer.shared.start()` 调用
- [x] 移除 `ClaudeSocketServer.shared.stop()` 调用

**ClaudeSocketServer** ✅:
- 已移至 `Claude_Deprecated/` 文件夹
- 不再被 ETermApp 调用
- 保留代码供参考，后续可删除

**环境变量**:
- `ETERM_SOCKET_DIR` - 不再需要（ETerm Socket 已废弃）
- `ETERM_TERMINAL_ID` - **保留**（仍需注入到 shell，供 hook 读取放入 context）

## 设计决策记录

### 2025-01-27: context 扩展字段

**问题**：HookEvent 需要携带 terminal_id，但这是 ETerm 特有概念

**方案对比**：
| 方案 | 优点 | 缺点 |
|------|------|------|
| A: HookEvent 加 terminal_id 字段 | 直接可用 | 协议耦合 ETerm 概念 |
| B: AICliKit 通过 session_id 反查 | 协议通用 | 首次 SessionStart 无法关联 |
| **C: 通用 context 字段** | 协议通用，可扩展 | 需要消费者解析 |

**决策**：采用方案 C，增加 `context: Option<serde_json::Value>` 字段
- vimo-agent 保持通用，只透传不解析
- ETerm 在 context 放 `{"terminal_id": 123}`
- 其他消费者可放自己的业务数据

## 注意事项

### Codex CR 发现的问题（待修复）
1. ~~ETerm JSON 字段未转义~~ → 已用 jq 安全构造
2. ~~terminal_id 非 ETerm 环境为空字符串~~ → 改用 context，null 处理
3. 建议在 Phase 3 清理时验证

### 测试验证
```bash
# 运行测试
cd ai-cli-session-db-hook-event
cargo test --features agent

# 验证 hook 脚本（需要 jq）
echo '{"session_id":"test","hook_event_name":"SessionStart"}' | bash ETerm-hook-event/ETerm/ETerm/Resources/Hooks/claude_hook.sh
```

## 相关文件路径

```
ai-cli-session-db-hook-event/
├── src/protocol.rs          # HookEvent 定义
├── src/agent/handler.rs     # 事件处理
├── src/client/ffi.rs        # FFI 层
├── include/ai_cli_session_db.h
└── tests/agent_tests.rs

ETerm-hook-event/
├── ETerm/ETerm/Resources/Hooks/claude_hook.sh  # 双写脚本
└── Plugins/AICliKit/Sources/AICliKit/
    ├── ClaudeProvider.swift   # Phase 2 改造目标
    └── AICliKitPlugin.swift
```
