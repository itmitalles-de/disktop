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
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use crate::mounts::{self, Usage};

/// How long one call may take before its mount counts as not responding.
pub const CALL_TIMEOUT: Duration = Duration::from_millis(500);
/// Mounts with an outstanding blocked call; further ones are not probed.
const MAX_HUNG: usize = 64;

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

enum Message {
    Started(usize),
    Finished(usize, Probe),
}

pub struct MountProber {
    probe: ProbeFn,
    timeout: Duration,
    /// Mount points whose call has not returned, with the flag that the
    /// blocked worker sets when it finally finishes.
    hung: HashMap<PathBuf, Arc<AtomicBool>>,
}

impl Default for MountProber {
    fn default() -> Self {
        Self::new(probe_mount, CALL_TIMEOUT)
    }
}

impl MountProber {
    pub fn new(probe: ProbeFn, timeout: Duration) -> Self {
        Self {
            probe,
            timeout,
            hung: HashMap::new(),
        }
    }

    /// Mount points currently waiting for an outstanding call.
    pub fn hung(&self) -> usize {
        self.hung.len()
    }

    /// Probe every path, in order, in worker threads. Returns one outcome per
    /// path. A worker stops after the call it is blocked in, so it never
    /// starts a call on a further mount once it was given up.
    pub fn probe_all(&mut self, paths: &[PathBuf]) -> Vec<Outcome> {
        self.hung.retain(|_, done| !done.load(Ordering::Acquire));
        let mut results: Vec<Option<Outcome>> = vec![None; paths.len()];
        let mut todo: Vec<usize> = Vec::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            if self.hung.contains_key(path) {
                results[index] = Some(Outcome::NotResponding);
            } else {
                todo.push(index);
            }
        }
        while !todo.is_empty() {
            if self.hung.len() >= MAX_HUNG {
                // Too many blocked threads already; do not risk more.
                for index in todo.drain(..) {
                    results[index] = Some(Outcome::NotResponding);
                }
                break;
            }
            let (sender, receiver) = mpsc::channel();
            let abandoned = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicBool::new(false));
            let jobs: Vec<(usize, PathBuf)> = todo
                .iter()
                .map(|index| (*index, paths[*index].clone()))
                .collect();
            let probe = self.probe;
            let worker_abandoned = Arc::clone(&abandoned);
            let worker_finished = Arc::clone(&finished);
            let spawned = std::thread::Builder::new()
                .name("disktop-probe".into())
                .stack_size(256 * 1024)
                .spawn(move || {
                    for (index, path) in jobs {
                        if worker_abandoned.load(Ordering::Acquire) {
                            break;
                        }
                        if sender.send(Message::Started(index)).is_err() {
                            break;
                        }
                        let result = probe(&path);
                        if sender.send(Message::Finished(index, result)).is_err() {
                            break;
                        }
                    }
                    worker_finished.store(true, Ordering::Release);
                });
            if spawned.is_err() {
                for index in todo.drain(..) {
                    results[index] = Some(Outcome::NotResponding);
                }
                break;
            }
            let mut current = None;
            let mut stuck = None;
            loop {
                match receiver.recv_timeout(self.timeout) {
                    Ok(Message::Started(index)) => current = Some(index),
                    Ok(Message::Finished(index, result)) => {
                        results[index] = Some(Outcome::Done(result));
                        current = None;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Some(index) = current {
                            stuck = Some(index);
                            break;
                        }
                        // Between two calls; the next message follows at once.
                    }
                }
            }
            let Some(index) = stuck else {
                break;
            };
            abandoned.store(true, Ordering::Release);
            results[index] = Some(Outcome::NotResponding);
            self.hung.insert(paths[index].clone(), finished);
            todo.retain(|other| results[*other].is_none());
        }
        results
            .into_iter()
            .map(|result| result.unwrap_or(Outcome::NotResponding))
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
        .name("disktop-resolve".into())
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
                    .name("disktop-resolve".into())
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
    use std::{sync::Mutex, time::Instant};

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
            real.resolve(Path::new("/nonexistent-disktop/x")),
            Path::new("/nonexistent-disktop/x")
        );
        assert_eq!(
            resolve_with_timeout(Path::new("/"), Duration::from_secs(2)),
            Some(PathBuf::from("/"))
        );
    }
}
