#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

echo "Building tmux-pane-border..."

if ! command -v cargo &>/dev/null; then
    echo "Error: Rust/Cargo not found. Install from https://rustup.rs/"
    exit 1
fi

cd "$PROJECT_DIR"
cargo build --release

BINARY="$PROJECT_DIR/target/release/tmux-pane-border"

if [ ! -f "$BINARY" ]; then
    echo "Error: Build failed"
    exit 1
fi

echo "Build successful: $BINARY"
echo ""
echo "To use with TPM, add to your ~/.tmux.conf:"
echo "  set -g @plugin 'path/to/tmux-pane-border'"
echo ""
echo "Or to use manually, add to ~/.tmux.conf:"
echo "  set -g default-command '$BINARY -- \$SHELL'"
echo "  set-hook -g pane-focus-in \"run-shell 'pid=\\\$(tmux display -p \\\"#{@pane_border_pid}\\\"); [ -n \\\"\\\$pid\\\" ] && kill -USR1 \\\$pid 2>/dev/null || true'\""
echo "  set-hook -g pane-focus-out \"run-shell 'pid=\\\$(tmux display -p \\\"#{@pane_border_pid}\\\"); [ -n \\\"\\\$pid\\\" ] && kill -USR2 \\\$pid 2>/dev/null || true'\""
