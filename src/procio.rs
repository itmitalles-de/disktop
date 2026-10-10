//! Per-process storage I/O from /proc/PID/io.
//!
//! `read_bytes` and `write_bytes` count bytes a task caused to be read from or
//! written (dirtied) to the storage layer, across all devices; they cannot be
//! attributed to individual block devices. `cancelled_write_bytes` counts
//! dirty data that a truncate discarded before writeback; the kernel charges
//! it to the truncating task, which need not be the writer, so it is shown
//! separately rather than subtracted.

use std::{
    collections::HashMap,
    ffi::CStr,
    fs,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Instant,
};

use crate::{
    model::{ProcessAccess, ProcessRow},
    text::sanitize_bytes,
};

const MAX_PROC_READ: u64 = 4096;
const MAX_PROCESSES: usize = 100_000;
const MAX_USERS: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoCounters {
    pub rchar: u64,
    pub wchar: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled_write_bytes: u64,
}

/// Parse /proc/PID/io. Storage counters are required: kernels without task
/// I/O accounting only provide rchar/wchar, which include page-cache hits.
pub fn parse_io(text: &str) -> Option<IoCounters> {
    let mut counters = IoCounters::default();
    let (mut read, mut write) = (false, false);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            continue;
        };
        match key.trim() {
            "rchar" => counters.rchar = value,
            "wchar" => counters.wchar = value,
            "read_bytes" => {
                counters.read_bytes = value;
                read = true;
            }
            "write_bytes" => {
                counters.write_bytes = value;
                write = true;
            }
            "cancelled_write_bytes" => counters.cancelled_write_bytes = value,
            _ => {}
        }
    }
    (read && write).then_some(counters)
}

/// Task name and start time (clock ticks after boot) from /proc/PID/stat.
/// The name may contain spaces and parentheses, so the last ')' ends it.
pub fn parse_stat(bytes: &[u8]) -> Option<(String, u64)> {
    let open = bytes.iter().position(|byte| *byte == b'(')?;
    let close = bytes.iter().rposition(|byte| *byte == b')')?;
    if close < open {
        return None;
    }
    let name = sanitize_bytes(&bytes[open + 1..close]);
    let rest = std::str::from_utf8(bytes.get(close + 1..)?).ok()?;
    // Fields after the name start with field 3 (state); starttime is field 22.
    let start = rest.split_ascii_whitespace().nth(19)?.parse().ok()?;
    Some((name, start))
}

/// Delta of cumulative counters, or `None` if any counter went backwards.
pub fn delta(previous: &IoCounters, current: &IoCounters) -> Option<IoCounters> {
    Some(IoCounters {
        rchar: current.rchar.checked_sub(previous.rchar)?,
        wchar: current.wchar.checked_sub(previous.wchar)?,
        read_bytes: current.read_bytes.checked_sub(previous.read_bytes)?,
        write_bytes: current.write_bytes.checked_sub(previous.write_bytes)?,
        cancelled_write_bytes: current
            .cancelled_write_bytes
            .checked_sub(previous.cancelled_write_bytes)?,
    })
}

struct Entry {
    start: u64,
    counters: IoCounters,
    name: String,
    command: String,
}

pub struct ProcessTable {
    proc_root: PathBuf,
    previous: HashMap<u32, Entry>,
    last: Option<(Instant, f64)>,
    clock_ticks: f64,
    users: Option<UserCache>,
}

