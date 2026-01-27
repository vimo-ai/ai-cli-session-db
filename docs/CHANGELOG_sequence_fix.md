# Sequence 递增修复

## 问题描述

在增量写入模式下，`scan_session_incremental` 函数使用 `convert_messages` 转换消息时，每批新消息的 sequence 都会从 0 重置，导致多次增量写入后 sequence 不连续。

### 问题根源

`convert_messages` 使用 `enumerate()` 从 0 开始生成 sequence，没有考虑数据库中已存在的消息。

```rust
// 旧代码 - 每次都从 0 开始
pub fn convert_messages(messages: &[...]) -> Vec<MessageInput> {
    messages
        .iter()
        .enumerate()
        .map(|(i, m)| convert_message(m, i as i64))  // ❌ 总是从 0 开始
        .collect()
}
```

### 影响

- 增量写入时，新消息的 sequence 会重置为 0, 1, 2...
- 虽然 UUID 去重保证不会插入重复数据，但 sequence 不正确会影响消息排序和查询

## 修复方案

### 1. 添加获取最大 sequence 的方法

在 `SessionDB` 中添加 `get_session_max_sequence` 方法：

```rust
/// 获取 Session 的最大 sequence
pub fn get_session_max_sequence(&self, session_id: &str) -> Result<Option<i64>> {
    let conn = self.conn.lock();
    conn.query_row(
        "SELECT MAX(sequence) FROM messages WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}
```

### 2. 添加支持起始 sequence 的转换函数

保持 `convert_messages` 向后兼容，添加新函数 `convert_messages_with_start_sequence`：

```rust
/// 批量转换消息（支持指定起始 sequence）
pub fn convert_messages_with_start_sequence(
    messages: &[ai_cli_session_collector::ParsedMessage],
    start_sequence: i64,
) -> Vec<MessageInput> {
    messages
        .iter()
        .enumerate()
        .map(|(i, m)| convert_message(m, start_sequence + i as i64))
        .collect()
}

/// 批量转换消息 (向后兼容，默认从 0 开始)
pub fn convert_messages(messages: &[...]) -> Vec<MessageInput> {
    convert_messages_with_start_sequence(messages, 0)
}
```

### 3. 修改增量扫描逻辑

在 `scan_session_incremental` 中，写入前查询最大 sequence 并重新设置：

```rust
// 获取当前最大 sequence，确保增量写入时 sequence 正确递增
let max_sequence = self.get_session_max_sequence(session_id)?.unwrap_or(-1);
let start_sequence = max_sequence + 1;

// 重新设置 sequence，从 max+1 开始
for (i, msg) in messages_to_process.iter_mut().enumerate() {
    msg.sequence = start_sequence + i as i64;
}
```

## 测试验证

添加了专门的测试用例 `test_incremental_scan_sequence_increment`：

```rust
#[test]
fn test_incremental_scan_sequence_increment() {
    // 第一批：插入 3 条消息 (sequence 0, 1, 2)
    let messages1 = create_messages(0..3);
    db.scan_session_incremental("session-001", project_id, messages1).unwrap();

    // 第二批：增量插入 2 条新消息
    let messages2 = create_messages(3..5);
    db.scan_session_incremental("session-001", project_id, messages2).unwrap();

    // 验证所有消息的 sequence 正确递增 (0, 1, 2, 3, 4)
    let all = db.list_messages("session-001", 100, 0).unwrap();
    assert_eq!(all[3].sequence, 3); // ✅ 应该是 3，而不是 0
    assert_eq!(all[4].sequence, 4); // ✅ 应该是 4，而不是 1
}
```

## 向后兼容性

- ✅ `convert_messages` 保持原有行为（从 0 开始），不影响全量写入场景
- ✅ 新增 `convert_messages_with_start_sequence` 函数，支持自定义起始 sequence
- ✅ 只有 `scan_session_incremental` 使用新逻辑，其他代码路径不受影响
- ✅ 所有现有测试通过

## 测试结果

```bash
$ cargo test incremental_scan --features writer
running 5 tests
test incremental_scan_tests::test_get_session_max_sequence ... ok
test incremental_scan_tests::test_incremental_scan_first_time ... ok
test incremental_scan_tests::test_incremental_scan_safety_margin ... ok
test incremental_scan_tests::test_incremental_scan_sequence_increment ... ok
test incremental_scan_tests::test_incremental_scan_with_checkpoint ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured
```

## 相关文件

- `ai-cli-session-db/src/db.rs` - 添加 `get_session_max_sequence` 方法
- `ai-cli-session-db/src/writer.rs` - 修改 `scan_session_incremental`，添加 `convert_messages_with_start_sequence`
- `ai-cli-session-db/tests/integration_tests.rs` - 添加测试用例
