use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::sys::termios;
use nix::unistd::{dup2, execvp, fork, setsid, ForkResult, Pid};
use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

/// Represents a running child process in a PTY.
pub struct ChildPty {
    pub master_fd: OwnedFd,
    pub child_pid: Pid,
}

/// Set the terminal size on a PTY fd.
pub fn set_pty_size(fd: RawFd, cols: u16, rows: u16) -> nix::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ is a well-defined ioctl for terminal size
    let ret = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
    if ret == -1 {
        Err(nix::errno::Errno::last())
    } else {
        Ok(())
    }
}

/// Get the terminal size from a fd.
pub fn get_terminal_size(fd: RawFd) -> nix::Result<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if ret == -1 {
        Err(nix::errno::Errno::last())
    } else {
        Ok((ws.ws_col, ws.ws_row))
    }
}

/// Spawn a child process in a new PTY.
/// `inner_cols` and `inner_rows` are the inner PTY dimensions.
/// `command` is the program to run (e.g., the user's shell).
/// `args` are additional arguments to pass to the command.
pub fn spawn_child(
    inner_cols: u16,
    inner_rows: u16,
    command: &str,
    args: &[String],
) -> Result<ChildPty, Box<dyn std::error::Error>> {
    let OpenptyResult { master, slave } = openpty(None, None)?;

    set_pty_size(slave.as_raw_fd(), inner_cols, inner_rows)?;

    let cmd = CString::new(command)?;
    let mut c_args: Vec<CString> = vec![cmd.clone()];
    for arg in args {
        c_args.push(CString::new(arg.as_str())?);
    }

    // SAFETY: fork() is safe here as we immediately exec in the child
    match unsafe { fork() }? {
        ForkResult::Child => {
            // Child process: set up the slave PTY as stdin/stdout/stderr
            drop(master);

            // Create a new session
            setsid().ok();

            // Set controlling terminal
            unsafe {
                libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0);
            }

            // Redirect stdio to slave PTY
            if dup2(slave.as_raw_fd(), 0).is_err()
                || dup2(slave.as_raw_fd(), 1).is_err()
                || dup2(slave.as_raw_fd(), 2).is_err()
            {
                unsafe { libc::_exit(1) };
            }

            if slave.as_raw_fd() > 2 {
                drop(slave);
            }

            // Exec the command
            let _ = execvp(&cmd, &c_args);
            std::process::exit(1);
        }
        ForkResult::Parent { child } => {
            drop(slave);
            Ok(ChildPty {
                master_fd: master,
                child_pid: child,
            })
        }
    }
}

/// Put the terminal into raw mode, returning the original termios for restoration.
pub fn enter_raw_mode(fd: RawFd) -> nix::Result<termios::Termios> {
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let orig = termios::tcgetattr(&borrowed)?;
    let mut raw = orig.clone();
    termios::cfmakeraw(&mut raw);
    termios::tcsetattr(&borrowed, termios::SetArg::TCSANOW, &raw)?;
    Ok(orig)
}

/// Restore the terminal to the given termios settings.
pub fn restore_terminal(fd: RawFd, orig: &termios::Termios) -> nix::Result<()> {
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    termios::tcsetattr(&borrowed, termios::SetArg::TCSANOW, orig)
}
