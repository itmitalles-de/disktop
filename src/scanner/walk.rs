//! Bounded directory traversal and fair retention of directory details.

use super::policy::RootPolicy;
use super::{Limits, MAX_NAME_CHARS};
use crate::model::{DirNode, Folded, HomeReport, StopReason};
use crate::text::sanitize_bytes;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::fs::{stat_at, stat_fd};

/// Sanitize a directory name for display and bound its length in
/// characters, marking a cut with "~".
pub(super) fn display_name(name: &[u8]) -> String {
    let name = sanitize_bytes(name);
    if name.chars().count() <= MAX_NAME_CHARS {
        return name;
    }
    let mut out: String = name.chars().take(MAX_NAME_CHARS - 1).collect();
    out.push('~');
    out
}

pub(super) fn name_key(name: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut key = String::with_capacity(name.len() * 2);
    for byte in name {
        key.push(char::from(HEX[usize::from(byte >> 4)]));
        key.push(char::from(HEX[usize::from(byte & 15)]));
    }
    key
}

#[derive(Default, Clone, Copy)]
pub(super) struct Totals {
    pub(super) allocated: u64,
    pub(super) apparent: u64,
    pub(super) files: u64,
    pub(super) dirs: u64,
    pub(super) unreadable: u64,
    pub(super) truncated: bool,
}

impl Totals {
    pub(super) fn of(node: &DirNode) -> Self {
        Self {
            allocated: node.allocated,
            apparent: node.apparent,
            files: node.files,
            dirs: node.dirs,
            ..Self::default()
        }
    }

    pub(super) fn add(&mut self, other: &Self) {
        self.allocated = self.allocated.saturating_add(other.allocated);
        self.apparent = self.apparent.saturating_add(other.apparent);
        self.files = self.files.saturating_add(other.files);
        self.dirs = self.dirs.saturating_add(other.dirs);
        self.unreadable = self.unreadable.saturating_add(other.unreadable);
        self.truncated |= other.truncated;
    }
}

struct Walker<'a> {
    limits: &'a Limits,
    cancel: &'a AtomicBool,
    progress: &'a AtomicU64,
    started: Instant,
    root_dev: u64,
    entries: u64,
    stopped: Option<StopReason>,
    other_filesystems: u64,
    hardlinks: HashSet<(u64, u64)>,
    hardlink_limit_reached: bool,
    tree: Tree,
}

/// Retained directories in a slab, so indexes held by the recursion stay
/// valid while finished nodes are evicted.
pub(super) struct Slot {
    pub(super) node: DirNode,
    pub(super) parent: Option<usize>,
    pub(super) depth: usize,
    pub(super) finished: bool,
}

#[derive(Default)]
pub(super) struct Tree {
    pub(super) slots: Vec<Option<Slot>>,
    pub(super) free: Vec<usize>,
    pub(super) live: usize,
}

impl Tree {
    pub(super) fn alloc(&mut self, node: DirNode, parent: Option<usize>, depth: usize) -> usize {
        let slot = Some(Slot {
            node,
            parent,
            depth,
            finished: false,
        });
        self.live += 1;
        match self.free.pop() {
            Some(index) => {
                self.slots[index] = slot;
                index
            }
            None => {
                self.slots.push(slot);
                self.slots.len() - 1
            }
        }
    }

    pub(super) fn slot(&self, index: usize) -> &Slot {
        self.slots[index].as_ref().expect("live slot")
    }

    pub(super) fn node(&mut self, index: usize) -> &mut DirNode {
        &mut self.slots[index].as_mut().expect("live slot").node
    }

    pub(super) fn release_subtree(&mut self, index: usize) {
        let mut pending = vec![index];
        while let Some(index) = pending.pop() {
            if let Some(slot) = self.slots[index].take() {
                pending.extend(slot.node.children.iter().map(|child| *child as usize));
                self.free.push(index);
                self.live -= 1;
            }
        }
    }

