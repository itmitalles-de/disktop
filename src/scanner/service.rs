//! Background scan lifecycle, cancellation, reports and growth history.

use super::Limits;
use super::policy::{RootPolicy, is_not_allowed};
use super::walk::{scan_with, unix_now};
use crate::model::{HomeErrorKind, HomeReport, HomeStatus, ScanState};
use crate::text::{sanitize, sanitize_bytes};
use anyhow::Result;
use std::collections::HashMap;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Annotate allocated growth against earlier scans of the same root, keyed
/// by the retained directory paths.
fn path_sizes(report: &HomeReport) -> HashMap<String, u64> {
    let mut sizes = HashMap::with_capacity(report.nodes.len());
    let mut stack = vec![(0u32, String::new())];
    while let Some((index, path)) = stack.pop() {
        let Some(node) = report.node(index) else {
            continue;
        };
        sizes.insert(path.clone(), node.allocated);
        for child in &node.children {
            if let Some(child_node) = report.node(*child) {
                stack.push((*child, format!("{path}/{}", child_node.identity())));
            }
        }
    }
    sizes
}

fn annotate(
    report: &mut HomeReport,
    previous: Option<&HashMap<String, u64>>,
    first: Option<&HashMap<String, u64>>,
) {
    let mut stack = vec![(0u32, String::new())];
    while let Some((index, path)) = stack.pop() {
        let Some(node) = report.nodes.get_mut(index as usize) else {
            continue;
        };
        let current = node.allocated;
        let growth = |map: Option<&HashMap<String, u64>>| {
            map.and_then(|map| map.get(&path)).map(|old| {
                (current as i128 - *old as i128).clamp(i64::MIN.into(), i64::MAX.into()) as i64
            })
        };
        node.growth_previous = growth(previous);
        node.growth_start = growth(first);
        let children: Vec<u32> = node.children.clone();
        for child in children {
            if let Some(child_node) = report.nodes.get(child as usize) {
                stack.push((child, format!("{path}/{}", child_node.identity())));
            }
        }
    }
}

/// Root, sizes after the previous scan, and sizes after the first scan.
type History = (PathBuf, HashMap<String, u64>, HashMap<String, u64>);

struct State {
    root: PathBuf,
    interval: Duration,
    rescan: bool,
    scanning: bool,
    scan_started: Option<Instant>,
    report: Option<Arc<HomeReport>>,
    last_finished: Option<Instant>,
    last_finished_unix: Option<u64>,
    error: Option<(HomeErrorKind, String)>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    cancel: AtomicBool,
    stop: AtomicBool,
    progress: AtomicU64,
    privileged: bool,
    policy: RootPolicy,
}

/// Periodic background scans in a dedicated, low-priority thread.
pub struct HomeService {
    shared: Arc<Shared>,
}

impl HomeService {
    /// `prepare` runs first in the new thread, for example to drop
    /// capabilities and lower I/O priority. Its failure becomes a visible
    /// error and no scan runs.
    pub fn start(
        root: PathBuf,
        interval: Duration,
        limits: Limits,
        privileged: bool,
        policy: RootPolicy,
        prepare: fn() -> Result<()>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                root,
                interval,
                rescan: true,
                scanning: false,
                scan_started: None,
                report: None,
                last_finished: None,
                last_finished_unix: None,
                error: None,
            }),
            wake: Condvar::new(),
            cancel: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            progress: AtomicU64::new(0),
            privileged,
            policy,
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("dwtop-scan".into())
            // Directory recursion is depth-bounded; leave generous room.
            .stack_size(16 * 1024 * 1024)
            .spawn(move || run(worker, limits, prepare))?;
        Ok(Self { shared })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Change the root or interval; a new root discards the old tree and
    /// growth history and starts scanning at once.
    pub fn configure(&self, root: PathBuf, interval: Duration) {
        let mut state = self.lock();
        state.interval = interval;
        if state.root != root {
            state.root = root;
            state.report = None;
            state.error = None;
            state.rescan = true;
            self.shared.cancel.store(true, Ordering::Relaxed);
        }
        self.shared.wake.notify_all();
    }

    pub fn rescan(&self) {
        let mut state = self.lock();
        state.rescan = true;
        self.shared.wake.notify_all();
    }

    pub fn status(&self) -> HomeStatus {
        let state = self.lock();
        let scan_state = if state.scanning {
            ScanState::Scanning
        } else if state.error.is_some() {
            ScanState::Failed
        } else if state.report.is_some() {
            ScanState::Done
        } else {
            ScanState::Idle
        };
        HomeStatus {
            root: sanitize_bytes(state.root.as_os_str().as_bytes()),
            state: scan_state,
            generation: state.report.as_ref().map_or(0, |report| report.generation),
            progress_entries: if state.scanning {
                self.shared.progress.load(Ordering::Relaxed)
            } else {
                0
            },
            progress_seconds: state
                .scan_started
                .filter(|_| state.scanning)
                .map_or(0.0, |started| started.elapsed().as_secs_f64()),
            last_finished_unix: state.last_finished_unix,
            next_scan_in_seconds: (!state.scanning)
                .then(|| {
                    state
                        .last_finished
                        .map(|finished| state.interval.saturating_sub(finished.elapsed()).as_secs())
                })
                .flatten(),
            privileged: self.shared.privileged,
            error: state.error.as_ref().map(|(_, message)| message.clone()),
            error_kind: state.error.as_ref().map(|(kind, _)| *kind),
            helper_refused: false,
        }
    }

    /// A copy of the latest report if it is newer than `known`.
    pub fn report_if_newer(&self, known: u64) -> Option<HomeReport> {
        let state = self.lock();
        state
            .report
            .as_ref()
            .filter(|report| report.generation > known)
            .map(|report| (**report).clone())
    }
}

