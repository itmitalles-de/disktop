//! Filesystem calls that can block forever, run with a timeout.
//!
//! stat, statvfs and path resolution wait for the filesystem to answer. A dead
//! NFS hard mount, a stuck FUSE daemon (ntfs-3g, exfat-fuse) or a failing disk
//! can keep them waiting indefinitely, and only SIGKILL helps. The sampling
//! thread therefore never makes such calls itself: a worker thread does, and a
//! mount that does not answer in time is reported as not responding and
//! skipped until its outstanding call returns. Each hung mount holds at most
//! one blocked thread, and the number of hung mounts tracked is bounded, so
//! leaked threads stay bounded too.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use crate::mounts::{self, Usage};

/// Per-call deadline and the total mount-probe budget of one sample.
pub const CALL_TIMEOUT: Duration = Duration::from_millis(500);
/// Total outstanding calls, including those that have exceeded their deadline.
const MAX_HUNG: usize = 64;
/// Calls still within their deadline; expired calls cannot occupy every lane.
const MAX_CONCURRENT: usize = 8;

/// What the blocking calls found out about one mount point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// The mount point is a directory; Docker binds single files such as
    /// /etc/hosts, which are not filesystems to show.
    pub is_dir: bool,
    pub usage: Option<Usage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done(Probe),
    /// The call did not return in time, or an earlier call still has not.
    NotResponding,
}

pub type ProbeFn = fn(&Path) -> Probe;

/// The real calls: lstat on the mount point, statvfs on directories.
pub fn probe_mount(path: &Path) -> Probe {
    let is_dir = fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
    Probe {
        is_dir,
        usage: is_dir.then(|| mounts::statvfs(path)).flatten(),
    }
}

pub struct MountProber {
    probe: ProbeFn,
    /// Both the per-call deadline and the total budget of a sample. The
    /// constructor baseline and first helper request each spend at most one
    /// budget, comfortably below the helper's ten-second reply deadline.
    timeout: Duration,
    /// At most one outstanding call per path. Its result remains in the
    /// channel across samples, including after it was reported unavailable.
    pending: HashMap<PathBuf, Instant>,
    sender: mpsc::Sender<(PathBuf, Probe)>,
    receiver: mpsc::Receiver<(PathBuf, Probe)>,
    /// Rotate launches so an early set of slow paths cannot starve later ones.
    next_start: usize,
}

impl Default for MountProber {
    fn default() -> Self {
        Self::new(probe_mount, CALL_TIMEOUT)
    }
}