impl ProcessTable {
    /// `resolve_users` is false in the privileged helper: user names are
    /// resolved by the unprivileged UI, so NSS modules never run with
    /// capabilities.
    pub fn new(proc_root: impl Into<PathBuf>, resolve_users: bool) -> Self {
        // SAFETY: sysconf has no preconditions.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        Self {
            proc_root: proc_root.into(),
            previous: HashMap::new(),
            last: None,
            clock_ticks: if ticks > 0 { ticks as f64 } else { 100.0 },
            users: resolve_users.then(UserCache::default),
        }
    }

    pub fn sample(&mut self, privileged: bool) -> (Vec<ProcessRow>, ProcessAccess) {
        let now = Instant::now();
        let boot_now = boot_seconds();
        let seconds = self
            .last
            .map(|(at, _)| now.duration_since(at).as_secs_f64())
            .filter(|seconds| *seconds > 0.0);
        let last_boot = self.last.map(|(_, boot)| boot);
        let mut rows = Vec::new();
        let mut current = HashMap::new();
        let mut unreadable = 0usize;
        let entries = match fs::read_dir(&self.proc_root) {
            Ok(entries) => entries,
            Err(error) => {
                return (
                    rows,
                    ProcessAccess {
                        privileged,
                        message: format!("cannot list processes: {error}"),
                        ..ProcessAccess::default()
                    },
                );
            }
        };
        for entry in entries.filter_map(Result::ok) {
            if rows.len() + unreadable >= MAX_PROCESSES {
                break;
            }
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
                .filter(|pid| *pid > 0)
            else {
                continue;
            };
            let directory = entry.path();
            let Some((name, start)) =
                read_bytes(&directory.join("stat")).and_then(|bytes| parse_stat(&bytes))
            else {
                continue; // Exited meanwhile.
            };
            let counters = match read_text(&directory.join("io")) {
                Ok(text) => match parse_io(&text) {
                    Some(counters) => counters,
                    None => {
                        unreadable += 1;
                        continue;
                    }
                },
                // ptrace access checks fail with EACCES; exited tasks with
                // ENOENT or ESRCH, which are simply skipped.
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    unreadable += 1;
                    continue;
                }
                Err(_) => continue,
            };
            let Ok(metadata) = fs::metadata(&directory) else {
                continue;
            };
            let uid = metadata.uid();
            let previous = self.previous.remove(&pid).filter(|old| old.start == start);
            let command = match &previous {
                Some(old) if old.name == name => old.command.clone(),
                _ => read_command(&directory, &name),
            };
            let started_after_last = last_boot.is_some_and(|boot| {
                start as f64 / self.clock_ticks + 1.0 / self.clock_ticks >= boot
            });
            let change = match (&previous, seconds) {
                (Some(old), Some(_)) => delta(&old.counters, &counters),
                // A process born after the previous sample started from zero.
                (None, Some(_)) if started_after_last => Some(counters),
                _ => None,
            };
            let rate = |value: u64| seconds.map(|seconds| value as f64 / seconds);
            rows.push(ProcessRow {
                pid,
                uid,
                user: self
                    .users
                    .as_mut()
                    .map_or_else(|| uid.to_string(), |users| users.name(uid)),
                name: name.clone(),
                command: command.clone(),
                read_rate: change.and_then(|change| rate(change.read_bytes)),
                write_rate: change.and_then(|change| rate(change.write_bytes)),
                cancelled_write_rate: change.and_then(|change| rate(change.cancelled_write_bytes)),
                read_bytes: counters.read_bytes,
                write_bytes: counters.write_bytes,
                cancelled_write_bytes: counters.cancelled_write_bytes,
            });
            current.insert(
                pid,
                Entry {
                    start,
                    counters,
                    name,
                    command,
                },
            );
        }
        // Exited processes are dropped, so memory follows the live process set.
        self.previous = current;
        self.last = Some((now, boot_now));
        let readable = rows.len();
        let message = if privileged {
            "All processes readable (helper or root)".to_owned()
        } else if unreadable > 0 {
            format!(
                "{unreadable} processes of other users are hidden; run scripts/setup-helper.sh once"
            )
        } else {
            "All visible processes readable".to_owned()
        };
        (
            rows,
            ProcessAccess {
                privileged,
                readable,
                unreadable,
                message,
            },
        )
    }
}

fn read_command(directory: &Path, name: &str) -> String {
    let Some(mut bytes) = read_bytes(&directory.join("cmdline")) else {
        return format!("[{name}]");
    };
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    if bytes.is_empty() {
        return format!("[{name}]");
    }
    for byte in &mut bytes {
        if *byte == 0 {
            *byte = b' ';
        }
    }
    sanitize_bytes(&bytes)
}

fn read_bytes(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_PROC_READ)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

