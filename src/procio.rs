//! Per-process storage I/O from /proc/PID/io.
//!
//! `read_bytes` and `write_bytes` count bytes a task caused to be read from or
//! written (dirtied) to the storage layer, across all devices; they cannot be
//! attributed to individual block devices. `cancelled_write_bytes` counts
//! dirty data that a truncate discarded before writeback; the kernel charges
//! it to the truncating task, which need not be the writer, so it is shown
//! separately rather than subtracted.
//!
//! Rates come from per-thread counters (/proc/PID/task/TID/io), not from the
//! process total: the total also contains the lifetime I/O of reaped
//! children, which the kernel adds to the parent when it waits for them, so
//! a shell that reaps a 5 GB `cp` would show 5 GB in one interval (and PID 1
//! or a subreaper constantly). Threads that started since the previous read
//! count from zero. I/O of threads that both start and exit between two
//! samples, and of a thread's last interval before it exits, is not shown.
//! The cumulative `read_bytes`/`write_bytes` fields stay the kernel's process
//! totals, including reaped children.

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
/// Processes with more threads fall back to the process total for rates.
const MAX_THREADS: usize = 10_000;

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

impl IoCounters {
    fn add(&mut self, other: &Self) {
        self.rchar = self.rchar.saturating_add(other.rchar);
        self.wchar = self.wchar.saturating_add(other.wchar);
        self.read_bytes = self.read_bytes.saturating_add(other.read_bytes);
        self.write_bytes = self.write_bytes.saturating_add(other.write_bytes);
        self.cancelled_write_bytes = self
            .cancelled_write_bytes
            .saturating_add(other.cancelled_write_bytes);
    }
}

/// Number of threads (field 20) from /proc/PID/stat.
pub fn parse_num_threads(bytes: &[u8]) -> Option<usize> {
    let close = bytes.iter().rposition(|byte| *byte == b')')?;
    let rest = std::str::from_utf8(bytes.get(close + 1..)?).ok()?;
    rest.split_ascii_whitespace().nth(17)?.parse().ok()
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

#[derive(Clone, Copy)]
struct Thread {
    start: u64,
    counters: IoCounters,
}

struct Entry {
    start: u64,
    counters: IoCounters,
    name: String,
    command: String,
    /// Per-thread counters and the boot time they were read at; `None` when
    /// the process had too many threads or the task list was unreadable.
    threads: Option<(HashMap<u32, Thread>, f64)>,
}

/// /proc mounted with hidepid restricts what users see of others' processes.
/// The helper bypasses it through CAP_SYS_PTRACE, so it applies the same
/// policy itself to processes of other users.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hidepid {
    #[default]
    Off,
    /// hidepid=1/noaccess: others' processes are listed, but their details
    /// are private. Only the task name (comm) is sent, never the command line.
    NoAccess,
    /// hidepid=2/invisible (and 4/ptraceable): others' processes are not
    /// listed. They are omitted and counted as hidden.
    Invisible,
}

/// The hidepid policy from /proc's superblock options (mountinfo), unless
/// the caller is in the exempt `gid=` group.
pub fn hidepid_from_options(options: &str, in_group: impl Fn(u32) -> bool) -> Hidepid {
    let mut mode = Hidepid::Off;
    for option in options.split(',') {
        match option.split_once('=') {
            Some(("hidepid", "1" | "noaccess")) => mode = Hidepid::NoAccess,
            Some(("hidepid", "2" | "invisible" | "4" | "ptraceable")) => mode = Hidepid::Invisible,
            Some(("gid", gid)) if gid.parse().is_ok_and(&in_group) => return Hidepid::Off,
            _ => {}
        }
    }
    mode
}

/// Whether the calling process has `gid` as its real or a supplementary group.
fn caller_in_group(gid: u32) -> bool {
    // SAFETY: getgid has no preconditions.
    if unsafe { libc::getgid() } == gid {
        return true;
    }
    let mut groups = vec![0 as libc::gid_t; 1024];
    // SAFETY: groups has room for the given number of entries.
    let count = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
    count > 0 && groups[..count as usize].contains(&gid)
}

