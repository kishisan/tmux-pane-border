mod border;
mod config;
mod pty;
mod signal;
mod vt_filter;

use clap::Parser;
use config::Config;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::termios::Termios;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::process::Command;

/// RAII guard that restores terminal settings on drop (including panic).
struct RawModeGuard {
    fd: RawFd,
    orig: Termios,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = pty::restore_terminal(self.fd, &self.orig);
    }
}

/// A PTY wrapper that draws a colored border around the pane content.
#[derive(Parser, Debug)]
#[command(name = "tmux-pane-border", version, about)]
struct Cli {
    /// Command to run inside the bordered pane
    #[arg(last = true, required = true)]
    command: Vec<String>,

    /// Start in active state (use --no-active to start inactive)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    active: bool,
}

fn main() {
    let cli = Cli::parse();
    let config = Config::load();

    if cli.command.is_empty() {
        eprintln!("tmux-pane-border: no command specified");
        std::process::exit(1);
    }

    let command = &cli.command[0];
    let args = &cli.command[1..];

    // Register PID with tmux (if running inside tmux)
    register_pane_pid();

    if let Err(e) = run(command, args, &config, cli.active) {
        eprintln!("tmux-pane-border: {e}");
        std::process::exit(1);
    }
}

/// Register our PID as @pane_border_pid in the current tmux pane.
fn register_pane_pid() {
    let pid = std::process::id();
    // Silently ignore errors (we might not be in tmux)
    let _ = Command::new("tmux")
        .args(["set", "-p", "@pane_border_pid", &pid.to_string()])
        .output();
}

fn run(
    command: &str,
    args: &[String],
    config: &Config,
    initial_active: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdin_fd = io::stdin().as_raw_fd();

    // Get outer terminal size
    let (outer_cols, outer_rows) = pty::get_terminal_size(stdin_fd)?;

    if outer_cols < 5 || outer_rows < 5 {
        eprintln!("tmux-pane-border: terminal too small ({}x{})", outer_cols, outer_rows);
        std::process::exit(1);
    }

    let inner_cols = outer_cols - 2;
    let inner_rows = outer_rows - 2;

    // Spawn child in inner PTY
    let child = pty::spawn_child(inner_cols, inner_rows, command, args)?;
    let master_raw = child.master_fd.as_raw_fd();

    // Enter raw mode on outer terminal (guard ensures restore on panic)
    let orig_termios = pty::enter_raw_mode(stdin_fd)?;
    let _raw_guard = RawModeGuard { fd: stdin_fd, orig: orig_termios.clone() };

    // Set up signal handling
    let (sig_flags, signals) = signal::SignalFlags::register()?;

    // Spawn signal processing thread
    let sig_flags_clone = signal::SignalFlags {
        winch: sig_flags.winch.clone(),
        usr1: sig_flags.usr1.clone(),
        usr2: sig_flags.usr2.clone(),
        child: sig_flags.child.clone(),
    };
    let mut signals_handle = signals;
    std::thread::spawn(move || {
        sig_flags_clone.process_signals(&mut signals_handle);
    });

    // Track state
    let mut is_active = initial_active;
    let mut cur_outer_cols = outer_cols;
    let mut cur_outer_rows = outer_rows;

    // Initial border draw + position cursor in inner area
    draw_border_and_setup(cur_outer_cols, cur_outer_rows, is_active, config);

    // Main I/O loop
    let mut stdin_buf = [0u8; 4096];
    let mut child_buf = [0u8; 8192];

    let mut stdout = io::stdout();
    let mut filter_state = vt_filter::FilterState::new();
    let (_, _, _, _, _, v_char) = border::style_chars(config.border.style);

    loop {
        // Check signals
        if sig_flags.take_child() {
            // Check if child actually exited
            match waitpid(child.child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => {
                    break;
                }
                _ => {}
            }
        }

        if sig_flags.take_winch() {
            // Outer terminal resized
            if let Ok((new_cols, new_rows)) = pty::get_terminal_size(stdin_fd) {
                if new_cols >= 5 && new_rows >= 5 {
                    cur_outer_cols = new_cols;
                    cur_outer_rows = new_rows;
                    let new_inner_cols = new_cols - 2;
                    let new_inner_rows = new_rows - 2;

                    // Resize inner PTY
                    let _ = pty::set_pty_size(master_raw, new_inner_cols, new_inner_rows);

                    // Redraw border
                    draw_border_and_setup(cur_outer_cols, cur_outer_rows, is_active, config);

                    // Reset cursor tracking (old position may be out of bounds)
                    filter_state.reset_cursor_row();
                }
            }
        }

        if sig_flags.take_usr1() {
            is_active = true;
            redraw_border(cur_outer_cols, cur_outer_rows, is_active, config);
        }

        if sig_flags.take_usr2() {
            is_active = false;
            redraw_border(cur_outer_cols, cur_outer_rows, is_active, config);
        }

        // Poll for I/O
        let stdin_pollfd = PollFd::new(
            unsafe { BorrowedFd::borrow_raw(stdin_fd) },
            PollFlags::POLLIN,
        );
        let master_pollfd = PollFd::new(
            unsafe { BorrowedFd::borrow_raw(master_raw) },
            PollFlags::POLLIN,
        );

        let mut fds = [stdin_pollfd, master_pollfd];

        match poll(&mut fds, PollTimeout::from(50u16)) {
            Ok(0) => continue, // timeout - check signals again
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                eprintln!("poll error: {e}");
                break;
            }
        }

        // Check stdin -> child (user input)
        if let Some(revents) = fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let n = match nix::unistd::read(stdin_fd, &mut stdin_buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => continue,
                    Err(_) => break,
                };

                let input = &stdin_buf[..n];

                // Check for mouse input and transform coordinates
                let to_write = if is_mouse_sequence(input) {
                    match vt_filter::transform_mouse_input(
                        input,
                        cur_outer_cols,
                        cur_outer_rows,
                    ) {
                        vt_filter::MouseTransform::Transformed(transformed) => transformed,
                        vt_filter::MouseTransform::OnBorder => continue,
                        vt_filter::MouseTransform::ParseError => input.to_vec(),
                    }
                } else {
                    input.to_vec()
                };

                if nix::unistd::write(&child.master_fd, &to_write).is_err() {
                    break;
                }
            }
            if revents.contains(PollFlags::POLLHUP) || revents.contains(PollFlags::POLLERR) {
                break;
            }
        }

        // Check child → stdout (child output)
        if let Some(revents) = fds[1].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let n = match nix::unistd::read(master_raw, &mut child_buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(nix::errno::Errno::EIO) => {
                        // EIO on master means child closed the slave
                        break;
                    }
                    Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => continue,
                    Err(_) => break,
                };

                let raw_output = &child_buf[..n];

                // Build border info for the VT filter (needed for border repair)
                let active_color_str = if is_active {
                    &config.border.active_color
                } else {
                    &config.border.inactive_color
                };
                let color_seq = border::fg_color_seq(active_color_str);
                let border_info = vt_filter::BorderInfo {
                    vertical_char: v_char,
                    color_seq: &color_seq,
                };

                // Filter and offset the output
                let filtered =
                    vt_filter::filter_child_output(raw_output, cur_outer_cols, cur_outer_rows, &border_info, &mut filter_state);

                stdout.write_all(&filtered).ok();

                // Redraw border if needed (alt screen switch, full clear, or RIS detected by state machine)
                if filter_state.take_border_redraw() {
                    let border_str = border::render_border(
                        cur_outer_cols,
                        cur_outer_rows,
                        config.border.style,
                        if is_active {
                            &config.border.active_color
                        } else {
                            &config.border.inactive_color
                        },
                    );
                    stdout.write_all(border_str.as_bytes()).ok();
                    // Re-set scroll region (alt screen exit resets it)
                    let inner_top = 2;
                    let inner_bottom = cur_outer_rows - 1;
                    write!(stdout, "\x1b[{inner_top};{inner_bottom}r").ok();
                }

                stdout.flush().ok();
            }
            if revents.contains(PollFlags::POLLHUP) || revents.contains(PollFlags::POLLERR) {
                break;
            }
        }
    }

    // Cleanup
    pty::restore_terminal(stdin_fd, &orig_termios)?;

    // Wait for child to fully exit
    let _ = waitpid(child.child_pid, None);

    Ok(())
}

