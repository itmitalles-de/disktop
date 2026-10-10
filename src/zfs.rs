//! Read-only OpenZFS pool discovery. Dataset statvfs values cannot be summed:
//! every dataset reports shared free space and may have its own quota.
//! `zpool list -HpvLP` supplies exact pool allocation and resolved leaf paths.
//! See OpenZFS zpool-list(8); verbose leaf rows start with a tab even in -H
//! mode, and auxiliary class headers use spaces (OpenZFS 2.2).
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, process::CommandExt},
    },
    path::Path,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use crate::{diskstats, model::ZfsPool};

const LIMIT: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_millis(500);
const REFRESH: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default)]
pub struct Pool {
    pub usage: ZfsPool,
    pub members: Vec<String>,
}

type Discovery = Result<HashMap<String, Pool>, String>;

#[derive(Default)]
pub struct Pools {
    pending: Option<Receiver<Discovery>>,
    checked: Option<Instant>,
    result: Option<Discovery>,
    last_members: HashMap<String, Vec<String>>,
}

impl Pools {
    /// At most one worker/child per collector; even a kernel-stuck child
    /// being reaped cannot accumulate new workers. Old values expire before
    /// querying again, so unavailable pools never look silently current.
    pub fn refresh(&mut self) {
        if let Some(receiver) = &self.pending {
            match receiver.try_recv() {
                Ok(result) => {
                    if let Ok(pools) = &result {
                        self.last_members = pools
                            .iter()
                            .map(|(name, pool)| (name.clone(), pool.members.clone()))
                            .collect();
                    }
                    self.result = Some(result);
                    self.pending = None;
                    self.checked = Some(Instant::now());
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result = Some(Err("ZFS discovery worker unavailable".into()));
                    self.pending = None;
                    self.checked = Some(Instant::now());
                }
                Err(mpsc::TryRecvError::Empty) => return,
            }
        }
        if self
            .checked
            .is_some_and(|checked| checked.elapsed() < REFRESH)
        {
            return;
        }
        self.result = None;
        let (sender, receiver) = mpsc::channel();
        if std::thread::Builder::new()
            .name("spctop-zfs".into())
            .spawn(move || {
                let _ = sender.send(discover());
            })
            .is_ok()
        {
            self.pending = Some(receiver);
        } else {
            self.checked = Some(Instant::now());
            self.result = Some(Err("ZFS discovery worker unavailable".into()));
        }
    }

    pub fn get(&self, source: &str) -> Pool {
        let name = source.split('/').next().unwrap_or_default();
        if let Some(Ok(pools)) = &self.result
            && let Some(pool) = pools.get(name)
        {
            return pool.clone();
        }
        let mut pool = Pool {
            usage: ZfsPool {
                name: name.into(),
                error: Some(match &self.result {
                    Some(Err(error)) => error.clone(),
                    Some(Ok(_)) => "ZFS pool not reported by zpool".into(),
                    None => "ZFS pool discovery pending".into(),
                }),
                ..ZfsPool::default()
            },
            ..Pool::default()
        };
        // While refreshing (or after a failed query), preserve only the last
        // membership, never stale capacity. A mounted dataset must not make
        // its known disks appear unmounted for one sample every five seconds.
        if !matches!(self.result, Some(Ok(_))) {
            pool.members = self.last_members.get(name).cloned().unwrap_or_default();
        }
        pool
    }
}

fn discover() -> Discovery {
    let executable = [
        "/usr/sbin/zpool",
        "/usr/bin/zpool",
        "/sbin/zpool",
        "/bin/zpool",
    ]
    .into_iter()
    .find(|path| trusted_executable(Path::new(path)))
    .ok_or("ZFS pool discovery unavailable: trusted zpool executable not found")?;
    let output = run(
        executable,
        &[
            "list",
            "-H",
            "-p",
            "-L",
            "-P",
            "-v",
            "-o",
            "name,size,allocated,free",
        ],
        TIMEOUT,
    )
    .map_err(
        |_| "ZFS pool discovery unavailable: zpool failed, timed out, or exceeded output limit",
    )?;
    parse(&output)
}

fn trusted_executable(path: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    if !path.is_file() {
        return false;
    }
    path.ancestors().all(|path| {
        fs::metadata(path).is_ok_and(|meta| meta.uid() == 0 && meta.mode() & 0o022 == 0)
    })
}

