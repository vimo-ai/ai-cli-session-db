#!/usr/bin/env bash
# ============================================================================
# ai-cli-session-db 编译脚本
#
# 编译 FFI dylib 并部署到 Swift 插件目录
#
# 使用方式:
#   ./build.sh              # 编译并部署
#   ./build.sh --no-deploy  # 只编译不部署
# ============================================================================
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_NAME="ai-cli-session-db"

# 尝试查找 ETerm 目录（支持独立编译和作为子模块）
if [ -d "$SCRIPT_DIR/../ETerm/Plugins" ]; then
    ETERM_DIR="$SCRIPT_DIR/../ETerm"
elif [ -d "$SCRIPT_DIR/../../ETerm/Plugins" ]; then
    ETERM_DIR="$SCRIPT_DIR/../../ETerm"
else
    ETERM_DIR=""
fi

DEPLOY=true
if [ "$1" = "--no-deploy" ]; then
    DEPLOY=false
fi

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[$PROJECT_NAME]${NC} $*"; }
log_success() { echo -e "${GREEN}[$PROJECT_NAME]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[$PROJECT_NAME]${NC} $*"; }
log_error() { echo -e "${RED}[$PROJECT_NAME]${NC} $*"; }

# 编译（指定 target-dir 避免被 workspace 覆盖）
log_info "Building FFI dylib..."
cd "$SCRIPT_DIR"

# 强制触发重新编译，避免 Cargo 增量编译缓存导致修改未生效
touch "$SCRIPT_DIR/src/lib.rs"

cargo build --release --features ffi,fts,client --target-dir "$SCRIPT_DIR/target"

DYLIB="$SCRIPT_DIR/target/release/libai_cli_session_db.dylib"
HEADER="$SCRIPT_DIR/include/ai_cli_session_db.h"

if [ ! -f "$DYLIB" ]; then
    log_error "FFI dylib not found: $DYLIB"
    exit 1
fi

log_success "Built: $DYLIB"

# 部署
if [ "$DEPLOY" = true ] && [ -n "$ETERM_DIR" ]; then
    VLAUDE_KIT="$ETERM_DIR/Plugins/VlaudeKit"
    MEMEX_KIT="$ETERM_DIR/Plugins/MemexKit"
    AICLI_KIT="$ETERM_DIR/Plugins/AICliKit"

    log_info "Deploying to VlaudeKit..."
    mkdir -p "$VLAUDE_KIT/Libs/SharedDB"
    cp "$DYLIB" "$VLAUDE_KIT/Libs/SharedDB/"
    [ -f "$HEADER" ] && cp "$HEADER" "$VLAUDE_KIT/Libs/SharedDB/"

    log_info "Deploying to MemexKit..."
    mkdir -p "$MEMEX_KIT/Libs/SharedDB"
    cp "$DYLIB" "$MEMEX_KIT/Libs/SharedDB/"
    [ -f "$HEADER" ] && cp "$HEADER" "$MEMEX_KIT/Libs/SharedDB/"

    log_info "Deploying to AICliKit..."
    mkdir -p "$AICLI_KIT/Libs/SharedDB"
    cp "$DYLIB" "$AICLI_KIT/Libs/SharedDB/"
    [ -f "$HEADER" ] && cp "$HEADER" "$AICLI_KIT/Libs/SharedDB/"

    log_success "Deployed to Swift plugins"
elif [ "$DEPLOY" = true ]; then
    log_warn "ETerm directory not found, skipping deploy"
fi

log_success "Done"
