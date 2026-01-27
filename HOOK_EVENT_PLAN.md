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

## Phase 1: 协议扩展 + 双写 ✅ 已完成

### ai-cli-session-db
- [x] `protocol.rs`: HookEvent struct, Request/Push/Event/EventType 扩展
- [x] `handler.rs`: handle_hook_event (触发 collection + 广播)
- [x] `client/ffi.rs`: AgentEventType::HookEvent = 3
- [x] `ai_cli_session_db.h`: C header 导出
- [x] 测试用例: 7 个协议测试 + 4 个集成测试

### ETerm (claude_hook.sh)
- [x] 双写架构: 同时通知 vimo-agent 和 ETerm Socket
- [x] build_agent_hook_event(): jq 安全构造 JSON
- [x] 支持所有事件类型: SessionStart, UserPromptSubmit, SessionEnd, Stop, PermissionRequest, Notification

## Phase 2: AICliKit 订阅 vimo-agent 🔜 待实施

### 目标
AICliKit 连接 vimo-agent，订阅 HookEvent，替代当前的 ClaudeSocketServer。

### 涉及文件
- `ETerm/Plugins/AICliKit/Sources/AICliKit/ClaudeProvider.swift`
- `ETerm/Plugins/AICliKit/Sources/AICliKit/AICliKitPlugin.swift`

### 实施步骤
1. AICliKit 内嵌 vimo-agent（参考 MemexKit/VlaudeKit 的 agent 下载方案）
2. 使用 `agent_client_*` FFI 接口连接 agent
3. 调用 `agent_client_subscribe([HookEvent])` 订阅事件
4. 在回调中处理 HookEvent，更新 Tab 装饰等 UI
5. 移除或保留 ClaudeSocketServer 作为备用

### FFI 接口（已就绪）
```c
// 创建客户端
FfiError agent_client_create(component, data_dir, agent_source_dir, &handle);

// 连接（自动启动 agent）
FfiError agent_client_connect(handle);

// 订阅事件
AgentEventType events[] = { HookEvent };
FfiError agent_client_subscribe(handle, events, 1);

// 设置回调
agent_client_set_push_callback(handle, callback, user_data);
```

## Phase 3: 清理旧路径 🔜 待实施

### 目标
移除 ETerm Socket 相关代码，简化架构。

### 涉及改动
1. `claude_hook.sh`: 移除 notify_eterm() 和 ETerm Socket 通知
2. `ClaudeProvider.swift`: 移除 ClaudeSocketServer
3. 移除 `ETERM_TERMINAL_ID` / `ETERM_SOCKET_DIR` 环境变量依赖

### 前提条件
- Phase 2 完成并验证稳定
- 确认所有 UI 功能通过 vimo-agent 事件正常工作

## 注意事项

### Codex CR 发现的问题（待修复）
1. ETerm JSON 字段未转义（session_id 等可能包含特殊字符）
2. terminal_id 作为原始值插入（非 ETerm 环境会变成空字符串）
3. 建议在 Phase 3 清理时一并修复

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
