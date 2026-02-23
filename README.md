# tmux-pane-border

A PTY wrapper that draws a colored border around each tmux pane, providing [Zellij](https://zellij.dev/)-like active pane visibility.

tmux's built-in borders are shared between adjacent panes and don't exist at window edges, so it's impossible to highlight all 4 sides of the active pane with tmux configuration alone. This tool solves that by drawing a border **inside** each pane using a PTY wrapper.

```
┌─ tmux pane (80x24) ──────────────────────────┐
│ ╭━━━━━━━━━━━━ active: blue ━━━━━━━━━━━━━━━╮  │
│ │                                          │  │
│ │   your shell runs here (78x22)           │  │
│ │                                          │  │
│ ╰━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╯  │
└───────────────────────────────────────────────┘
```

## Features

- Full 4-edge colored border for the active pane
- Instant color switching on pane focus change (via `SIGUSR1`/`SIGUSR2`)
- VT sequence coordinate translation — fullscreen apps (vim, htop, less, etc.) work correctly
- Mouse event coordinate transformation (SGR and X10)
- 5 border styles: `rounded`, `heavy`, `double`, `single`, `ascii`
- Configurable colors (24-bit true color)
- [TPM](https://github.com/tmux-plugins/tpm) plugin support

## Requirements

- Linux
- Rust toolchain (for building)
- tmux

## Installation

### With TPM (recommended)

Add to `~/.tmux.conf`:

```tmux
set -g @plugin 'kishisan/tmux-pane-border'
```

Then press `prefix` + <kbd>I</kbd> to install. The plugin will be built automatically if `cargo` is available.

### Manual

```bash
git clone https://github.com/kishisan/tmux-pane-border.git
cd tmux-pane-border
cargo build --release
```

Then add to `~/.tmux.conf`:

```tmux
set -g default-command '/path/to/tmux-pane-border/target/release/tmux-pane-border -- /bin/zsh'

set-hook -g pane-focus-in  "run-shell 'pid=$(tmux display -p \"#{@pane_border_pid}\"); [ -n \"$pid\" ] && kill -USR1 $pid 2>/dev/null || true'"
set-hook -g pane-focus-out "run-shell 'pid=$(tmux display -p \"#{@pane_border_pid}\"); [ -n \"$pid\" ] && kill -USR2 $pid 2>/dev/null || true'"
```

Replace `/bin/zsh` with your shell.

## Configuration

Create `~/.config/tmux-pane-border/config.toml`:

```toml
[border]
style = "rounded"          # rounded | heavy | double | single | ascii
active_color = "#61afef"   # active pane border color
inactive_color = "#5c6370" # inactive pane border color

[behavior]
dim_inactive = false       # dim content of inactive panes
```

All options are optional — defaults are shown above.

### Border styles

```
rounded (default)    heavy          double         single         ascii
╭──────╮             ┏━━━━━━┓       ╔══════╗       ┌──────┐       +------+
│      │             ┃      ┃       ║      ║       │      │       |      |
╰──────╯             ┗━━━━━━┛       ╚══════╝       └──────┘       +------+
```

## How it works

Each tmux pane runs the wrapper instead of a bare shell. The wrapper:

1. Creates an inner PTY sized 2 columns and 2 rows smaller than the pane
2. Spawns your shell inside the inner PTY
3. Draws a Unicode box-drawing border in the 1-cell margin
4. Intercepts all child output, parsing VT sequences and offsetting absolute coordinates by (+1, +1)
5. Transforms incoming mouse events by (-1, -1) before forwarding to the inner PTY
6. Listens for `SIGUSR1`/`SIGUSR2` to switch border color (sent by tmux hooks on focus change)
7. Handles `SIGWINCH` to resize the inner PTY and redraw the border

## Known limitations

- **tmux copy-mode**: Border characters may be included when selecting text with `copy-mode`. A future workaround via a `capture-pane` wrapper is planned.
- **Usable pane area**: Each pane loses 2 columns and 2 rows for the border.
- **VT sequence coverage**: Most common sequences are handled. Some rare sequences may not be offset correctly — please open an issue if you find one.

## License

MIT