fn proc_hidepid(proc_root: &Path) -> Hidepid {
    let Ok(bytes) = crate::collector::read_pseudo("/proc/self/mountinfo") else {
        return Hidepid::Off;
    };
    crate::mounts::visible(&crate::mounts::parse(&bytes))
        .into_iter()
        .rfind(|mount| mount.fs_type == "proc" && mount.mount_point == proc_root)
        .map_or(Hidepid::Off, |mount| {
            hidepid_from_options(&mount.super_options, caller_in_group)
        })
}

pub struct ProcessTable {
    proc_root: PathBuf,
    previous: HashMap<u32, Entry>,
    last: Option<(Instant, f64)>,
    clock_ticks: f64,
    users: Option<UserCache>,
    hidepid: Hidepid,
    /// The real uid whose processes hidepid never hides.
    own_uid: u32,
}

impl ProcessTable {
    /// `resolve_users` is false in the privileged helper: user names are
    /// resolved by the unprivileged UI, so NSS modules never run with
    /// capabilities.
    pub fn new(proc_root: impl Into<PathBuf>, resolve_users: bool) -> Self {
        // SAFETY: sysconf has no preconditions.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let proc_root = proc_root.into();
        // Root is exempt from kernel hidepid; a direct unprivileged reader must
        // still disclose restrictions even when hidepid=2 hides enumeration.
        // SAFETY: getuid has no preconditions.
        let own_uid = unsafe { libc::getuid() };
        let hidepid = if own_uid == 0 {
            Hidepid::Off
        } else {
            proc_hidepid(&proc_root)
        };
        Self {
            proc_root,
            previous: HashMap::new(),
            last: None,
            clock_ticks: if ticks > 0 { ticks as f64 } else { 100.0 },
            users: resolve_users.then(UserCache::default),
            hidepid,
            own_uid,
        }
    }

    /// The table of the privileged helper: no user name lookups, and the
    /// hidepid policy of `proc_root` applied to other users' processes.
    pub fn for_helper(proc_root: impl Into<PathBuf>) -> Self {
        let mut table = Self::new(proc_root, false);
        table.hidepid = proc_hidepid(&table.proc_root);
        table
    }

    /// Per-thread counters of a process, or `None` if they cannot be read
    /// completely (too many threads, task list unreadable).
    fn threads(&self, directory: &Path) -> Option<HashMap<u32, Thread>> {
        let entries = fs::read_dir(directory.join("task")).ok()?;
        let mut threads = HashMap::new();
        for entry in entries.filter_map(Result::ok) {
            if threads.len() >= MAX_THREADS {
                return None;
            }
            let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            let path = entry.path();
            // A thread that exited meanwhile is simply missing.
            let Some((_, start)) =
                read_bytes(&path.join("stat")).and_then(|bytes| parse_stat(&bytes))
            else {
                continue;
            };
            let Some(counters) = read_text(&path.join("io"))
                .ok()
                .and_then(|text| parse_io(&text))
            else {
                continue;
            };
            threads.insert(tid, Thread { start, counters });
        }
        Some(threads)
    }

    /// I/O of a process's threads since `old` was read at boot time `at`.
    /// A known thread contributes its increase, a thread that started after
    /// `at` its whole counters; one that existed before but was not read
    /// contributes nothing.
    fn thread_change(
        &self,
        old: &HashMap<u32, Thread>,
        at: f64,
        now: &HashMap<u32, Thread>,
    ) -> IoCounters {
        let mut change = IoCounters::default();
        for (tid, thread) in now {
            match old
                .get(tid)
                .filter(|previous| previous.start == thread.start)
            {
                Some(previous) => {
                    if let Some(increase) = delta(&previous.counters, &thread.counters) {
                        change.add(&increase);
                    }
                }
                None if self.started_after(thread.start, at) => change.add(&thread.counters),
                None => {}
            }
        }
        change
    }