fn run(executable: &str, args: &[&str], timeout: Duration) -> io::Result<String> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Only async-signal-safe syscalls in the post-fork child. No inherited
    // capabilities or file/setuid privilege gains, including root execution.
    let parent_pid = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                return Err(io::Error::other("collector exited before child setup"));
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            #[repr(C)]
            struct Header {
                version: u32,
                pid: i32,
            }
            let header = Header {
                version: 0x2008_0522,
                pid: 0,
            };
            let caps = [0u32; 6];
            if libc::syscall(libc::SYS_capset, &header, caps.as_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        let fd = stdout.as_raw_fd();
        // SAFETY: stdout owns this open descriptor until the loop returns.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let started = Instant::now();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            if started.elapsed() >= timeout {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "zpool timeout"));
            }
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    if let Some(status) = child.try_wait()? {
                        if !status.success() {
                            return Err(io::Error::other("zpool failed"));
                        }
                        return String::from_utf8(bytes).map_err(io::Error::other);
                    }
                }
                Ok(count) => {
                    if bytes.len() + count > LIMIT {
                        return Err(io::Error::other("zpool output limit"));
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    // Reap in this one worker. If kernel I/O prevents exit, its receiver
    // remains pending: collection stays responsive and never starts another.
    let _ = child.wait();
    result
}

fn parse(output: &str) -> Discovery {
    let mut pools: HashMap<String, Pool> = HashMap::new();
    let mut current = None::<String>;
    let mut skip_spares = false;
    for line in output.lines() {
        if let Some(member_line) = line.strip_prefix('\t') {
            let Some(pool) = current.as_ref().and_then(|name| pools.get_mut(name)) else {
                return Err("ZFS pool output has a member without a pool".into());
            };
            if skip_spares {
                continue;
            }
            let path = member_line.split('\t').next().unwrap_or_default();
            if let Some(name) = path.strip_prefix("/dev/") {
                if diskstats::valid_name(name) {
                    pool.members.push(name.into());
                } else {
                    pool.usage.error = Some("ZFS pool has unresolved member paths".into());
                }
            } else if path.starts_with('/') || path.chars().all(|ch| ch.is_ascii_digit()) {
                pool.usage.error = Some("ZFS pool has file-backed or unavailable members".into());
            }
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() == 4 {
            let name = fields[0];
            if name.is_empty() || name.contains('/') || name.chars().any(char::is_control) {
                return Err("ZFS pool output has an invalid name".into());
            }
            let usage = ZfsPool {
                name: name.into(),
                total_bytes: fields[1].parse().ok(),
                used_bytes: fields[2].parse().ok(),
                available_bytes: fields[3].parse().ok(),
                error: None,
            };
            if pools
                .insert(
                    name.into(),
                    Pool {
                        usage,
                        members: Vec::new(),
                    },
                )
                .is_some()
            {
                return Err("ZFS pool output has duplicate pools".into());
            }
            current = Some(name.into());
            skip_spares = false;
        } else {
            match line.split_whitespace().next() {
                Some("spare" | "spares") => skip_spares = true,
                Some("logs" | "cache" | "special" | "dedup") => skip_spares = false,
                _ => return Err("ZFS pool output format unavailable".into()),
            }
        }
    }
    for pool in pools.values_mut() {
        pool.members = pool
            .members
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if pool.members.is_empty()
            || pool.usage.total_bytes.is_none()
            || pool.usage.used_bytes.is_none()
            || pool.usage.available_bytes.is_none()
        {
            pool.usage.error = Some("ZFS pool usage or membership unavailable".into());
        }
    }
    Ok(pools)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_multiple_pools_mirrors_classes_and_excludes_unused_spares() {
        let pools = parse("tank\t1000\t400\t600\n\tmirror-0\t1000\t400\t600\t-\n\t/dev/sda1\t-\t-\t-\n\t/dev/sdb1\t-\t-\t-\nspecial    - - -\n\t/dev/nvme0n1p1\t-\t-\t-\nspare      - - -\n\t/dev/sdc1\t-\t-\t-\nother\t200\t20\t180\n\t/dev/sdd\t200\t20\t180\n").unwrap();
        assert_eq!(pools["tank"].members, ["nvme0n1p1", "sda1", "sdb1"]);
        assert_eq!(pools["tank"].usage.used_bytes, Some(400));
        assert!(pools["tank"].usage.error.is_none());
        assert_eq!(pools["other"].members, ["sdd"]);
        let discovery = Pools {
            result: Some(Ok(pools)),
            ..Pools::default()
        };
        assert_eq!(
            discovery.get("tank").members,
            discovery.get("tank/home/user").members
        );
        assert_eq!(discovery.get("tank/home").usage.used_bytes, Some(400));
        assert!(discovery.get("missing/home").usage.error.is_some());
    }
    #[test]
    fn unavailable_members_and_capacity_are_explicit() {
        let pools = parse("tank\t-\t-\t-\n\t/tmp/vdev\t-\t-\t-\n").unwrap();
        assert!(pools["tank"].members.is_empty());
        assert!(pools["tank"].usage.error.is_some());
        assert!(parse("\t/dev/sda\t1\n").is_err());
        let pending = Pools::default().get("tank/home/user");
        assert_eq!(pending.usage.name, "tank");
        assert!(pending.usage.error.is_some());
        let mut refreshing = Pools {
            last_members: HashMap::from([("tank".into(), vec!["sda1".into(), "sdb1".into()])]),
            ..Pools::default()
        };
        let pool = refreshing.get("tank/home");
        assert_eq!(pool.members, ["sda1", "sdb1"]);
        assert!(pool.usage.used_bytes.is_none() && pool.usage.error.is_some());
        refreshing.result = Some(Err("query failed".into()));
        assert_eq!(refreshing.get("tank").members, ["sda1", "sdb1"]);
        refreshing.result = Some(Ok(HashMap::new()));
        assert!(refreshing.get("tank").members.is_empty());
    }
    #[test]
    fn command_timeout_output_limit_and_environment_are_bounded() {
        let start = Instant::now();
        assert!(run("/bin/sleep", &["1"], Duration::from_millis(30)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(run("/usr/bin/yes", &[], Duration::from_secs(2)).is_err());
        let environment = run("/usr/bin/env", &[], TIMEOUT).unwrap();
        assert_eq!(environment.lines().count(), 2);
        assert!(environment.contains("LC_ALL=C"));
        let status = run("/bin/cat", &["/proc/self/status"], TIMEOUT).unwrap();
        for key in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
            let value = status.lines().find(|line| line.starts_with(key)).unwrap();
            assert_eq!(value.split_whitespace().nth(1), Some("0000000000000000"));
        }
        assert!(status.lines().any(|line| line == "NoNewPrivs:\t1"));
    }
}
