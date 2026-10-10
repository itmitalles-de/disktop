//! Cooperative terminal shutdown for externally delivered termination signals.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use signal_hook::{
    SigId,
    consts::{SIGHUP, SIGINT, SIGTERM},
    flag, low_level,
};

/// Keep this guard alive until the terminal has been restored. Handlers only set
/// a flag; the main event loop performs cleanup outside the signal handler.
pub struct SignalGuard {
    requested: Arc<AtomicBool>,
    handlers: Vec<SigId>,
}

impl SignalGuard {
    pub fn new() -> io::Result<Self> {
        let mut guard = Self {
            requested: Arc::new(AtomicBool::new(false)),
            handlers: Vec::with_capacity(3),
        };
        for signal in [SIGTERM, SIGINT, SIGHUP] {
            guard
                .handlers
                .push(flag::register(signal, Arc::clone(&guard.requested))?);
        }
        Ok(guard)
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }

    /// Request shutdown as if SIGHUP had arrived, for a terminal that hung up.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Relaxed);
    }

    /// The shared flag, for a watchdog thread.
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.requested)
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            low_level::unregister(handler);
        }
    }
}