    /// The task started after boot time `at` (one clock tick of tolerance).
    fn started_after(&self, start: u64, at: f64) -> bool {
        start as f64 / self.clock_ticks + 1.0 / self.clock_ticks >= at
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
        let mut hidden_by_hidepid = 0usize;
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
            let stat = match read_bytes_result(&directory.join("stat")) {
                Ok(stat) => stat,
                // hidepid=1 denies stat before the I/O read. Do not mistake
                // such a process for one that exited between enumeration and
                // sampling: its missing counters must remain visible.
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    unreadable += 1;
                    continue;
                }
                Err(_) => continue,
            };
            let Some((name, start)) = parse_stat(&stat) else {
                continue;
            };
            let thread_count = parse_num_threads(&stat).unwrap_or(usize::MAX);
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
            let foreign = uid != self.own_uid;
            if foreign && self.hidepid == Hidepid::Invisible {
                hidden_by_hidepid += 1;
                continue;
            }
            let previous = self.previous.remove(&pid).filter(|old| old.start == start);
            let command = match &previous {
                _ if foreign && self.hidepid == Hidepid::NoAccess => format!("[{name}]"),
                Some(old) if old.name == name => old.command.clone(),
                _ => read_command(&directory, &name),
            };
            let started_after_last = last_boot.is_some_and(|boot| self.started_after(start, boot));
            // Threads are read only when the process total moved (or to
            // start a baseline), so idle processes cost no extra reads.
            let unchanged = previous
                .as_ref()
                .is_some_and(|old| old.counters == counters && old.threads.is_some());
            let threads = if unchanged {
                previous.as_ref().and_then(|old| old.threads.clone())
            } else if thread_count <= MAX_THREADS {
                self.threads(&directory).map(|threads| (threads, boot_now))
            } else {
                None
            };
            let change = match (&previous, seconds) {
                (Some(_), Some(_)) if unchanged => Some(IoCounters::default()),
                (Some(old), Some(_)) => match (&old.threads, &threads) {
                    (Some((before, at)), Some((now, _))) => {
                        Some(self.thread_change(before, *at, now))
                    }
                    // Without thread counters, fall back to the total.
                    _ => delta(&old.counters, &counters),
                },
                // A process born after the previous sample started from zero.
                (None, Some(_)) if started_after_last => Some(match &threads {
                    Some((now, _)) => self.thread_change(&HashMap::new(), 0.0, now),
                    None => counters,
                }),
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
                    threads,
                },
            );
        }
        // Exited processes are dropped, so memory follows the live process set.
        self.previous = current;
        self.last = Some((now, boot_now));
        let readable = rows.len();
        let unreadable = unreadable + hidden_by_hidepid;
        let message = if hidden_by_hidepid > 0 {
            format!("{hidden_by_hidepid} processes of other users hidden by /proc hidepid")
        } else if self.hidepid == Hidepid::Invisible && !privileged {
            "Other users' processes hidden by /proc hidepid; total hidden count unavailable"
                .to_owned()
        } else if self.hidepid == Hidepid::NoAccess {
            if privileged {
                "Process I/O readable; other users' command lines hidden by /proc hidepid"
                    .to_owned()
            } else {
                format!("{unreadable} processes unreadable under /proc hidepid")
            }
        } else if privileged {
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
    read_bytes_result(path).ok()
}