/// Draw the border and clear/set up the inner area.
fn draw_border_and_setup(cols: u16, rows: u16, active: bool, config: &Config) {
    let color = if active {
        &config.border.active_color
    } else {
        &config.border.inactive_color
    };

    let mut stdout = io::stdout();

    // Clear screen first
    let _ = stdout.write_all(b"\x1b[2J");

    // Draw border
    let border_str = border::render_border(cols, rows, config.border.style, color);
    let _ = stdout.write_all(border_str.as_bytes());

    // Set scroll region to inner area
    let inner_top = 2;
    let inner_bottom = rows - 1;
    let _ = write!(stdout, "\x1b[{inner_top};{inner_bottom}r");

    // Position cursor at top-left of inner area
    let _ = stdout.write_all(b"\x1b[2;2H");

    let _ = stdout.flush();
}

/// Redraw just the border (without clearing the inner content).
fn redraw_border(cols: u16, rows: u16, active: bool, config: &Config) {
    let color = if active {
        &config.border.active_color
    } else {
        &config.border.inactive_color
    };

    let mut stdout = io::stdout();
    let border_str = border::render_border(cols, rows, config.border.style, color);
    let _ = stdout.write_all(border_str.as_bytes());
    let _ = stdout.flush();
}

/// Simple heuristic to check if input looks like a mouse sequence.
fn is_mouse_sequence(input: &[u8]) -> bool {
    // SGR mouse: ESC [ <
    if input.len() >= 3 && input[0] == 0x1B && input[1] == b'[' && input[2] == b'<' {
        return true;
    }
    // X10 mouse: ESC [ M followed by 3 bytes
    if input.len() >= 6 && input[0] == 0x1B && input[1] == b'[' && input[2] == b'M' {
        return true;
    }
    false
}