    /// Divide retained nodes equally between root-child subtrees, lending
    /// unused shares to subtrees that still have candidates. Active ancestors
    /// are mandatory and count against their own subtree's share. Within a
    /// subtree, retain shallow nodes first, then larger directories; this
    /// keeps every selected node's ancestors. A same-depth flood in one home
    /// therefore cannot consume the other homes' shares.
    pub(super) fn evict_to(&mut self, target: usize) {
        if self.live <= target {
            return;
        }
        let mut branches: HashMap<usize, (usize, Vec<usize>)> = HashMap::new();
        let mut unfinished = 0;
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(slot) = slot else { continue };
            if !slot.finished || slot.parent.is_none() {
                unfinished += 1;
            }
            if slot.parent.is_none() {
                continue;
            }
            let mut branch = index;
            while let Some(parent) = self.slot(branch).parent {
                if self.slot(parent).parent.is_none() {
                    break;
                }
                branch = parent;
            }
            let (mandatory, candidates) = branches.entry(branch).or_default();
            if slot.finished {
                candidates.push(index);
            } else {
                *mandatory += 1;
            }
        }
        let mut branches: Vec<_> = branches.into_iter().collect();
        // When there is not even room for all root children, prefer larger
        // finished ones, with names as a stable tie break instead of visit order.
        branches.sort_by(|(left, _), (right, _)| {
            let left = &self.slot(*left).node;
            let right = &self.slot(*right).node;
            right
                .allocated
                .cmp(&left.allocated)
                .then(left.name.cmp(&right.name))
        });
        let mut available = std::collections::BinaryHeap::new();
        for (rank, (_, (mandatory, candidates))) in branches.iter_mut().enumerate() {
            candidates.sort_by(|left, right| {
                let left = self.slot(*left);
                let right = self.slot(*right);
                left.depth
                    .cmp(&right.depth)
                    .then(right.node.allocated.cmp(&left.node.allocated))
                    .then(left.node.name.cmp(&right.node.name))
            });
            candidates.reverse();
            if !candidates.is_empty() {
                available.push(std::cmp::Reverse((*mandatory, rank)));
            }
        }
        for _ in 0..target.saturating_sub(unfinished) {
            let Some(std::cmp::Reverse((count, rank))) = available.pop() else {
                break;
            };
            let candidates = &mut branches[rank].1.1;
            candidates.pop();
            if !candidates.is_empty() {
                available.push(std::cmp::Reverse((count + 1, rank)));
            }
        }
        let evicted: Vec<_> = branches
            .into_iter()
            .flat_map(|(_, (_, candidates))| candidates)
            .collect();
        let mut gone = vec![false; self.slots.len()];
        for index in &evicted {
            gone[*index] = true;
        }
        for index in &evicted {
            let (parent, totals) = {
                let slot = self.slot(*index);
                (slot.parent, Totals::of(&slot.node))
            };
            if let Some(parent) = parent.filter(|parent| !gone[*parent]) {
                let parent = self.node(parent);
                parent.children.retain(|child| *child as usize != *index);
                fold(&mut parent.other, &totals);
            }
        }
        for index in evicted {
            if self.slots[index].take().is_some() {
                self.free.push(index);
                self.live -= 1;
            }
        }
    }

    /// Renumber the reachable nodes breadth-first from the root.
    pub(super) fn into_nodes(mut self) -> Vec<DirNode> {
        let mut out: Vec<DirNode> = Vec::with_capacity(self.live);
        let mut queue = std::collections::VecDeque::from([(0usize, None::<usize>)]);
        while let Some((old, parent)) = queue.pop_front() {
            let Some(mut slot) = self.slots.get_mut(old).and_then(Option::take) else {
                continue;
            };
            let index = out.len();
            let children = std::mem::take(&mut slot.node.children);
            out.push(slot.node);
            if let Some(parent) = parent {
                out[parent].children.push(index as u32);
            }
            queue.extend(
                children
                    .into_iter()
                    .map(|child| (child as usize, Some(index))),
            );
        }
        out
    }
}

fn is_directory(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFDIR
}

fn allocated(stat: &libc::stat) -> u64 {
    (stat.st_blocks.max(0) as u64).saturating_mul(512)
}

fn apparent(stat: &libc::stat) -> u64 {
    let kind = stat.st_mode & libc::S_IFMT;
    if kind == libc::S_IFREG || kind == libc::S_IFLNK || kind == libc::S_IFDIR {
        stat.st_size.max(0) as u64
    } else {
        0
    }
}

