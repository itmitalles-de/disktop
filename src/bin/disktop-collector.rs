//! Capability-limited helper: fixed operations, no shell, no user config.
//!
//! It reads /proc/PID/io of all processes (DAC_READ_SEARCH, SYS_PTRACE in the
//! sampling thread) and lists directories for the home view (DAC_READ_SEARCH
//! only, in the scanner thread). It never reads file contents.

use std::{
    io::{self, BufReader},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use disktop::{
    collector::Collector,
    helper::{self, PROTOCOL_VERSION, Reply, Request},
    privilege,
};

fn serve() -> Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 1 || arguments[0] != "--stdio" {
        bail!("this helper is launched by disktop; use disktop for the terminal interface");
    }
    privilege::require_helper_capabilities()?;
    // Directory names of other users must not end up in core dumps.
    // RLIMIT_CORE alone does not suppress pipe-based crash handlers.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // Every later thread, including the scanner, starts from these two
    // capabilities and NO_NEW_PRIVS; the scanner drops SYS_PTRACE itself.
    privilege::retain_process_read_privileges()?;
    let mut collector = Collector::new(true)?;
    let mut input = BufReader::new(io::stdin().lock());
    let mut output = io::stdout().lock();
    let mut previous: Option<Instant> = None;
    while let Some(frame) = helper::read_frame(&mut input, helper::MAX_REQUEST)? {
        let request: Request = serde_json::from_slice(&frame)?;
        request.validate()?;
        if let Some(previous) = previous {
            // Bound the sampling rate a caller can demand.
            let minimum = Duration::from_millis(100);
            std::thread::sleep(minimum.saturating_sub(previous.elapsed()));
        }
        let snapshot = collector.sample(&request.home)?;
        helper::write_reply(
            &mut output,
            &Reply::Snapshot {
                version: PROTOCOL_VERSION,
                snapshot: Box::new(snapshot),
            },
        )?;
        previous = Some(Instant::now());
    }
    Ok(())
}

fn main() {
    // Nothing may read inherited variables with capabilities: reset them first,
    // while still single-threaded.
    // SAFETY: no other thread exists yet.
    let result = unsafe { privilege::reset_environment() }.and_then(|()| serve());
    if let Err(error) = result {
        let _ = helper::write_reply(
            &mut io::stdout().lock(),
            &Reply::Error {
                version: PROTOCOL_VERSION,
                message: format!("{error:#}"),
            },
        );
        std::process::exit(1);
    }
}
