# ai-cli-session-db

Shared database library for Claude session management with Agent-based architecture.

## Features

- **Agent Architecture**: Single vimo-agent handles all writes, eliminating conflicts
- **AgentClient**: Components communicate via Unix socket to vimo-agent
- **FTS5 Search**: Full-text search support
- **Incremental Scan**: Efficient updates based on timestamps
- **UUID Deduplication**: Messages are deduplicated by UUID

## Feature Flags

```toml
[features]
default = ["writer", "reader", "search"]
writer = []           # Write capability
reader = []           # Read-only capability
search = ["fts"]      # Search capability
fts = []              # FTS5 support
ffi = []              # C FFI export (for Swift bindings)
agent = [...]         # Agent mode (file watching, event pushing)
client = []           # Agent client (for components)
```

## Architecture

```
┌─────────────┐  ┌─────────────┐  ┌─────────────┐
│  MemexKit   │  │  VlaudeKit  │  │   Memex     │
│  (Plugin)   │  │  (Plugin)   │  │  (Daemon)   │
└──────┬──────┘  └──────┬──────┘  └──────┬──────┘
       │                │                │
       │   AgentClient  │   AgentClient  │   AgentClient
       │                │                │
       └────────────────┼────────────────┘
                        │
                ┌───────▼───────┐
                │  vimo-agent   │  ← Single Writer
                │  (Unix Socket)│
                └───────┬───────┘
                        │
                ┌───────▼───────┐
                │  sessions.db  │
                └───────────────┘
```

## Usage

### Agent (Writer)

```bash
# Start vimo-agent
./vimo-agent
```

### Client (Swift/Rust Components)

```rust
use ai_cli_session_db::{AgentClient, ClientConfig, connect_or_start_agent};

// Connect to agent (auto-starts if not running)
let client = connect_or_start_agent(ClientConfig::default()).await?;

// Notify file change
client.notify_file_change("/path/to/session.jsonl").await?;

// Write approval result
client.write_approve_result("tool-call-id", ApprovalStatus::Approved, timestamp).await?;
```

## Test

```bash
cargo test
```

## License

MIT