impl MountProber {
    pub fn new(probe: ProbeFn, timeout: Duration) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            probe,
            timeout,
            pending: HashMap::new(),
            sender,
            receiver,
            next_start: 0,
        }
    }

    /// Mount points currently waiting for an outstanding call.
    pub fn hung(&self) -> usize {
        self.pending.len()
    }

    fn accept(
        &mut self,
        message: (PathBuf, Probe),
        requested: &HashSet<PathBuf>,
        results: &mut HashMap<PathBuf, Outcome>,
    ) {
        let (path, probe) = message;
        self.pending.remove(&path);
        if requested.contains(&path) {
            results.insert(path, Outcome::Done(probe));
        }
    }

    fn drain(&mut self, requested: &HashSet<PathBuf>, results: &mut HashMap<PathBuf, Outcome>) {
        while let Ok(message) = self.receiver.try_recv() {
            self.accept(message, requested, results);
        }
    }

    /// Probe paths within one shared deadline. A bounded number of calls run
    /// concurrently; calls that exceed the deadline remain outstanding and
    /// are never restarted until their result arrives. Late results are used
    /// on the next sample instead of being lost or immediately reprobed.
    pub fn probe_all(&mut self, paths: &[PathBuf]) -> Vec<Outcome> {
        let deadline = Instant::now() + self.timeout;
        let requested: HashSet<_> = paths.iter().cloned().collect();
        let mut results = HashMap::new();
        self.drain(&requested, &mut results);
        let mut queued = HashSet::new();
        let start = self.next_start % paths.len().max(1);
        let mut todo: VecDeque<_> = (0..paths.len())
            .map(|offset| (start + offset) % paths.len())
            .filter_map(|index| {
                let path = &paths[index];
                (!results.contains_key(path)
                    && !self.pending.contains_key(path)
                    && queued.insert(path.clone()))
                .then(|| (index, path.clone()))
            })
            .collect();
        loop {
            self.drain(&requested, &mut results);
            if Instant::now() >= deadline {
                break;
            }
            let mut active = self
                .pending
                .values()
                .filter(|started| started.elapsed() < self.timeout)
                .count();
            while active < MAX_CONCURRENT && self.pending.len() < MAX_HUNG {
                let Some((index, path)) = todo.pop_front() else {
                    break;
                };
                self.next_start = index + 1;
                let sender = self.sender.clone();
                let probe = self.probe;
                let owned = path.clone();
                let started = Instant::now();
                let spawned = std::thread::Builder::new()
                    .name("spctop-probe".into())
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        let result = probe(&owned);
                        let _ = sender.send((owned, result));
                    });
                if spawned.is_ok() {
                    self.pending.insert(path, started);
                    active += 1;
                } else {
                    results.insert(path, Outcome::NotResponding);
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
            self.drain(&requested, &mut results);
            // Only wait for calls that can still answer in time. Old stalled
            // calls do not add another wait, including when all slots are full.
            let now = Instant::now();
            let next_call_deadline = self
                .pending
                .iter()
                .filter(|(path, _)| requested.contains(*path) || !todo.is_empty())
                .map(|(_, started)| *started + self.timeout)
                .filter(|end| *end > now)
                .min();
            let Some(end) = next_call_deadline else {
                // Completed calls freed lanes: start queued paths immediately.
                if !todo.is_empty() && self.pending.len() < MAX_HUNG && now < deadline {
                    continue;
                }
                break;
            };
            // Draining may have freed a lane before all queued mounts started.
            if !todo.is_empty()
                && self.pending.len() < MAX_HUNG
                && self
                    .pending
                    .values()
                    .filter(|started| started.elapsed() < self.timeout)
                    .count()
                    < MAX_CONCURRENT
            {
                continue;
            }
            match self
                .receiver
                .recv_timeout(end.min(deadline).saturating_duration_since(now))
            {
                Ok(message) => self.accept(message, &requested, &mut results),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.drain(&requested, &mut results);
        paths
            .iter()
            .map(|path| results.get(path).copied().unwrap_or(Outcome::NotResponding))
            .collect()
    }
}

type ResolveFn = fn(&Path) -> Option<PathBuf>;

fn canonicalize(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

/// Resolve `path` with a timeout. `None` while it has not answered.
pub fn resolve_with_timeout(path: &Path, timeout: Duration) -> Option<PathBuf> {
    let (sender, receiver) = mpsc::channel();
    let owned = path.to_owned();
    std::thread::Builder::new()
        .name("spctop-resolve".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let _ = sender.send(canonicalize(&owned));
        })
        .ok()?;
    receiver.recv_timeout(timeout).ok().flatten()
}

/// Cached symlink resolution of the home path, done off the sampling thread.
/// Until the path answers, the lexical path is used; one resolution per
/// distinct path is outstanding at a time.
pub struct Resolver {
    resolve: ResolveFn,
    timeout: Duration,
    path: PathBuf,
    resolved: Option<PathBuf>,
    pending: Option<mpsc::Receiver<Option<PathBuf>>>,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new(canonicalize, Duration::from_millis(200))
    }
}

impl Resolver {
    pub fn new(resolve: ResolveFn, timeout: Duration) -> Self {
        Self {
            resolve,
            timeout,
            path: PathBuf::new(),
            resolved: None,
            pending: None,
        }
    }

