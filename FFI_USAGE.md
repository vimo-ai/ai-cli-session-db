# Claude Session DB - FFI Usage Guide

## Overview

This library provides a C FFI interface for Swift integration in MemexKit and VlaudeKit.

## Build

```bash
# Build with FFI support
cargo build --release --features ffi,fts,coordination

# Output:
# - Dynamic library: target/release/libai_cli_session_db.dylib (macOS)
# - C header: include/ai_cli_session_db.h
```

## C Header

The header is auto-generated using `cbindgen` during build. Location: `include/ai_cli_session_db.h`

## Swift Integration

### 1. Import Header

Add to your bridging header:

```c
#import "ai_cli_session_db.h"
```

### 2. Basic Usage

```swift
import Foundation

class SessionDBWrapper {
    private var handle: OpaquePointer?

    init?(path: String) {
        var outHandle: OpaquePointer?
        let result = session_db_connect(path, &outHandle)

        guard result == Success, let handle = outHandle else {
            return nil
        }

        self.handle = handle
    }

    deinit {
        if let handle = handle {
            session_db_close(handle)
        }
    }

    // Project operations
    func createProject(name: String, path: String, source: String) -> Int64? {
        var projectId: Int64 = 0
        let result = session_db_upsert_project(handle, name, path, source, &projectId)
        return result == Success ? projectId : nil
    }

    func listProjects() -> [ProjectInfo] {
        var arrayPtr: UnsafeMutablePointer<ProjectArray>?
        let result = session_db_list_projects(handle, &arrayPtr)

        guard result == Success, let array = arrayPtr else {
            return []
        }

        defer { session_db_free_projects(array) }

        let buffer = UnsafeBufferPointer(start: array.pointee.data, count: Int(array.pointee.len))
        return buffer.map { project in
            ProjectInfo(
                id: project.id,
                name: String(cString: project.name),
                path: String(cString: project.path),
                source: String(cString: project.source),
                createdAt: project.created_at,
                updatedAt: project.updated_at
            )
        }
    }

    // Session operations
    func createSession(sessionId: String, projectId: Int64) -> Bool {
        let result = session_db_upsert_session(handle, sessionId, projectId)
        return result == Success
    }

    // Message operations
    func insertMessages(sessionId: String, messages: [MessageInput]) -> Int? {
        let cMessages = messages.map { msg in
            MessageInputC(
                uuid: (msg.uuid as NSString).utf8String,
                role: msg.role == .human ? 0 : 1,
                content: (msg.content as NSString).utf8String,  // FFI 层使用 content，内部映射到 content_text/content_full
                timestamp: msg.timestamp,
                sequence: msg.sequence
            )
        }

        var inserted: Int = 0
        let result = session_db_insert_messages(
            handle,
            sessionId,
            cMessages,
            cMessages.count,
            &inserted
        )

        return result == Success ? inserted : nil
    }

    // Search (FTS5)
    func search(query: String, limit: Int = 50) -> [SearchResult] {
        var arrayPtr: UnsafeMutablePointer<SearchResultArray>?
        let result = session_db_search_fts(handle, query, limit, &arrayPtr)

        guard result == Success, let array = arrayPtr else {
            return []
        }

        defer { session_db_free_search_results(array) }

        let buffer = UnsafeBufferPointer(start: array.pointee.data, count: Int(array.pointee.len))
        return buffer.map { r in
            SearchResult(
                messageId: r.message_id,
                sessionId: String(cString: r.session_id),
                projectId: r.project_id,
                projectName: String(cString: r.project_name),
                snippet: String(cString: r.snippet),
                score: r.score
            )
        }
    }
}

// Helper types
struct ProjectInfo {
    let id: Int64
    let name: String
    let path: String
    let source: String
    let createdAt: Int64
    let updatedAt: Int64
}

struct MessageInput {
    let uuid: String
    let role: MessageRole
    let content: String  // FFI 层使用单一 content，内部映射到 content_text 和 content_full
    let timestamp: Int64
    let sequence: Int64
}

// Note: 数据库内部使用双字段存储:
// - content_text: 纯对话文本（用于向量化）
// - content_full: 完整格式化内容（用于 FTS 搜索）

enum MessageRole {
    case human
    case assistant
}

struct SearchResult {
    let messageId: Int64
    let sessionId: String
    let projectId: Int64
    let projectName: String
    let snippet: String
    let score: Double
}
```

## Memory Management

**CRITICAL**: Always free returned arrays and strings:

- `session_db_free_projects()` - Free project arrays
- `session_db_free_sessions()` - Free session arrays
- `session_db_free_messages()` - Free message arrays
- `session_db_free_search_results()` - Free search result arrays
- `session_db_free_string()` - Free individual strings
- `session_db_close()` - Close database handle

## Error Handling

All functions return `SessionDbError`:

```c
typedef enum SessionDbError {
    Success = 0,
    NullPointer = 1,
    InvalidUtf8 = 2,
    DatabaseError = 3,
    CoordinationError = 4,
    PermissionDenied = 5,
    Unknown = 99,
} SessionDbError;
```

## Thread Safety

- Uses `parking_lot::Mutex` internally (no poisoning)
- All FFI calls use `panic::catch_unwind` for safety
- Safe for multi-threaded Swift/ObjC applications

## Writer Coordination

```swift
// Register as writer
var role: Int32 = 0
let result = session_db_register_writer(handle, 2, &role) // 2 = MemexKit
if result == Success {
    print(role == 0 ? "Writer" : "Reader")
}

// Heartbeat (call periodically)
session_db_heartbeat(handle)

// Release (on app quit)
session_db_release_writer(handle)
```

## Features

Enable via Cargo features:

- `ffi` - C FFI exports
- `fts` - Full-text search
- `coordination` - Writer coordination
- `writer` - Write capabilities
- `reader` - Read-only capabilities

Default features: `writer`, `reader`, `search`, `coordination`

## Testing

```bash
# Run tests
cargo test --all-features

# Check FFI
cargo build --features ffi && ls -lh target/debug/libai_cli_session_db.dylib
```

## Integration with Xcode

1. Add library to "Link Binary With Libraries":
   - `libai_cli_session_db.dylib`

2. Add to "Build Settings" -> "Library Search Paths":
   - `$(PROJECT_DIR)/../ai-cli-session-db/target/release`

3. Add to "Build Settings" -> "Header Search Paths":
   - `$(PROJECT_DIR)/../ai-cli-session-db/include`

4. Import in bridging header:
   ```objective-c
   #import "ai_cli_session_db.h"
   ```

## Notes

- All strings must be UTF-8 encoded
- NULL values represented as -1 for timestamps
- Arrays use opaque `*Array` types
- Search queries auto-escaped for FTS5
