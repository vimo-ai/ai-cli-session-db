# ai-cli-session-db

Shared database library for Claude session management with writer coordination.

## Features

- **Writer Coordination**: Multiple components can coexist, only one writes at a time
- **Atomic Preemption**: Higher priority components can take over writer role
- **Heartbeat Monitoring**: Automatic failover when writer crashes
- **FTS5 Search**: Full-text search support
- **Incremental Scan**: Efficient updates based on timestamps
- **UUID Deduplication**: Messages are deduplicated by UUID

## Feature Flags

```toml
[features]
default = ["writer", "reader", "search", "coordination"]
writer = []           # Write capability
reader = []           # Read-only capability
search = ["fts"]      # Search capability
fts = []              # FTS5 support
coordination = []     # Writer coordination logic
ffi = []              # C FFI export (for Swift bindings)
```

## Usage

```rust
use ai_cli_session_db::{SessionDB, DbConfig, WriterType, Role};

// Connect to database
let config = DbConfig::local("~/.memex/session.db");
let mut db = SessionDB::connect(config)?;

// Register as writer
let role = db.register_writer(WriterType::MemexDaemon)?;

match role {
    Role::Writer => {
        // Scan JSONL files, write to database
        db.heartbeat()?;  // Call periodically (every 10s)
    }
    Role::Reader => {
        // Read-only mode, monitor writer health
        let health = db.check_writer_health()?;
    }
}

// On exit
db.release_writer()?;
```

## Writer Priority

| Component | Priority | Description |
|-----------|----------|-------------|
| MemexKit | 2 | ETerm plugin, event-driven |
| VlaudeKit | 2 | ETerm plugin, event-driven |
| MemexDaemon | 1 | File scanning daemon |
| VlaudeDaemon | 1 | File scanning daemon |

Higher priority can preempt lower priority. Same priority: first come, first served.

## Coordination Parameters

| Parameter | Value | Description |
|-----------|-------|-------------|
| Heartbeat interval | 10s | Writer updates heartbeat every 10s |
| Timeout threshold | 30s | Consider writer dead if no heartbeat for 30s |
| Confirm count | 3 | Need 3 consecutive timeouts to confirm |
| Fastest takeover | ~0s | When writer releases normally |
| Slowest takeover | ~50s | When writer crashes |

## Test

```bash
cargo test
```

42 tests covering all core functionality.

## License

MIT