    pub fn resolve(&mut self, path: &Path) -> PathBuf {
        if self.path != path {
            self.path = path.to_owned();
            self.resolved = None;
            // A resolution of the previous path may still be running; its
            // result is dropped with the receiver.
            self.pending = None;
        }
        if let Some(resolved) = &self.resolved {
            return resolved.clone();
        }
        let receiver = match self.pending.take() {
            Some(receiver) => receiver,
            None => {
                let (sender, receiver) = mpsc::channel();
                let resolve = self.resolve;
                let owned = path.to_owned();
                let spawned = std::thread::Builder::new()
                    .name("spctop-resolve".into())
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        let _ = sender.send(resolve(&owned));
                    });
                if spawned.is_err() {
                    return mounts::lexical(path);
                }
                receiver
            }
        };
        match receiver.recv_timeout(self.timeout) {
            Ok(result) => {
                // A path that does not exist (yet) stays lexical.
                let resolved = result.unwrap_or_else(|| mounts::lexical(path));
                self.resolved = Some(resolved.clone());
                resolved
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending = Some(receiver);
                mounts::lexical(path)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => mounts::lexical(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Instant,
    };

    /// Paths below /hang block until the test releases them.
    static RELEASED: Mutex<bool> = Mutex::new(false);

    fn fake(path: &Path) -> Probe {
        if path.starts_with("/hang") {
            while !*RELEASED.lock().unwrap() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Probe {
            is_dir: true,
            usage: Some(Usage {
                total: path.as_os_str().len() as u64,
                ..Usage::default()
            }),
        }
    }

    #[test]
    fn hung_mounts_are_reported_skipped_and_recovered() {
        let mut prober = MountProber::new(fake, Duration::from_millis(50));
        let paths: Vec<PathBuf> = ["/a", "/hang", "/bb", "/ccc"].map(PathBuf::from).into();
        let started = Instant::now();
        let first = prober.probe_all(&paths);
        assert!(matches!(first[0], Outcome::Done(probe) if probe.usage.unwrap().total == 2));
        assert_eq!(first[1], Outcome::NotResponding);
        // Mounts after the hung one are still probed by a new worker.
        assert!(matches!(first[2], Outcome::Done(probe) if probe.usage.unwrap().total == 3));
        assert!(matches!(first[3], Outcome::Done(_)));
        assert_eq!(prober.hung(), 1);
        // The hung mount is skipped without waiting again.
        let again_started = Instant::now();
        let second = prober.probe_all(&paths);
        assert_eq!(second[1], Outcome::NotResponding);
        assert!(again_started.elapsed() < Duration::from_millis(45));
        assert!(started.elapsed() < Duration::from_secs(5));
        // Once the call returns, the mount is probed again.
        *RELEASED.lock().unwrap() = true;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let third = prober.probe_all(&paths);
            if matches!(third[1], Outcome::Done(_)) {
                break;
            }
            assert!(Instant::now() < deadline, "hung mount never recovered");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(prober.hung(), 0);
        assert!(prober.probe_all(&[]).is_empty());
    }

    #[test]
    fn many_slow_mounts_share_one_budget_and_keep_late_results() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn slow(_: &Path) -> Probe {
            CALLS.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(250));
            Probe {
                is_dir: true,
                usage: Some(Usage {
                    total: 123,
                    ..Default::default()
                }),
            }
        }
        let paths: Vec<_> = (0..21)
            .map(|index| PathBuf::from(format!("/slow-{index}")))
            .collect();
        let mut prober = MountProber::new(slow, Duration::from_millis(20));
        let start = Instant::now();
        assert!(
            prober
                .probe_all(&paths)
                .iter()
                .all(|result| *result == Outcome::NotResponding)
        );
        // An old per-path timeout takes at least 420 ms here. Two sampling
        // passes model the constructor baseline plus first helper reply.
        assert!(start.elapsed() < Duration::from_millis(150));
        prober.probe_all(&paths);
        assert!(start.elapsed() < Duration::from_millis(200));
        let first_path = paths[0].clone();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = prober.probe_all(std::slice::from_ref(&first_path));
            if matches!(result[0], Outcome::Done(probe) if probe.usage.unwrap().total == 123) {
                break;
            }
            assert!(Instant::now() < deadline, "late result was discarded");
            std::thread::sleep(Duration::from_millis(5));
        }
        // The first path was not retried while waiting or when accepting its
        // result. The two original passes launch no more than sixteen calls.
        assert!(CALLS.load(Ordering::Relaxed) <= MAX_CONCURRENT * 2);
    }

    #[test]
    fn pending_paths_are_unique_bounded_and_later_mounts_get_a_turn() {
        static RELEASE: AtomicBool = AtomicBool::new(false);
        const PATHS: usize = MAX_HUNG + 11;
        static CALLS: [AtomicUsize; PATHS] = [const { AtomicUsize::new(0) }; PATHS];
        struct Release;
        impl Drop for Release {
            fn drop(&mut self) {
                RELEASE.store(true, Ordering::Release);
            }
        }
        fn blocked(path: &Path) -> Probe {
            let index: usize = path.file_name().unwrap().to_str().unwrap().parse().unwrap();
            CALLS[index].fetch_add(1, Ordering::Relaxed);
            while !RELEASE.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
            Probe {
                is_dir: true,
                usage: Some(Usage {
                    total: index as u64,
                    ..Default::default()
                }),
            }
        }
        let _release = Release;
        let mut paths: Vec<_> = (0..PATHS)
            .map(|index| PathBuf::from(format!("/{index}")))
            .collect();
        paths.push(paths[0].clone()); // Duplicate mounts still own one call.
        let mut prober = MountProber::new(blocked, Duration::from_millis(10));
        for _ in 0..MAX_HUNG {
            prober.probe_all(&paths);
            assert!(prober.hung() <= MAX_HUNG);
            if prober.hung() == MAX_HUNG {
                break;
            }
        }
        assert_eq!(prober.hung(), MAX_HUNG);
        assert!(CALLS.iter().all(|calls| calls.load(Ordering::Relaxed) <= 1));
        // All blocked lanes are full: no new thread and no repeated wait.
        let start = Instant::now();
        prober.probe_all(&paths);
        assert!(start.elapsed() < Duration::from_millis(50));
        RELEASE.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while CALLS[PATHS - 1].load(Ordering::Relaxed) == 0 {
            prober.probe_all(&paths);
            assert!(
                Instant::now() < deadline,
                "later mount starved after lanes freed"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            CALLS[MAX_HUNG..]
                .iter()
                .all(|calls| calls.load(Ordering::Relaxed) > 0)
        );
        // Drain outstanding results even when the caller removed every mount.
        while prober.hung() > 0 {
            prober.probe_all(&[]);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn quick_mount_after_slow_lanes_is_not_starved() {
        static RELEASE: AtomicBool = AtomicBool::new(false);
        fn mixed(path: &Path) -> Probe {
            while path != Path::new("/ready") && !RELEASE.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
            Probe {
                is_dir: true,
                usage: Some(Usage {
                    total: 42,
                    ..Default::default()
                }),
            }
        }
        let mut paths: Vec<_> = (0..MAX_CONCURRENT * 2)
            .map(|index| PathBuf::from(format!("/slow/{index}")))
            .collect();
        paths.push(PathBuf::from("/ready"));
        let mut prober = MountProber::new(mixed, Duration::from_millis(10));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = prober.probe_all(&paths);
            if matches!(result.last(), Some(Outcome::Done(probe)) if probe.usage.unwrap().total == 42)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "quick mount behind slow paths starved"
            );
        }
        RELEASE.store(true, Ordering::Release);
    }

    #[test]
    fn resolver_uses_the_lexical_path_until_resolution_answers() {
        fn slow(path: &Path) -> Option<PathBuf> {
            std::thread::sleep(Duration::from_millis(100));
            Some(Path::new("/var").join(path.strip_prefix("/").unwrap()))
        }
        let mut resolver = Resolver::new(slow, Duration::from_millis(10));
        assert_eq!(
            resolver.resolve(Path::new("/home/./tim")),
            Path::new("/home/tim")
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while resolver.resolve(Path::new("/home/./tim")) != Path::new("/var/home/./tim") {
            assert!(Instant::now() < deadline, "resolution never arrived");
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut real = Resolver::default();
        assert_eq!(
            real.resolve(Path::new("/nonexistent-spctop/x")),
            Path::new("/nonexistent-spctop/x")
        );
        assert_eq!(
            resolve_with_timeout(Path::new("/"), Duration::from_secs(2)),
            Some(PathBuf::from("/"))
        );
    }
}