fn read_text(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_PROC_READ)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Seconds since boot, the clock used by /proc/PID/stat start times.
pub fn boot_seconds() -> f64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is a valid out-parameter.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
        return 0.0;
    }
    time.tv_sec as f64 + time.tv_nsec as f64 / 1e9
}

/// Bounded uid -> user name cache using the reentrant NSS lookup.
#[derive(Default)]
pub struct UserCache {
    names: HashMap<u32, String>,
}

impl UserCache {
    pub fn name(&mut self, uid: u32) -> String {
        if let Some(name) = self.names.get(&uid) {
            return name.clone();
        }
        if self.names.len() >= MAX_USERS {
            self.names.clear();
        }
        let name = lookup_user(uid).unwrap_or_else(|| uid.to_string());
        self.names.insert(uid, name.clone());
        name
    }

    /// Fill names for rows whose user field holds only the numeric uid.
    pub fn resolve(&mut self, rows: &mut [ProcessRow]) {
        for row in rows {
            row.user = self.name(row.uid);
        }
    }
}

fn lookup_user(uid: u32) -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: zeroed passwd is a valid out-parameter for getpwuid_r.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the given buffer length.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || entry.pw_name.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so pw_name points into buffer.
    let name = unsafe { CStr::from_ptr(entry.pw_name) };
    Some(sanitize_bytes(name.to_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_parsing_requires_storage_counters_and_tolerates_odd_lines() {
        let text = "rchar: 10\nwchar: 20\nsyscr: 1\nsyscw: 2\nread_bytes: 4096\n\
                    write_bytes: 8192\ncancelled_write_bytes: 512\n";
        assert_eq!(
            parse_io(text),
            Some(IoCounters {
                rchar: 10,
                wchar: 20,
                read_bytes: 4096,
                write_bytes: 8192,
                cancelled_write_bytes: 512,
            })
        );
        // Without CONFIG_TASK_IO_ACCOUNTING only character counters exist.
        assert_eq!(parse_io("rchar: 10\nwchar: 20\n"), None);
        assert_eq!(parse_io(""), None);
        let odd = "garbage\nread_bytes: -1\nread_bytes:  7 \nwrite_bytes:9\nnew_field: 3\n";
        assert_eq!(
            parse_io(odd).map(|c| (c.read_bytes, c.write_bytes, c.cancelled_write_bytes)),
            Some((7, 9, 0))
        );
    }

    #[test]
    fn stat_names_with_spaces_and_parentheses() {
        let line = b"1234 (my (odd) name) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 10";
        assert_eq!(parse_stat(line), Some(("my (odd) name".to_owned(), 98765)));
        assert_eq!(
            parse_stat(b"1 (a\x1bb) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 5")
                .unwrap()
                .0,
            "a\\x1bb"
        );
        assert_eq!(parse_stat(b"1 (short) S 0"), None);
        assert_eq!(parse_stat(b"no parentheses"), None);
        assert_eq!(parse_stat(b") reversed ("), None);
    }

    #[test]
    fn deltas_reject_counter_decreases() {
        let old = IoCounters {
            read_bytes: 10,
            write_bytes: 10,
            ..IoCounters::default()
        };
        let new = IoCounters {
            read_bytes: 15,
            write_bytes: 30,
            ..IoCounters::default()
        };
        assert_eq!(delta(&old, &new).unwrap().write_bytes, 20);
        assert_eq!(delta(&new, &old), None);
    }

    #[test]
    fn own_process_writes_are_measured_between_samples() {
        let mut table = ProcessTable::new("/proc", true);
        let (rows, access) = table.sample(false);
        let me = std::process::id();
        assert!(rows.iter().any(|row| row.pid == me), "own process missing");
        assert!(access.readable > 0);
        let row = rows.iter().find(|row| row.pid == me).unwrap();
        assert_eq!(row.read_rate, None, "first sample has no rates");
        assert!(!row.user.is_empty());
        let (rows, _) = table.sample(false);
        let row = rows.iter().find(|row| row.pid == me).unwrap();
        assert!(row.read_rate.is_some() && row.write_rate.is_some());
        assert!(row.command.contains("disktop") || row.command.starts_with('['));
    }
}