fn read_bytes_result(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_PROC_READ)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
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
    use std::time::Duration;

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
    fn hidepid_options_and_thread_deltas() {
        let none = |_| false;
        assert_eq!(hidepid_from_options("rw,nosuid", none), Hidepid::Off);
        assert_eq!(
            hidepid_from_options("rw,hidepid=1", none),
            Hidepid::NoAccess
        );
        assert_eq!(
            hidepid_from_options("rw,hidepid=noaccess", none),
            Hidepid::NoAccess
        );
        assert_eq!(
            hidepid_from_options("rw,hidepid=2", none),
            Hidepid::Invisible
        );
        assert_eq!(
            hidepid_from_options("hidepid=invisible,subset=pid", none),
            Hidepid::Invisible
        );
        assert_eq!(
            hidepid_from_options("hidepid=ptraceable", none),
            Hidepid::Invisible
        );
        assert_eq!(hidepid_from_options("hidepid=0", none), Hidepid::Off);
        // Members of the gid= group see everything.
        assert_eq!(
            hidepid_from_options("hidepid=2,gid=27", |gid| gid == 27),
            Hidepid::Off
        );
        assert_eq!(
            hidepid_from_options("hidepid=2,gid=27", |gid| gid == 4),
            Hidepid::Invisible
        );

        let table = ProcessTable::new("/proc", false);
        let ticks = table.clock_ticks;
        let thread = |start_seconds: f64, read: u64| Thread {
            start: (start_seconds * ticks) as u64,
            counters: IoCounters {
                read_bytes: read,
                ..IoCounters::default()
            },
        };
        let before: HashMap<u32, Thread> = [(10, thread(5.0, 1000)), (11, thread(6.0, 50))].into();
        let now: HashMap<u32, Thread> = [
            (10, thread(5.0, 1500)), // +500
            // 11 exited: its last interval is lost (documented).
            (12, thread(20.0, 300)), // started after the read at 10 s: all of it
            (13, thread(2.0, 7000)), // existed before but was not read: nothing
            (14, thread(30.0, 0)),
        ]
        .into();
        assert_eq!(table.thread_change(&before, 10.0, &now).read_bytes, 800);
        // A reused thread ID with another start time counts as a new thread.
        let reused: HashMap<u32, Thread> = [(10, thread(25.0, 40))].into();
        assert_eq!(table.thread_change(&before, 10.0, &reused).read_bytes, 40);
        let stat = b"1234 (a) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 7 0 98765 1000 10";
        assert_eq!(parse_num_threads(stat), Some(7));
    }

    #[test]
    fn reaped_children_are_not_charged_to_the_parent() {
        // A shell runs dd and reaps it; the kernel then adds dd's lifetime
        // I/O to the shell's /proc/PID/io total. The shell itself writes
        // nothing. The file lives below target/, not on a tmpfs /tmp, where
        // writes are not storage I/O.
        const MIB: u64 = 1024 * 1024;
        let file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("dwtop-child-io-{}", std::process::id()));
        let script = format!(
            "sleep 0.5; dd if=/dev/zero of='{}' bs=1M count=8 conv=fsync status=none; sleep 10",
            file.display()
        );
        let Ok(mut shell) = std::process::Command::new("sh")
            .args(["-c", &script])
            .spawn()
        else {
            return;
        };
        let pid = shell.id();
        let total = || {
            read_text(&Path::new("/proc").join(pid.to_string()).join("io"))
                .ok()
                .and_then(|text| parse_io(&text))
                .map_or(0, |counters| counters.write_bytes)
        };
        let mut table = ProcessTable::new("/proc", false);
        std::thread::sleep(Duration::from_millis(100));
        table.sample(false);
        let started = Instant::now();
        let deadline = started + Duration::from_secs(20);
        while total() < 8 * MIB && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let reaped = total() >= 8 * MIB;
        let (rows, _) = table.sample(false);
        let elapsed = started.elapsed().as_secs_f64();
        let _ = shell.kill();
        let _ = shell.wait();
        let _ = fs::remove_file(&file);
        if !reaped {
            return; // No write accounting here (or dd missing).
        }
        let row = rows.iter().find(|row| row.pid == pid).expect("shell row");
        assert!(row.write_bytes >= 8 * MIB, "{row:?}");
        let charged = row.write_rate.expect("rate") * elapsed;
        assert!(
            charged < MIB as f64,
            "reaped child's I/O charged to the shell: {charged}"
        );
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
        assert!(row.command.contains("dwtop") || row.command.starts_with('['));
    }
}