impl Drop for HomeService {
    fn drop(&mut self) {
        // The thread is detached: a scan blocked in a slow filesystem call
        // must not delay the UI's exit. It stops at its next entry.
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, State> {
    shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn run(shared: Arc<Shared>, limits: Limits, prepare: fn() -> Result<()>) {
    if let Err(error) = prepare() {
        lock(&shared).error = Some((
            HomeErrorKind::StartFailed,
            sanitize(&format!("cannot start scanner: {error:#}")),
        ));
        return;
    }
    let mut history: Option<History> = None;
    let mut generation = 0u64;
    loop {
        let root = {
            let mut state = lock(&shared);
            loop {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                let due = state
                    .last_finished
                    .is_none_or(|finished| finished.elapsed() >= state.interval);
                if state.rescan || due {
                    break;
                }
                let wait = state
                    .last_finished
                    .map_or(state.interval, |finished| {
                        state.interval.saturating_sub(finished.elapsed())
                    })
                    .min(Duration::from_secs(1));
                state = shared
                    .wake
                    .wait_timeout(state, wait)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            state.rescan = false;
            state.scanning = true;
            state.scan_started = Some(Instant::now());
            shared.cancel.store(false, Ordering::Relaxed);
            state.root.clone()
        };
        let result = scan_with(
            &root,
            &shared.policy,
            &limits,
            &shared.cancel,
            &shared.progress,
        );
        let mut state = lock(&shared);
        state.scanning = false;
        state.last_finished = Some(Instant::now());
        state.last_finished_unix = Some(unix_now());
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        if state.root != root {
            continue; // Reconfigured meanwhile; `rescan` is already set.
        }
        match result {
            Ok(mut report) => {
                if history.as_ref().is_none_or(|(path, _, _)| *path != root) {
                    history = None;
                }
                // Missing or approximate sizes would show as shrinking and
                // then growing again when a later scan is complete. Retention
                // folding preserves exact totals and remains eligible; entry,
                // time, depth, access and hardlink limits do not.
                if report.stopped.is_none()
                    && report.root_node().is_some_and(|node| !node.partial())
                    && !report.hardlink_limit_reached
                {
                    annotate(
                        &mut report,
                        history.as_ref().map(|(_, previous, _)| previous),
                        history.as_ref().map(|(_, _, first)| first),
                    );
                    let sizes = path_sizes(&report);
                    history = Some(match history.take() {
                        Some((path, _, first)) => (path, sizes, first),
                        None => (root.clone(), sizes.clone(), sizes),
                    });
                }
                generation += 1;
                report.generation = generation;
                report.privileged = shared.privileged;
                state.report = Some(Arc::new(report));
                state.error = None;
            }
            Err(error) => {
                let kind = if is_not_allowed(&error) {
                    HomeErrorKind::NotAllowed
                } else {
                    HomeErrorKind::ScanFailed
                };
                state.error = Some((
                    kind,
                    sanitize(&format!("cannot scan {}: {error}", root.display())),
                ));
            }
        }
    }
}
