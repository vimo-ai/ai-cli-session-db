#!/usr/bin/env bash
# ============================================================================
# 更新所有依赖 ai-cli-session-db 的下游项目
#
# 使用方式: ./scripts/update_downstream.sh
#
# 下游项目：
#   - VlaudeKit (FFI dylib)
#   - MemexKit (FFI dylib)
#   - memex-rs (Rust 依赖)
#   - vlaude-core/session-reader (Rust 依赖)
# ============================================================================
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
ETERM_ROOT="$(dirname "$PROJECT_DIR")"

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[ai-cli-session-db]${NC} $*"; }
log_success() { echo -e "${GREEN}[ai-cli-session-db]${NC} $*"; }

# 获取当前 commit hash
cd "$PROJECT_DIR"
LATEST_HASH=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
log_info "Current commit: $LATEST_HASH"

# 调用 ETerm 统一编译脚本
if [ -f "$ETERM_ROOT/scripts/build.sh" ]; then
    log_info "Building FFI and deploying to plugins..."
    "$ETERM_ROOT/scripts/build.sh" ffi

    log_info "Rebuilding memex..."
    "$ETERM_ROOT/scripts/build.sh" memex
else
    log_info "ETerm build script not found, building manually..."

    # 编译 FFI
    log_info "Building FFI..."
    cargo build --release --features ffi,fts,coordination

    # 复制到 VlaudeKit
    VLAUDE_KIT="$ETERM_ROOT/english/Plugins/VlaudeKit/Libs/SharedDB"
    if [ -d "$VLAUDE_KIT" ] || mkdir -p "$VLAUDE_KIT"; then
        cp target/release/libai_cli_session_db.dylib "$VLAUDE_KIT/"
        log_info "Copied to VlaudeKit"
    fi

    # 复制到 MemexKit
    MEMEX_KIT="$ETERM_ROOT/english/Plugins/MemexKit/Libs/SharedDB"
    if [ -d "$MEMEX_KIT" ] || mkdir -p "$MEMEX_KIT"; then
        cp target/release/libai_cli_session_db.dylib "$MEMEX_KIT/"
        log_info "Copied to MemexKit"
    fi

    # 重建 memex-rs
    if [ -d "$ETERM_ROOT/memex/memex-rs" ]; then
        log_info "Rebuilding memex-rs..."
        cd "$ETERM_ROOT/memex/memex-rs"
        cargo build --release --features cli
    fi
fi

echo ""
log_success "Downstream projects updated!"
echo "  ai-cli-session-db: $LATEST_HASH"
echo ""
echo "Next: Run plugin build.sh or rebuild ETerm to load new binaries"
