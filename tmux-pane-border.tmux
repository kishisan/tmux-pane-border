#!/usr/bin/env bash
# TPM plugin entry point for tmux-pane-border

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Require tmux 3.1+ (for set -p pane-level options)
TMUX_VERSION="$(tmux -V | sed 's/[^0-9.]//g')"
TMUX_MAJOR="$(echo "$TMUX_VERSION" | cut -d. -f1)"
TMUX_MINOR="$(echo "$TMUX_VERSION" | cut -d. -f2)"
if [ "$TMUX_MAJOR" -lt 3 ] 2>/dev/null || { [ "$TMUX_MAJOR" -eq 3 ] && [ "${TMUX_MINOR:-0}" -lt 1 ]; } 2>/dev/null; then
    tmux display-message "tmux-pane-border: requires tmux 3.1+, found $TMUX_VERSION"
    exit 1
fi

BINARY="$CURRENT_DIR/target/release/tmux-pane-border"

# Check if binary exists, if not try to build
if [ ! -f "$BINARY" ]; then
    if command -v cargo &>/dev/null; then
        (cd "$CURRENT_DIR" && cargo build --release 2>/dev/null)
    fi
fi

if [ ! -f "$BINARY" ]; then
    tmux display-message "tmux-pane-border: binary not found. Run 'cargo build --release' in $CURRENT_DIR"
    exit 1
fi

# Get user's default shell
DEFAULT_SHELL="${SHELL:-/bin/bash}"

# Set default-command to wrap new panes with the border wrapper
tmux set -g default-command "$BINARY -- $DEFAULT_SHELL"

# Register hooks for active/inactive pane switching
# Uses @pane_border_pid pane option set by the wrapper at startup
tmux set-hook -g pane-focus-in \
    "run-shell 'pid=\$(tmux display -p \"#{@pane_border_pid}\"); \
     [ -n \"\$pid\" ] && kill -USR1 \$pid 2>/dev/null || true'"

tmux set-hook -g pane-focus-out \
    "run-shell 'pid=\$(tmux display -p \"#{@pane_border_pid}\"); \
     [ -n \"\$pid\" ] && kill -USR2 \$pid 2>/dev/null || true'"