impl Walker<'_> {
    fn budget_exhausted(&mut self) -> bool {
        if self.stopped.is_some() {
            return true;
        }
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            self.stopped = Some(StopReason::EntryBudget);
        } else if self.entries.is_multiple_of(512) {
            self.progress.store(self.entries, Ordering::Relaxed);
            if self.cancel.load(Ordering::Relaxed) {
                self.stopped = Some(StopReason::Cancelled);
            } else if self.started.elapsed() > self.limits.max_duration {
                self.stopped = Some(StopReason::TimeBudget);
            }
        }
        self.stopped.is_some()
    }

    /// Walk one open directory. The descriptor is consumed and closed.
    fn walk(&mut self, descriptor: OwnedFd, node: Option<usize>, depth: usize) -> Totals {
        let mut totals = Totals::default();
        let raw = descriptor.into_raw_fd();
        // SAFETY: raw is an open directory descriptor owned by us; on success
        // the DIR stream owns it and closedir closes it.
        let stream = unsafe { libc::fdopendir(raw) };
        if stream.is_null() {
            // SAFETY: fdopendir failed, so we still own raw.
            drop(unsafe { OwnedFd::from_raw_fd(raw) });
            totals.unreadable = 1;
            return totals;
        }
        let directory = raw;
        loop {
            // SAFETY: errno is thread-local; reset to detect readdir errors.
            unsafe { *libc::__errno_location() = 0 };
            // SAFETY: stream is a valid open DIR stream.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                if io::Error::last_os_error().raw_os_error().unwrap_or(0) != 0 {
                    totals.unreadable += 1;
                }
                break;
            }
            // SAFETY: d_name is NUL-terminated within the returned dirent.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            if self.budget_exhausted() {
                break;
            }
            let stat = match stat_at(directory, name) {
                Ok(stat) => stat,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
                Err(_) => {
                    totals.unreadable += 1;
                    continue;
                }
            };
            if is_directory(&stat) {
                if stat.st_dev != self.root_dev {
                    self.other_filesystems += 1;
                    continue;
                }
                totals.dirs += 1;
                let child = self.directory(directory, name, &stat, node, depth + 1);
                totals.add(&child);
            } else {
                totals.files += 1;
                let counted = stat.st_nlink <= 1 || self.first_link(stat.st_dev, stat.st_ino);
                let (bytes, size) = if counted {
                    (allocated(&stat), apparent(&stat))
                } else {
                    (0, 0)
                };
                totals.allocated = totals.allocated.saturating_add(bytes);
                totals.apparent = totals.apparent.saturating_add(size);
                if let Some(index) = node {
                    let own = &mut self.tree.node(index).own_files;
                    own.files += 1;
                    own.allocated = own.allocated.saturating_add(bytes);
                    own.apparent = own.apparent.saturating_add(size);
                }
            }
        }
        // SAFETY: stream is valid and closed exactly once.
        unsafe { libc::closedir(stream) };
        if let Some(index) = node {
            self.prune(index, self.limits.top_children);
        }
        totals
    }

    /// Count a subdirectory: its own inode plus its subtree.
    fn directory(
        &mut self,
        parent: libc::c_int,
        name: &CStr,
        stat: &libc::stat,
        node: Option<usize>,
        depth: usize,
    ) -> Totals {
        let mut sub = Totals {
            allocated: allocated(stat),
            apparent: apparent(stat),
            ..Totals::default()
        };
        let child_node = (node.is_some() && depth <= self.limits.keep_depth && self.reserve())
            .then(|| {
                self.tree.alloc(
                    DirNode {
                        name: display_name(name.to_bytes()),
                        key: Some(name_key(name.to_bytes())),
                        ..DirNode::default()
                    },
                    node,
                    depth,
                )
            });
        if depth >= self.limits.max_depth {
            sub.truncated = true;
        } else {
            // SAFETY: name is NUL-terminated; O_NOFOLLOW refuses a symlink that
            // replaced the directory after fstatat, O_DIRECTORY anything else.
            let child = unsafe {
                libc::openat(
                    parent,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if child < 0 {
                sub.unreadable += 1;
            } else {
                // SAFETY: openat returned a new descriptor that we now own.
                let child = unsafe { OwnedFd::from_raw_fd(child) };
                match stat_fd(child.as_raw_fd()) {
                    // A mount or replacement appeared between fstatat and openat.
                    Ok(opened) if opened.st_dev != stat.st_dev || opened.st_ino != stat.st_ino => {
                        sub.unreadable += 1;
                    }
                    Ok(_) => {
                        let inner = self.walk(child, child_node, depth);
                        sub.add(&inner);
                    }
                    Err(_) => sub.unreadable += 1,
                }
            }
        }
        match (child_node, node) {
            (Some(index), Some(parent)) => {
                let slot = self.tree.slots[index].as_mut().expect("live slot");
                slot.finished = true;
                let entry = &mut slot.node;
                entry.allocated = sub.allocated;
                entry.apparent = sub.apparent;
                entry.files = sub.files;
                entry.dirs = sub.dirs;
                entry.unreadable = sub.unreadable;
                entry.truncated = sub.truncated;
                let siblings = &mut self.tree.node(parent).children;
                siblings.push(index as u32);
                // Exact streaming top-K: an evicted child can never re-enter.
                if siblings.len() > 2 * self.limits.top_children {
                    self.prune(parent, self.limits.top_children);
                }
            }
            (None, Some(parent)) => fold(&mut self.tree.node(parent).other, &sub),
            _ => {}
        }
        sub
    }

    /// Room for one more retained node, evicting low-priority ones if needed.
    fn reserve(&mut self) -> bool {
        let cap = self.limits.max_nodes.saturating_mul(2) + self.limits.keep_depth + 1;
        if self.tree.live >= cap {
            // The active descent has a separate bounded allowance: it must
            // not force a completed neighbour to lose its fair share merely
            // because another subtree currently has many open ancestors.
            self.tree
                .evict_to(self.limits.max_nodes.saturating_add(self.limits.keep_depth));
        }
        self.tree.live < cap
    }

    fn first_link(&mut self, device: u64, inode: u64) -> bool {
        if self.hardlinks.contains(&(device, inode)) {
            return false;
        }
        if self.hardlinks.len() >= self.limits.max_hardlinks {
            self.hardlink_limit_reached = true;
        } else {
            self.hardlinks.insert((device, inode));
        }
        true
    }

    /// Keep only the largest subdirectories of a node; summarize the rest.
    fn prune(&mut self, index: usize, keep: usize) {
        let mut children = std::mem::take(&mut self.tree.node(index).children);
        children
            .sort_by_key(|child| std::cmp::Reverse(self.tree.slot(*child as usize).node.allocated));
        if children.len() > keep {
            for child in children.split_off(keep) {
                let totals = Totals::of(&self.tree.slot(child as usize).node);
                fold(&mut self.tree.node(index).other, &totals);
                self.tree.release_subtree(child as usize);
            }
        }
        self.tree.node(index).children = children;
    }
}

pub(super) fn fold(other: &mut Folded, totals: &Totals) {
    other.dirs = other.dirs.saturating_add(1);
    other.allocated = other.allocated.saturating_add(totals.allocated);
    other.apparent = other.apparent.saturating_add(totals.apparent);
    other.files = other.files.saturating_add(totals.files);
}

/// Scan `root` once, opened like `du`. Fails only if the root itself
/// cannot be opened.
pub fn scan(
    root: &Path,
    limits: &Limits,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> io::Result<HomeReport> {
    scan_with(root, &RootPolicy::Follow, limits, cancel, progress)
}

/// Scan `root` once after opening it under `policy`.
pub fn scan_with(
    root: &Path,
    policy: &RootPolicy,
    limits: &Limits,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> io::Result<HomeReport> {
    let started = Instant::now();
    let started_unix = unix_now();
    let descriptor = policy.open(root)?;
    let stat = stat_fd(descriptor.as_raw_fd())?;
    progress.store(0, Ordering::Relaxed);
    let mut walker = Walker {
        limits,
        cancel,
        progress,
        started,
        root_dev: stat.st_dev,
        entries: 0,
        stopped: None,
        other_filesystems: 0,
        hardlinks: HashSet::new(),
        hardlink_limit_reached: false,
        tree: Tree::default(),
    };
    walker.tree.alloc(
        DirNode {
            name: sanitize_bytes(root.as_os_str().as_bytes()),
            ..DirNode::default()
        },
        None,
        0,
    );
    let mut totals = walker.walk(descriptor, Some(0), 0);
    totals.allocated = totals.allocated.saturating_add(allocated(&stat));
    totals.apparent = totals.apparent.saturating_add(apparent(&stat));
    walker.tree.evict_to(limits.max_nodes);
    let root_node = walker.tree.node(0);
    root_node.allocated = totals.allocated;
    root_node.apparent = totals.apparent;
    root_node.files = totals.files;
    root_node.dirs = totals.dirs;
    root_node.unreadable = totals.unreadable;
    root_node.truncated = totals.truncated;
    progress.store(walker.entries, Ordering::Relaxed);
    Ok(HomeReport {
        root: sanitize_bytes(root.as_os_str().as_bytes()),
        generation: 0,
        started_unix,
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        entries: walker.entries.min(limits.max_entries),
        stopped: walker.stopped,
        other_filesystems: walker.other_filesystems,
        hardlink_limit_reached: walker.hardlink_limit_reached,
        privileged: false,
        nodes: walker.tree.into_nodes(),
    })
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
