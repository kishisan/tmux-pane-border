use signal_hook::consts::{SIGCHLD, SIGUSR1, SIGUSR2, SIGWINCH};
use signal_hook::iterator::Signals;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Signals we care about, represented as atomic flags readable from the main loop.
pub struct SignalFlags {
    pub winch: Arc<AtomicBool>,
    pub usr1: Arc<AtomicBool>,
    pub usr2: Arc<AtomicBool>,
    pub child: Arc<AtomicBool>,
}

impl SignalFlags {
    /// Register signal handlers and return the flags.
    /// Also returns the Signals iterator handle so its thread can be managed.
    pub fn register() -> std::io::Result<(Self, Signals)> {
        let winch = Arc::new(AtomicBool::new(false));
        let usr1 = Arc::new(AtomicBool::new(false));
        let usr2 = Arc::new(AtomicBool::new(false));
        let child = Arc::new(AtomicBool::new(false));

        let signals = Signals::new([SIGWINCH, SIGUSR1, SIGUSR2, SIGCHLD])?;

        Ok((
            Self {
                winch,
                usr1,
                usr2,
                child,
            },
            signals,
        ))
    }

    /// Process pending signals from the iterator, setting atomic flags.
    /// Call this from a dedicated thread.
    pub fn process_signals(&self, signals: &mut Signals) {
        for sig in signals.forever() {
            match sig {
                SIGWINCH => self.winch.store(true, Ordering::SeqCst),
                SIGUSR1 => self.usr1.store(true, Ordering::SeqCst),
                SIGUSR2 => self.usr2.store(true, Ordering::SeqCst),
                SIGCHLD => self.child.store(true, Ordering::SeqCst),
                _ => {}
            }
        }
    }

    pub fn take_winch(&self) -> bool {
        self.winch.swap(false, Ordering::SeqCst)
    }

    pub fn take_usr1(&self) -> bool {
        self.usr1.swap(false, Ordering::SeqCst)
    }

    pub fn take_usr2(&self) -> bool {
        self.usr2.swap(false, Ordering::SeqCst)
    }

    pub fn take_child(&self) -> bool {
        self.child.swap(false, Ordering::SeqCst)
    }
}

