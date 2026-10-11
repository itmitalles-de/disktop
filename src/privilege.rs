//! Linux per-thread capability handling for the optional helper.
//!
//! The UI has no file capabilities. The helper receives only DAC_READ_SEARCH
//! and SYS_PTRACE. Its sampling thread keeps both to read other users'
//! /proc/PID/io; the home scanner thread keeps only DAC_READ_SEARCH to list
//! directories. Every thread sets NO_NEW_PRIVS.

use anyhow::{Context, Result, bail};

pub const CAP_DAC_READ_SEARCH: u32 = 2;
pub const CAP_SYS_PTRACE: u32 = 19;
pub const READ_CAPS: u64 = (1 << CAP_DAC_READ_SEARCH) | (1 << CAP_SYS_PTRACE);
pub const SCAN_CAPS: u64 = 1 << CAP_DAC_READ_SEARCH;

#[repr(C)]
struct Header {
    version: u32,
    pid: i32,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Data {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Permitted capabilities of the calling thread.
pub fn permitted() -> Result<u64> {
    let mut header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [Data::default(); 2];
    // The kernel capability ABI version 3 uses exactly two 32-bit words.
    let status = unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("reading thread capabilities");
    }
    Ok(u64::from(data[0].permitted) | (u64::from(data[1].permitted) << 32))
}

pub fn require_helper_capabilities() -> Result<()> {
    if permitted()? & READ_CAPS != READ_CAPS {
        bail!("Helper setup incomplete: run scripts/setup-helper.sh once as your regular user");
    }
    Ok(())
}

/// Keep at most `allowed` in the calling thread and lock out new privileges.
fn retain(allowed: u64) -> Result<()> {
    let keep = permitted()? & allowed;
    let mut header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [
        Data {
            effective: keep as u32,
            permitted: keep as u32,
            inheritable: 0,
        },
        Data {
            effective: (keep >> 32) as u32,
            permitted: (keep >> 32) as u32,
            inheritable: 0,
        },
    ];
    let status = unsafe { libc::syscall(libc::SYS_capset, &mut header, data.as_ptr()) };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("dropping thread capabilities");
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("locking thread privileges");
    }
    Ok(())
}

/// Replace the inherited environment with `LANG=C`, as when the UI launches
/// the helper. The installed helper has file capabilities even when executed
/// directly, so nothing may consult variables chosen by its caller (NSS,
/// locale or allocator tuning, for example).
///
/// # Safety
///
/// The process must still be single-threaded: no other thread may read or
/// modify the environment concurrently.
pub unsafe fn reset_environment() -> Result<()> {
    // SAFETY: guaranteed single-threaded by the caller.
    if unsafe { libc::clearenv() } != 0 {
        bail!("cannot clear the helper environment");
    }
    // SAFETY: guaranteed single-threaded by the caller.
    unsafe { std::env::set_var("LANG", "C") };
    Ok(())
}

/// Called by the helper's sampling thread before serving requests.
pub fn retain_process_read_privileges() -> Result<()> {
    retain(READ_CAPS)
}

/// Called first in every home scanner thread (helper or UI): directory
/// listing needs at most DAC_READ_SEARCH. The scan also runs with idle I/O
/// priority and a lower CPU priority so it does not disturb the system it
/// measures.
pub fn prepare_scan_thread() -> Result<()> {
    retain(SCAN_CAPS)?;
    // ioprio_set(IOPRIO_WHO_PROCESS, 0 = calling thread, IOPRIO_CLASS_IDLE).
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    const IOPRIO_CLASS_IDLE: libc::c_int = 3;
    const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
    // Best effort: a kernel or seccomp policy may refuse either change.
    unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT,
        );
        // On Linux, a thread ID selects only that thread.
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_environment_removes_injected_variables() {
        const CHILD: &str = "DWTOP_TEST_RESET_ENVIRONMENT_CHILD";
        const NAME: &str = "privilege::tests::reset_environment_removes_injected_variables";
        if std::env::var_os(CHILD).is_some() {
            // SAFETY: this re-executed test process runs only this test on one
            // test thread, and nothing else touches the environment meanwhile.
            unsafe { reset_environment() }.unwrap();
            let remaining: Vec<_> = std::env::vars_os().collect();
            assert_eq!(remaining, [("LANG".into(), "C".into())]);
            for name in [c"LD_LIBRARY_PATH", c"PATH", c"MALLOC_CONF", c"LOCPATH"] {
                // SAFETY: getenv reads a valid C string; no concurrent writers.
                assert!(unsafe { libc::getenv(name.as_ptr()) }.is_null());
            }
            return;
        }
        // Clearing the environment would affect other tests in this process.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--test-threads=1"])
            .env(CHILD, "1")
            .env("MALLOC_CONF", "/tmp/dwtop-test-injected")
            .env("LOCPATH", "/tmp/dwtop-test-injected")
            .env("LD_LIBRARY_PATH", "/tmp/dwtop-test-injected")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        assert!(
            stdout.contains("1 passed"),
            "child test did not run: {stdout}"
        );
    }

    #[test]
    fn scan_thread_keeps_at_most_dac_read_search_without_changing_parent() {
        let before = permitted().unwrap();
        std::thread::spawn(|| {
            prepare_scan_thread().unwrap();
            assert_eq!(permitted().unwrap() & !SCAN_CAPS, 0);
            assert_eq!(
                unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) },
                1
            );
        })
        .join()
        .unwrap();
        assert_eq!(permitted().unwrap(), before);
    }
}
