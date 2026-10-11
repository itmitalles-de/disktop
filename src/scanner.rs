//! Bounded background scanner for the home view.
//!
//! The scan stays on the root's filesystem (st_dev), never follows symlinks
//! (every directory is opened relative to its parent with O_NOFOLLOW), counts
//! hard-linked files once while its bounded (dev, inode) set has room, and
//! never reads file contents. Only directory names and sizes are retained,
//! in a tree limited by depth, children per directory and total node count.

use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;

use crate::{
    model::{DirNode, Folded, HomeErrorKind, HomeReport, HomeStatus, ScanState, StopReason},
    text::{sanitize, sanitize_bytes},
};

pub const MAX_ROOT_BYTES: usize = 4096;
/// Longest retained directory name in characters. A name of 255 invalid
/// bytes would otherwise become 1020 characters of escapes, and 20,000 such
/// nodes would approach the helper's response limit.
pub const MAX_NAME_CHARS: usize = 256;
/// Root-owned list of the trees the helper may scan, one absolute path per
/// line; `#` starts a comment. Without the file only /home is allowed.
pub const SCAN_ROOTS_PATH: &str = "/etc/dwtop/scan-roots";
pub const PREVIOUS_SCAN_ROOTS_PATH: &str = "/etc/spctop/scan-roots";
pub const LEGACY_SCAN_ROOTS_PATH: &str = "/etc/disktop/scan-roots";
pub const DEFAULT_SCAN_ROOTS: &[&str] = &["/home"];
const MAX_SCAN_ROOTS: usize = 64;
const MAX_POLICY_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct Limits {
    /// Directory entries visited per scan.
    pub max_entries: u64,
    pub max_duration: Duration,
    /// Directories deeper than this are not opened (bounds open descriptors).
    pub max_depth: usize,
    /// Directories deeper than this are summarized in their ancestor.
    pub keep_depth: usize,
    pub max_nodes: usize,
    /// Largest subdirectories kept per directory; the rest are summarized.
    pub top_children: usize,
    pub max_hardlinks: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 20_000_000,
            max_duration: Duration::from_secs(15 * 60),
            max_depth: 128,
            keep_depth: 8,
            max_nodes: 20_000,
            top_children: 200,
            max_hardlinks: 500_000,
        }
    }
}

/// An invalid home path; `Display` is the stable English text, the UI
/// translates the kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootError(pub HomeErrorKind);

impl std::fmt::Display for RootError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.0 {
            HomeErrorKind::Length => "home path must be 1 to 4096 bytes",
            HomeErrorKind::Relative => "home path must be absolute",
            HomeErrorKind::ControlCharacters => "home path must not contain control characters",
            HomeErrorKind::NotNormalized => {
                "home path must be normalized (no trailing or double slashes)"
            }
            HomeErrorKind::DotComponents => "home path must not contain . or .. components",
            HomeErrorKind::NotAllowed => "home path is not below a helper scan root",
            HomeErrorKind::StartFailed => "cannot start scanner",
            HomeErrorKind::ScanFailed => "cannot scan home path",
        })
    }
}

impl std::error::Error for RootError {}

/// A home root must be an absolute, normalized path without control
/// characters. Without the helper, symlinks in the root itself are followed,
/// as `du` does for its arguments; nothing below it is.
pub fn validate_root(root: &str) -> Result<(), RootError> {
    let fail = |kind| Err(RootError(kind));
    if root.is_empty() || root.len() > MAX_ROOT_BYTES {
        return fail(HomeErrorKind::Length);
    }
    if !root.starts_with('/') {
        return fail(HomeErrorKind::Relative);
    }
    if root.chars().any(char::is_control) {
        return fail(HomeErrorKind::ControlCharacters);
    }
    if root != "/" && (root.ends_with('/') || root.contains("//")) {
        return fail(HomeErrorKind::NotNormalized);
    }
    // Path::components would silently drop "." components; check the text.
    if root
        .split('/')
        .any(|component| component == "." || component == "..")
    {
        return fail(HomeErrorKind::DotComponents);
    }
    Ok(())
}

/// The scan root is outside the helper's allowed roots, or reaches below
/// one through a symlink.
#[derive(Debug)]
pub struct RootNotAllowed;

impl std::fmt::Display for RootNotAllowed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "not below a helper scan root in {SCAN_ROOTS_PATH} (symlinks below it are not followed)"
        )
    }
}

impl std::error::Error for RootNotAllowed {}

fn not_allowed() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, RootNotAllowed)
}

pub fn is_not_allowed(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<RootNotAllowed>())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowedRoot {
    /// As written in the scan-roots file.
    pub configured: PathBuf,
    /// With symlinks resolved when the helper started (/home -> /var/home).
    pub canonical: PathBuf,
}

/// Which scan roots may be opened, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootPolicy {
    /// Without the helper: any root the user can read, opened like `du` does.
    Follow,
    /// In the helper: only roots at or below these, opened component by
    /// component without following any symlink below the allowed root, so
    /// /home/alice/link -> /root cannot widen the helper's reach.
    Allowed(Vec<AllowedRoot>),
}

impl RootPolicy {
    /// The allowed root that `root` lies in, and the components below it.
    fn locate<'a>(&'a self, root: &Path) -> Option<(&'a AllowedRoot, PathBuf)> {
        let Self::Allowed(roots) = self else {
            return None;
        };
        roots
            .iter()
            .flat_map(|allowed| {
                [&allowed.configured, &allowed.canonical]
                    .into_iter()
                    .filter_map(move |base| {
                        root.strip_prefix(base)
                            .ok()
                            .map(|rest| (base.components().count(), allowed, rest.to_owned()))
                    })
            })
            .max_by_key(|(depth, _, _)| *depth)
            .map(|(_, allowed, rest)| (allowed, rest))
    }

    /// Whether the lexical path may be requested at all.
    pub fn allows(&self, root: &Path) -> bool {
        match self {
            Self::Follow => true,
            Self::Allowed(_) => self.locate(root).is_some(),
        }
    }

    pub fn open(&self, root: &Path) -> io::Result<OwnedFd> {
        let Self::Allowed(_) = self else {
            let path = CString::new(root.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
            // SAFETY: path is NUL-terminated; the root itself may be a symlink.
            let descriptor = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: open returned a new descriptor that we now own.
            return Ok(unsafe { OwnedFd::from_raw_fd(descriptor) });
        };
        let (allowed, rest) = self.locate(root).ok_or_else(not_allowed)?;
        // The allowed root was resolved when the helper started; it is
        // walked without following symlinks too, so a later swap of one of
        // its components is refused rather than followed.
        let mut directory = open_component(None, Path::new("/"))?;
        for part in allowed.canonical.components().chain(rest.components()) {
            match part {
                std::path::Component::RootDir => {}
                std::path::Component::Normal(name) => {
                    directory = open_component(Some(&directory), Path::new(name))?;
                }
                _ => return Err(not_allowed()),
            }
        }
        Ok(directory)
    }
}

/// openat(O_NOFOLLOW|O_DIRECTORY); a symlink is reported as not allowed.
fn open_component(parent: Option<&OwnedFd>, name: &Path) -> io::Result<OwnedFd> {
    let name = CString::new(name.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let base = parent.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd);
    // SAFETY: name is NUL-terminated and base is AT_FDCWD or an open descriptor.
    let descriptor = unsafe {
        libc::openat(
            base,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor >= 0 {
        // SAFETY: openat returned a new descriptor that we now own.
        return Ok(unsafe { OwnedFd::from_raw_fd(descriptor) });
    }
    let error = io::Error::last_os_error();
    let symlink =
        stat_at(base, &name).is_ok_and(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFLNK);
    if symlink {
        Err(not_allowed())
    } else {
        Err(error)
    }
}

/// Check every existing component before treating a missing policy as absent.
/// A missing leaf below an unsafe or dangling symlink must not enable defaults.
fn trusted_policy_exists(path: &Path) -> std::result::Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let components: Vec<_> = path
        .ancestors()
        .filter(|part| !part.as_os_str().is_empty())
        .collect();
    for component in components.into_iter().rev() {
        let metadata = match std::fs::symlink_metadata(component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("{}: {error}", component.display())),
        };
        if metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
            || if component == path {
                !metadata.is_file()
            } else {
                !metadata.is_dir()
            }
        {
            return Err(format!(
                "{} must be a root-owned file/directory that others cannot write; \
                 no helper scans are allowed",
                component.display()
            ));
        }
    }
    Ok(true)
}

/// Prefer the current policy; only a safely absent path permits legacy fallback.
pub fn load_scan_roots_with_legacy(
    path: &Path,
    previous: &Path,
    legacy: &Path,
) -> (RootPolicy, Option<String>) {
    for candidate in [path, previous, legacy] {
        match trusted_policy_exists(candidate) {
            Ok(false) => continue,
            Ok(true) => return load_scan_roots_inner(candidate, false),
            Err(problem) => return (RootPolicy::Allowed(Vec::new()), Some(problem)),
        }
    }
    load_scan_roots(legacy)
}

/// The helper's scan roots from `path`. The file and every directory above it
/// must be root-owned regular files/directories that others cannot write;
/// otherwise nothing may be scanned with privileges. Safely missing file: /home.
/// The second value describes a problem with the file, for diagnostics.
pub fn load_scan_roots(path: &Path) -> (RootPolicy, Option<String>) {
    load_scan_roots_inner(path, true)
}

fn load_scan_roots_inner(path: &Path, allow_default: bool) -> (RootPolicy, Option<String>) {
    let parse_roots = |entries: Vec<String>| {
        RootPolicy::Allowed(
            entries
                .into_iter()
                .filter(|entry| validate_root(entry).is_ok())
                .take(MAX_SCAN_ROOTS)
                .map(|entry| {
                    let configured = PathBuf::from(&entry);
                    let canonical =
                        crate::probe::resolve_with_timeout(&configured, Duration::from_secs(1))
                            .unwrap_or_else(|| configured.clone());
                    AllowedRoot {
                        configured,
                        canonical,
                    }
                })
                .collect(),
        )
    };
    match trusted_policy_exists(path) {
        Ok(false) if !allow_default => {
            return (
                RootPolicy::Allowed(Vec::new()),
                Some(format!(
                    "{} disappeared while loading; no helper scans are allowed",
                    path.display()
                )),
            );
        }
        Ok(false) => {
            return (
                parse_roots(
                    DEFAULT_SCAN_ROOTS
                        .iter()
                        .map(|root| (*root).to_owned())
                        .collect(),
                ),
                None,
            );
        }
        Err(problem) => return (RootPolicy::Allowed(Vec::new()), Some(problem)),
        Ok(true) => {}
    }
    match read_scan_roots(path) {
        Ok(entries) => (parse_roots(entries), None),
        Err(error) => (
            RootPolicy::Allowed(Vec::new()),
            Some(format!("{}: {error}", path.display())),
        ),
    }
}

fn read_scan_roots(path: &Path) -> io::Result<Vec<String>> {
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path)?;
    io::Read::read_to_end(
        &mut io::Read::take(file, MAX_POLICY_BYTES as u64 + 1),
        &mut bytes,
    )?;
    // A partial final path can broaden an allowlist (/srv/private -> /).
    // Reject the entire policy, including legacy policies, rather than
    // parsing any prefix of an overlong file.
    if bytes.len() > MAX_POLICY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "scan-roots exceeds 64 KiB; no helper scans are allowed",
        ));
    }
    let contents = String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "scan-roots must be UTF-8; no helper scans are allowed",
        )
    })?;
    let entries: Vec<String> = contents
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect();
    if entries.len() > MAX_SCAN_ROOTS || entries.iter().any(|entry| validate_root(entry).is_err()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "scan-roots contains invalid paths or too many roots; no helper scans are allowed",
        ));
    }
    Ok(entries)
}

/// Sanitize a directory name for display and bound its length in
/// characters, marking a cut with "~".
fn display_name(name: &[u8]) -> String {
    let name = sanitize_bytes(name);
    if name.chars().count() <= MAX_NAME_CHARS {
        return name;
    }
    let mut out: String = name.chars().take(MAX_NAME_CHARS - 1).collect();
    out.push('~');
    out
}

fn name_key(name: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut key = String::with_capacity(name.len() * 2);
    for byte in name {
        key.push(char::from(HEX[usize::from(byte >> 4)]));
        key.push(char::from(HEX[usize::from(byte & 15)]));
    }
    key
}

#[derive(Default, Clone, Copy)]
struct Totals {
    allocated: u64,
    apparent: u64,
    files: u64,
    dirs: u64,
    unreadable: u64,
    truncated: bool,
}

impl Totals {
    fn of(node: &DirNode) -> Self {
        Self {
            allocated: node.allocated,
            apparent: node.apparent,
            files: node.files,
            dirs: node.dirs,
            ..Self::default()
        }
    }

    fn add(&mut self, other: &Self) {
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
struct Slot {
    node: DirNode,
    parent: Option<usize>,
    depth: usize,
    finished: bool,
}

#[derive(Default)]
struct Tree {
    slots: Vec<Option<Slot>>,
    free: Vec<usize>,
    live: usize,
}

impl Tree {
    fn alloc(&mut self, node: DirNode, parent: Option<usize>, depth: usize) -> usize {
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

    fn slot(&self, index: usize) -> &Slot {
        self.slots[index].as_ref().expect("live slot")
    }

    fn node(&mut self, index: usize) -> &mut DirNode {
        &mut self.slots[index].as_mut().expect("live slot").node
    }

    fn release_subtree(&mut self, index: usize) {
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
    fn evict_to(&mut self, target: usize) {
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
    fn into_nodes(mut self) -> Vec<DirNode> {
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

fn stat_at(directory: libc::c_int, name: &CStr) -> io::Result<libc::stat> {
    // SAFETY: zeroed stat is a valid out-parameter; name is NUL-terminated.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            directory,
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

fn stat_fd(descriptor: libc::c_int) -> io::Result<libc::stat> {
    // SAFETY: zeroed stat is a valid out-parameter for fstat.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(descriptor, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
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

fn fold(other: &mut Folded, totals: &Totals) {
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::{
            ffi::OsStringExt,
            fs::{PermissionsExt, symlink},
        },
    };

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(label: &str) -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "dwtop-scan-{label}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            // Restore permissions so cleanup can descend everywhere.
            let _ = fs::set_permissions(
                self.0.join("alice/private"),
                fs::Permissions::from_mode(0o755),
            );
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_scan(root: &Path, limits: &Limits) -> HomeReport {
        scan(root, limits, &AtomicBool::new(false), &AtomicU64::new(0)).unwrap()
    }

    fn wait_report(service: &HomeService, known: u64) -> HomeReport {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(report) = service.report_if_newer(known) {
                return report;
            }
            assert!(Instant::now() < deadline, "scan did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn child<'a>(report: &'a HomeReport, parent: u32, name: &str) -> &'a DirNode {
        let node = report.node(parent).unwrap();
        node.children
            .iter()
            .map(|index| report.node(*index).unwrap())
            .find(|node| node.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
    }

    fn running_as_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn sizes_hardlinks_symlinks_and_unreadable_directories() {
        let tree = TempTree::new("basic");
        let root = &tree.0;
        fs::create_dir_all(root.join("alice/docs/deep")).unwrap();
        fs::create_dir_all(root.join("bob")).unwrap();
        fs::write(root.join("alice/docs/a.bin"), vec![1u8; 64 * 1024]).unwrap();
        fs::write(root.join("alice/docs/deep/b.bin"), vec![2u8; 32 * 1024]).unwrap();
        fs::write(root.join("bob/c.bin"), vec![3u8; 16 * 1024]).unwrap();
        // A hard link to the same inode is counted once.
        fs::hard_link(root.join("bob/c.bin"), root.join("alice/c-link.bin")).unwrap();
        // Symlinks are counted as entries but never followed.
        let outside = TempTree::new("outside");
        fs::write(outside.0.join("huge.bin"), vec![4u8; 256 * 1024]).unwrap();
        symlink(&outside.0, root.join("alice/escape")).unwrap();
        // Non-UTF-8 and control characters are displayed safely.
        fs::create_dir(root.join(std::ffi::OsStr::from_bytes(b"bad\xffname\n"))).unwrap();
        fs::create_dir_all(root.join("alice/private/hidden")).unwrap();
        fs::write(root.join("alice/private/hidden/secret"), vec![5u8; 8192]).unwrap();
        fs::set_permissions(
            root.join("alice/private"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let report = run_scan(root, &Limits::default());
        let top = report.root_node().unwrap();
        assert_eq!(report.stopped, None);
        let alice = child(&report, 0, "alice");
        let bob = child(&report, 0, "bob");
        let docs = child(&report, report.resolve(&[name_key(b"alice")]).0, "docs");
        assert!(docs.allocated >= 96 * 1024, "{}", docs.allocated);
        assert_eq!(
            docs.apparent,
            96 * 1024
                + fs::metadata(root.join("alice/docs")).unwrap().len()
                + fs::metadata(root.join("alice/docs/deep")).unwrap().len()
        );
        assert_eq!(docs.files, 2);
        assert_eq!(docs.dirs, 1);
        // Exactly one of the two links carries the 16 KiB.
        let linked = alice.own_files.apparent + bob.own_files.apparent;
        assert_eq!(
            linked,
            16 * 1024
                + fs::symlink_metadata(root.join("alice/escape"))
                    .unwrap()
                    .len()
        );
        assert!(top.allocated < 256 * 1024, "symlink was followed");
        assert!(
            report
                .nodes
                .iter()
                .any(|node| node.name == "bad\\xffname\\x0a"),
            "non-UTF-8 name"
        );
        if !running_as_root() {
            let private = child(&report, report.resolve(&[name_key(b"alice")]).0, "private");
            assert_eq!(private.unreadable, 1);
            assert!(private.partial() && alice.partial() && top.partial());
            assert!(!bob.partial());
        }
        // Display names and stable navigation identities are separate.
        assert_eq!(
            report
                .resolve(&[name_key(b"alice"), name_key(b"docs"), name_key(b"deep")])
                .1,
            3
        );
        assert_eq!(
            report.resolve(&[name_key(b"alice"), name_key(b"gone")]).1,
            1
        );
    }

    #[test]
    fn budgets_depth_and_node_limits_bound_the_result() {
        let tree = TempTree::new("bounds");
        let root = &tree.0;
        for index in 0..30 {
            let directory = root.join(format!("d{index:02}"));
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("f"), vec![0u8; 4096 * (index + 1)]).unwrap();
        }
        let mut deep = root.join("d29");
        for level in 0..12 {
            deep = deep.join(format!("l{level}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deepest"), vec![0u8; 4096]).unwrap();

        let limits = Limits {
            top_children: 5,
            keep_depth: 3,
            max_depth: 6,
            ..Limits::default()
        };
        let report = run_scan(root, &limits);
        let top = report.root_node().unwrap();
        assert_eq!(top.children.len(), 5);
        // The largest directories are kept, sorted by allocated size.
        assert_eq!(report.node(top.children[0]).unwrap().name, "d29");
        assert_eq!(top.other.dirs, 25);
        let kept: u64 = top
            .children
            .iter()
            .map(|child| report.node(*child).unwrap().allocated)
            .sum();
        assert!(kept + top.other.allocated <= top.allocated);
        assert!(top.truncated, "max_depth must mark the subtree truncated");
        assert!(report.nodes.len() <= 5 + 1 + 3, "{}", report.nodes.len());
        let (_, depth) = report.resolve(&[
            name_key(b"d29"),
            name_key(b"l0"),
            name_key(b"l1"),
            name_key(b"l2"),
        ]);
        assert_eq!(depth, 3, "keep_depth limits retained nodes");

        let report = run_scan(
            root,
            &Limits {
                max_entries: 10,
                ..Limits::default()
            },
        );
        assert_eq!(report.stopped, Some(StopReason::EntryBudget));
        assert_eq!(report.entries, 10);
        let cancelled = scan(
            root,
            &Limits::default(),
            &AtomicBool::new(true),
            &AtomicU64::new(0),
        )
        .unwrap();
        // Cancellation is checked periodically; small trees may finish first.
        assert!(matches!(
            cancelled.stopped,
            None | Some(StopReason::Cancelled)
        ));
        let report = run_scan(
            root,
            &Limits {
                max_nodes: 3,
                ..Limits::default()
            },
        );
        assert!(report.nodes.len() <= 3);
        assert!(report.root_node().unwrap().other.dirs >= 28);
    }

    #[test]
    fn node_budget_prefers_shallow_and_large_directories_over_visit_order() {
        let tree = TempTree::new("budget");
        let root = &tree.0;
        let mut deep = root.join("deep");
        for level in 0..20 {
            deep = deep.join(format!("l{level}"));
            fs::create_dir_all(&deep).unwrap();
            fs::create_dir(deep.with_file_name(format!("side{level}"))).unwrap();
        }
        for (index, size) in [(1, 64), (2, 128), (3, 256)] {
            let directory = root.join(format!("big{index}"));
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("data"), vec![1u8; size * 1024]).unwrap();
        }
        let report = run_scan(
            root,
            &Limits {
                max_nodes: 6,
                ..Limits::default()
            },
        );
        assert!(report.nodes.len() <= 6, "{}", report.nodes.len());
        let top: Vec<_> = report
            .root_node()
            .unwrap()
            .children
            .iter()
            .map(|index| report.node(*index).unwrap().name.as_str())
            .collect();
        for name in ["big3", "big2", "big1", "deep"] {
            assert!(top.contains(&name), "{name} missing from {top:?}");
        }
        // Everything not retained is still counted in the parents' totals.
        let deep_node = child(&report, 0, "deep");
        assert_eq!(deep_node.dirs, 40);
        let kept: u64 = report.nodes.iter().skip(1).map(|node| node.files).sum();
        assert!(kept <= report.root_node().unwrap().files);
    }

    #[test]
    fn sibling_homes_keep_details_despite_same_depth_flood_in_either_order() {
        // Drive the slab in an explicit DFS order: filesystem readdir order
        // cannot reliably be controlled by creating directories in order.
        let mut results = Vec::new();
        for order in [["alice", "bob", "carol"], ["carol", "bob", "alice"]] {
            let limits = Limits {
                max_nodes: 13,
                keep_depth: 8,
                ..Limits::default()
            };
            let mut tree = Tree::default();
            tree.alloc(DirNode::default(), None, 0);
            let cap = limits.max_nodes * 2 + limits.keep_depth + 1;
            let mut total = 0;
            let mut files = 0;
            for name in order {
                let home = tree.alloc(
                    DirNode {
                        name: name.into(),
                        ..DirNode::default()
                    },
                    Some(0),
                    1,
                );
                let count = if name == "alice" { 1000 } else { 3 };
                let mut home_total = 0;
                for number in 1..=count {
                    if tree.live >= cap {
                        tree.evict_to(limits.max_nodes + limits.keep_depth);
                    }
                    let bytes = if name == "alice" {
                        number * 1000
                    } else {
                        number
                    };
                    let child = tree.alloc(
                        DirNode {
                            name: format!("d{number:04}"),
                            allocated: bytes,
                            apparent: bytes,
                            files: 1,
                            own_files: Folded {
                                allocated: bytes,
                                apparent: bytes,
                                files: 1,
                                dirs: 0,
                            },
                            ..DirNode::default()
                        },
                        Some(home),
                        2,
                    );
                    tree.slots[child].as_mut().unwrap().finished = true;
                    tree.node(home).children.push(child as u32);
                    home_total += bytes;
                    assert!(tree.live <= cap);
                    assert!(tree.slots.len() <= cap);
                }
                let slot = tree.slots[home].as_mut().unwrap();
                slot.finished = true;
                slot.node.allocated = home_total;
                slot.node.apparent = home_total;
                slot.node.files = count;
                slot.node.dirs = count;
                tree.node(0).children.push(home as u32);
                total += home_total;
                files += count;
            }
            tree.node(0).allocated = total;
            tree.node(0).files = files;
            tree.evict_to(limits.max_nodes);
            assert_eq!(tree.live, limits.max_nodes);
            let nodes = tree.into_nodes();
            assert_eq!(
                nodes.len(),
                limits.max_nodes,
                "all retained nodes must be reachable"
            );
            assert_eq!(nodes[0].allocated, total);
            assert_eq!(nodes[0].files, files);
            let mut retained = Vec::new();
            for index in &nodes[0].children {
                let home = &nodes[*index as usize];
                assert_eq!(home.children.len(), 3, "{} lost its fair share", home.name);
                let children: Vec<_> = home
                    .children
                    .iter()
                    .map(|index| &nodes[*index as usize])
                    .collect();
                assert_eq!(
                    home.allocated,
                    home.other.allocated + children.iter().map(|node| node.allocated).sum::<u64>()
                );
                assert_eq!(
                    home.apparent,
                    home.other.apparent + children.iter().map(|node| node.apparent).sum::<u64>()
                );
                assert_eq!(
                    home.files,
                    home.other.files + children.iter().map(|node| node.files).sum::<u64>()
                );
                assert_eq!(home.dirs, home.other.dirs + children.len() as u64);
                if home.name == "alice" {
                    assert_eq!(home.other.dirs, 997, "omitted detail must remain visible");
                } else {
                    assert_eq!(home.other.dirs, 0);
                }
                retained.extend(
                    children
                        .iter()
                        .map(|node| format!("{}/{}", home.name, node.name)),
                );
            }
            retained.sort();
            results.push(retained);
        }
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn active_deep_descent_cannot_spend_finished_neighbours_shares() {
        let mut tree = Tree::default();
        tree.alloc(DirNode::default(), None, 0);
        for name in ["bob", "carol"] {
            let home = tree.alloc(
                DirNode {
                    name: name.into(),
                    ..DirNode::default()
                },
                Some(0),
                1,
            );
            let detail = tree.alloc(
                DirNode {
                    name: "detail".into(),
                    ..DirNode::default()
                },
                Some(home),
                2,
            );
            tree.node(home).children.push(detail as u32);
            tree.node(0).children.push(home as u32);
            tree.slots[detail].as_mut().unwrap().finished = true;
            tree.slots[home].as_mut().unwrap().finished = true;
        }
        let mut ancestors = Vec::new();
        let mut parent = 0;
        for depth in 1..=7 {
            let index = tree.alloc(
                DirNode {
                    name: format!("active{depth}"),
                    ..DirNode::default()
                },
                Some(parent),
                depth,
            );
            // Active nodes are attached only after finishing, as in Walker.
            ancestors.push(index);
            parent = index;
        }
        for number in 0..50 {
            if tree.live > 7 * 2 + 8 {
                tree.evict_to(7 + 8);
                assert_eq!(tree.slot(1).node.children.len(), 1);
                assert_eq!(tree.slot(3).node.children.len(), 1);
            }
            let index = tree.alloc(
                DirNode {
                    name: format!("leaf{number}"),
                    ..DirNode::default()
                },
                Some(parent),
                8,
            );
            tree.node(parent).children.push(index as u32);
            tree.slots[index].as_mut().unwrap().finished = true;
        }
        for index in ancestors.into_iter().rev() {
            let parent = tree.slot(index).parent.unwrap();
            tree.slots[index].as_mut().unwrap().finished = true;
            tree.node(parent).children.push(index as u32);
        }
        tree.evict_to(7);
        assert_eq!(tree.live, 7);
        let nodes = tree.into_nodes();
        assert_eq!(nodes.len(), 7);
        for home in nodes[0]
            .children
            .iter()
            .map(|index| &nodes[*index as usize])
        {
            assert_eq!(home.children.len(), 1, "{} lost its detail", home.name);
        }
    }

    #[test]
    fn fair_eviction_preserves_ancestors_and_full_scan_totals() {
        let tree = TempTree::new("fair-budget");
        for (user, count, size) in [("alice", 80, 8192), ("bob", 3, 1), ("carol", 3, 1)] {
            for index in 0..count {
                let directory = tree.0.join(format!("{user}/d{index:03}/nested/deep"));
                fs::create_dir_all(&directory).unwrap();
                fs::write(directory.join("data"), vec![1u8; size]).unwrap();
            }
        }
        let full = run_scan(&tree.0, &Limits::default());
        for top_children in [3, 200] {
            for max_nodes in [1, 3, 16] {
                let bounded = run_scan(
                    &tree.0,
                    &Limits {
                        max_nodes,
                        top_children,
                        ..Limits::default()
                    },
                );
                assert_eq!(bounded.stopped, None);
                assert_eq!(bounded.nodes.len(), max_nodes);
                if max_nodes == 16 {
                    for user in ["alice", "bob", "carol"] {
                        assert!(
                            !child(&bounded, 0, user).children.is_empty(),
                            "{user} lost all details"
                        );
                    }
                }
                let mut queue = vec![(0, 0)];
                let mut seen = HashSet::new();
                while let Some((small_index, full_index)) = queue.pop() {
                    assert!(seen.insert(small_index));
                    let small = bounded.node(small_index).unwrap();
                    let original = full.node(full_index).unwrap();
                    assert_eq!(small.allocated, original.allocated);
                    assert_eq!(small.apparent, original.apparent);
                    assert_eq!(small.files, original.files);
                    assert_eq!(small.dirs, original.dirs);
                    assert_eq!(small.truncated, original.truncated);
                    let mut omitted = Folded::default();
                    for original_child in &original.children {
                        let original_node = full.node(*original_child).unwrap();
                        match small
                            .children
                            .iter()
                            .find(|index| bounded.node(**index).unwrap().name == original_node.name)
                        {
                            Some(index) => queue.push((*index, *original_child)),
                            None => fold(&mut omitted, &Totals::of(original_node)),
                        }
                    }
                    assert_eq!(small.other.allocated, omitted.allocated);
                    assert_eq!(small.other.apparent, omitted.apparent);
                    assert_eq!(small.other.files, omitted.files);
                    assert_eq!(small.other.dirs, omitted.dirs);
                }
                assert_eq!(seen.len(), bounded.nodes.len(), "ancestor closure");
                assert!(bounded.nodes.iter().any(|node| node.other.dirs > 0));
            }
        }
    }

    #[test]
    fn hardlink_limit_is_reported_and_other_filesystems_are_skipped() {
        let tree = TempTree::new("links");
        let root = &tree.0;
        for index in 0..4 {
            fs::write(root.join(format!("f{index}")), vec![0u8; 4096]).unwrap();
            fs::hard_link(
                root.join(format!("f{index}")),
                root.join(format!("g{index}")),
            )
            .unwrap();
        }
        let report = run_scan(
            root,
            &Limits {
                max_hardlinks: 2,
                ..Limits::default()
            },
        );
        assert!(report.hardlink_limit_reached);
        // /dev usually contains separate mounts (pts, shm, mqueue).
        let dev = run_scan(Path::new("/dev"), &Limits::default());
        let pts_is_mount = fs::metadata("/dev/pts").is_ok_and(|pts| {
            use std::os::unix::fs::MetadataExt;
            fs::metadata("/dev").is_ok_and(|dev| dev.dev() != pts.dev())
        });
        if pts_is_mount {
            assert!(dev.other_filesystems >= 1);
        }
        assert!(
            scan(
                Path::new("/nonexistent-dwtop"),
                &Limits::default(),
                &AtomicBool::new(false),
                &AtomicU64::new(0)
            )
            .is_err()
        );
    }

    #[test]
    fn helper_policy_allows_only_listed_roots_and_never_follows_symlinks_below() {
        let tree = TempTree::new("policy");
        let allowed = tree.0.join("home");
        fs::create_dir_all(allowed.join("alice/docs")).unwrap();
        let outside = TempTree::new("policy-outside");
        // Large enough to stand out from directory blocks on any filesystem.
        fs::write(outside.0.join("secret.bin"), vec![1u8; 1 << 20]).unwrap();
        symlink(&outside.0, allowed.join("alice/link")).unwrap();
        // A symlinked allowed root (/home -> /var/home) is resolved once.
        symlink(&allowed, tree.0.join("home-link")).unwrap();
        let policy = RootPolicy::Allowed(vec![AllowedRoot {
            configured: tree.0.join("home-link"),
            canonical: fs::canonicalize(&allowed).unwrap(),
        }]);
        for path in [
            tree.0.join("home-link"),
            tree.0.join("home-link/alice"),
            allowed.join("alice/docs"),
        ] {
            assert!(policy.allows(&path), "{}", path.display());
            assert!(policy.open(&path).is_ok(), "{}", path.display());
        }
        let report = scan_with(
            &tree.0.join("home-link/alice"),
            &policy,
            &Limits::default(),
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap();
        assert!(report.root_node().unwrap().allocated < 1 << 19);
        // Outside the list, and a symlink below an allowed root.
        assert!(!policy.allows(&outside.0));
        assert!(!policy.allows(&tree.0.join("home2")));
        let error = policy.open(&outside.0).unwrap_err();
        assert!(is_not_allowed(&error), "{error}");
        let escape = allowed.join("alice/link");
        assert!(policy.allows(&escape), "lexically below the root");
        let error = policy.open(&escape).unwrap_err();
        assert!(is_not_allowed(&error), "{error}");
        let error = policy.open(&allowed.join("alice/missing")).unwrap_err();
        assert!(!is_not_allowed(&error) && error.kind() == io::ErrorKind::NotFound);
        // Without the helper, the root itself may be a symlink, like du.
        assert!(RootPolicy::Follow.open(&escape).is_ok());
    }

    #[test]
    fn scan_roots_default_to_home_and_unsafe_files_allow_nothing() {
        let tree = TempTree::new("roots");
        // / is trusted even when tests run as an ordinary user. /tmp and the
        // user-owned fixture are deliberately not trusted policy ancestors.
        let missing = PathBuf::from(format!(
            "/nonexistent-dwtop-policy-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!missing.exists());
        let (policy, problem) = load_scan_roots(&missing.join("scan-roots"));
        assert!(problem.is_none());
        let RootPolicy::Allowed(roots) = &policy else {
            panic!("helper policy expected");
        };
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].configured, Path::new("/home"));
        assert!(policy.allows(Path::new("/home/alice")));
        assert!(!policy.allows(Path::new("/root")) && !policy.allows(Path::new("/")));
        // A file the test user owns is not trusted: nothing may be scanned.
        let file = tree.0.join("scan-roots");
        fs::write(&file, "/srv\n").unwrap();
        let (policy, problem) = load_scan_roots(&file);
        if !running_as_root() {
            assert_eq!(policy, RootPolicy::Allowed(Vec::new()));
            assert!(problem.unwrap().contains("root-owned"));
            assert!(!policy.allows(Path::new("/srv")) && !policy.allows(Path::new("/home")));
        }
    }

    #[test]
    fn policy_migration_never_uses_defaults_below_unsafe_ancestors() {
        let tree = TempTree::new("policy-migration");
        let missing = PathBuf::from(format!(
            "/nonexistent-dwtop-migration-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let current = missing.join("dwtop/scan-roots");
        let legacy = missing.join("disktop/scan-roots");
        let (policy, problem) = load_scan_roots_with_legacy(&current, &legacy, &legacy);
        assert!(problem.is_none());
        assert!(policy.allows(Path::new("/home/alice")));
        assert!(!policy.allows(Path::new("/root")));

        // Missing policy below a writable/user-owned parent is an error,
        // whether it is the current policy or the legacy fallback.
        let unsafe_missing = tree.0.join("missing/scan-roots");
        for (new, old) in [(&unsafe_missing, &legacy), (&current, &unsafe_missing)] {
            let (policy, problem) = load_scan_roots_with_legacy(new, old, old);
            assert_eq!(policy, RootPolicy::Allowed(Vec::new()));
            assert!(problem.unwrap().contains("root-owned"));
        }

        // /proc/self is a root-owned symlink under trusted /proc. Looking only
        // at the missing leaf would miss it and incorrectly enable /home.
        let symlink_parent = Path::new("/proc/self/nonexistent-dwtop-policy/scan-roots");
        for (new, old) in [
            (symlink_parent, legacy.as_path()),
            (current.as_path(), symlink_parent),
        ] {
            let (policy, problem) = load_scan_roots_with_legacy(new, old, old);
            assert_eq!(policy, RootPolicy::Allowed(Vec::new()));
            assert!(problem.unwrap().contains("/proc/self"));
        }
    }

    #[test]
    fn long_names_are_capped_and_aborted_scans_never_set_growth() {
        let long = vec![0xffu8; 255];
        let name = display_name(&long);
        assert_eq!(name.chars().count(), MAX_NAME_CHARS);
        assert!(name.ends_with('~'));
        assert_eq!(display_name(b"short"), "short");

        let tree = TempTree::new("aborted");
        for index in 0..40 {
            fs::create_dir(tree.0.join(format!("d{index}"))).unwrap();
        }
        let wait = |service: &HomeService, known: u64| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(report) = service.report_if_newer(known) {
                    return report;
                }
                assert!(Instant::now() < deadline, "scan did not finish");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        // The first scan hits the entry budget: it is no growth baseline.
        let service = HomeService::start(
            tree.0.clone(),
            Duration::from_secs(3600),
            Limits {
                max_entries: 10,
                ..Limits::default()
            },
            false,
            RootPolicy::Follow,
            || Ok(()),
        )
        .unwrap();
        let first = wait(&service, 0);
        assert_eq!(first.stopped, Some(StopReason::EntryBudget));
        assert!(first.nodes.iter().all(|node| node.growth_start.is_none()));
        service.rescan();
        let second = wait(&service, 1);
        assert!(
            second
                .nodes
                .iter()
                .all(|node| node.growth_previous.is_none())
        );
        drop(service);
        // After a complete scan, an aborted one is not annotated either.
        let service = HomeService::start(
            tree.0.clone(),
            Duration::from_secs(3600),
            Limits::default(),
            false,
            RootPolicy::Follow,
            || Ok(()),
        )
        .unwrap();
        let complete = wait(&service, 0);
        assert_eq!(complete.stopped, None);
        service.rescan();
        let again = wait(&service, 1);
        assert_eq!(again.root_node().unwrap().growth_previous, Some(0));
    }

    #[test]
    fn policy_limit_rejects_partial_paths_and_accepts_exact_bound() {
        let tree = TempTree::new("policy-limit");
        // Both current and legacy policies use this reader. A long comment
        // puts the limit exactly after the slash of the otherwise narrow root.
        for filename in ["scan-roots", "legacy-scan-roots"] {
            let path = tree.0.join(filename);
            let dangerous = format!("#{}\n/srv/private\n", "x".repeat(MAX_POLICY_BYTES - 3));
            assert_eq!(&dangerous.as_bytes()[MAX_POLICY_BYTES - 1..][..1], b"/");
            fs::write(&path, dangerous).unwrap();
            let error = read_scan_roots(&path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("no helper scans are allowed"));

            let accepted = format!("#{}\n/home", "x".repeat(MAX_POLICY_BYTES - 7));
            assert_eq!(accepted.len(), MAX_POLICY_BYTES);
            fs::write(&path, accepted).unwrap();
            assert_eq!(read_scan_roots(&path).unwrap(), ["/home"]);
        }
    }

    #[test]
    fn malformed_policy_rejects_the_entire_allowlist() {
        let tree = TempTree::new("invalid-policy");
        let path = tree.0.join("scan-roots");
        for contents in [
            b"/home\nrelative\n".to_vec(),
            b"/home\n/srv/../etc\n".to_vec(),
            b"/home\n\xff\n".to_vec(),
            "/home\n".repeat(MAX_SCAN_ROOTS + 1).into_bytes(),
        ] {
            fs::write(&path, contents).unwrap();
            assert_eq!(
                read_scan_roots(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        fs::write(&path, "# no privileged roots\n").unwrap();
        assert!(read_scan_roots(&path).unwrap().is_empty());
    }

    #[test]
    fn disappearing_selected_policy_never_enables_defaults() {
        let path = PathBuf::from(format!(
            "/dwtop-policy-missing-{}/scan-roots",
            std::process::id()
        ));
        let (policy, error) = load_scan_roots_inner(&path, false);
        assert!(!policy.allows(Path::new("/home")));
        assert!(error.unwrap().contains("disappeared"));
    }

    #[test]
    fn colliding_display_names_keep_navigation_and_growth_distinct() {
        let tree = TempTree::new("name-identity");
        let mut names = Vec::new();
        for prefix in [vec![0xff; 64], b"\\xff".repeat(60)] {
            for suffix in *b"ab" {
                let mut name = prefix.clone();
                name.push(suffix);
                let path = tree.0.join(std::ffi::OsString::from_vec(name.clone()));
                fs::create_dir(&path).unwrap();
                fs::create_dir(path.join(format!("child-{}", char::from(suffix)))).unwrap();
                fs::write(
                    path.join("data"),
                    vec![0u8; if suffix == b'a' { 8192 } else { 65536 }],
                )
                .unwrap();
                names.push(name);
            }
        }
        for pair in names.as_chunks::<2>().0 {
            assert_eq!(display_name(&pair[0]), display_name(&pair[1]));
            assert_ne!(name_key(&pair[0]), name_key(&pair[1]));
        }
        assert_eq!(name_key(&vec![0xff; 255]).len(), 510);
        let service = HomeService::start(
            tree.0.clone(),
            Duration::from_secs(3600),
            Limits::default(),
            false,
            RootPolicy::Follow,
            || Ok(()),
        )
        .unwrap();
        let first = wait_report(&service, 0);
        for name in &names {
            let key = name_key(name);
            let (index, depth) = first.resolve(std::slice::from_ref(&key));
            assert_eq!(depth, 1);
            let node = first.node(index).unwrap();
            assert_eq!(node.identity(), key);
            assert_eq!(node.name, display_name(name));
            let leaf = format!("child-{}", char::from(*name.last().unwrap()));
            let (leaf_index, depth) = first.resolve(&[key, name_key(leaf.as_bytes())]);
            assert_eq!(depth, 2);
            assert_eq!(first.node(leaf_index).unwrap().name, leaf);
        }
        service.rescan();
        let second = wait_report(&service, first.generation);
        for node in &second.nodes {
            assert_eq!(node.growth_previous, Some(0), "{}", node.identity());
            assert_eq!(node.growth_start, Some(0));
        }
    }

    #[test]
    fn unreadable_scan_does_not_replace_complete_growth_baseline() {
        if running_as_root() {
            return; // Root bypasses the permissions needed for this fixture.
        }
        let tree = TempTree::new("partial-growth");
        let private = tree.0.join("alice/private");
        fs::create_dir_all(&private).unwrap();
        fs::write(private.join("data"), vec![0u8; 128 * 1024]).unwrap();
        let service = HomeService::start(
            tree.0.clone(),
            Duration::from_secs(3600),
            Limits::default(),
            false,
            RootPolicy::Follow,
            || Ok(()),
        )
        .unwrap();
        let first = wait_report(&service, 0);
        assert!(!first.root_node().unwrap().partial());
        fs::set_permissions(&private, fs::Permissions::from_mode(0o0)).unwrap();
        service.rescan();
        let partial = wait_report(&service, first.generation);
        assert_eq!(partial.stopped, None);
        assert!(partial.root_node().unwrap().partial());
        assert!(
            partial
                .nodes
                .iter()
                .all(|node| node.growth_previous.is_none() && node.growth_start.is_none())
        );
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        service.rescan();
        let recovered = wait_report(&service, partial.generation);
        assert!(!recovered.root_node().unwrap().partial());
        assert_eq!(
            recovered.root_node().unwrap().allocated,
            first.root_node().unwrap().allocated
        );
        assert_eq!(recovered.root_node().unwrap().growth_previous, Some(0));
        assert_eq!(recovered.root_node().unwrap().growth_start, Some(0));
    }

    #[test]
    fn depth_and_hardlink_limits_exclude_growth_but_retention_folding_does_not() {
        let tree = TempTree::new("growth-completeness");
        fs::create_dir_all(tree.0.join("deep/nested")).unwrap();
        fs::write(tree.0.join("deep/nested/data"), vec![0u8; 8192]).unwrap();
        fs::hard_link(tree.0.join("deep/nested/data"), tree.0.join("linked")).unwrap();
        for (limits, incomplete) in [
            (
                Limits {
                    max_depth: 1,
                    ..Limits::default()
                },
                true,
            ),
            (
                Limits {
                    max_hardlinks: 0,
                    ..Limits::default()
                },
                true,
            ),
            (
                Limits {
                    max_nodes: 1,
                    ..Limits::default()
                },
                false,
            ),
        ] {
            let service = HomeService::start(
                tree.0.clone(),
                Duration::from_secs(3600),
                limits,
                false,
                RootPolicy::Follow,
                || Ok(()),
            )
            .unwrap();
            let first = wait_report(&service, 0);
            assert_eq!(first.stopped, None);
            assert_eq!(
                first.root_node().unwrap().partial() || first.hardlink_limit_reached,
                incomplete
            );
            service.rescan();
            let second = wait_report(&service, first.generation);
            if incomplete {
                assert!(
                    second
                        .nodes
                        .iter()
                        .all(|node| node.growth_previous.is_none() && node.growth_start.is_none())
                );
            } else {
                assert!(second.root_node().unwrap().other.dirs > 0);
                assert_eq!(second.root_node().unwrap().growth_previous, Some(0));
                assert_eq!(second.root_node().unwrap().growth_start, Some(0));
            }
        }
    }

    #[test]
    fn root_validation() {
        for valid in ["/", "/home", "/srv/home dirs", "/home/Grüße"] {
            assert!(validate_root(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "home",
            "/home/",
            "//home",
            "/home/../etc",
            "/home/./x",
            "/a\nb",
        ] {
            assert!(validate_root(invalid).is_err(), "{invalid:?}");
        }
        assert!(validate_root(&format!("/{}", "a".repeat(MAX_ROOT_BYTES))).is_err());
    }

    #[test]
    fn service_scans_reports_growth_and_follows_reconfiguration() {
        let tree = TempTree::new("service");
        fs::create_dir(tree.0.join("grow")).unwrap();
        let service = HomeService::start(
            tree.0.clone(),
            Duration::from_secs(3600),
            Limits::default(),
            false,
            RootPolicy::Follow,
            || Ok(()),
        )
        .unwrap();
        let wait = |known: u64| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(report) = service.report_if_newer(known) {
                    return report;
                }
                assert!(Instant::now() < deadline, "scan did not finish");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let first = wait(0);
        assert_eq!(first.generation, 1);
        assert_eq!(first.root_node().unwrap().growth_previous, None);
        assert_eq!(service.status().state, ScanState::Done);
        assert!(service.report_if_newer(1).is_none());
        fs::write(tree.0.join("grow/new.bin"), vec![7u8; 128 * 1024]).unwrap();
        service.rescan();
        let second = wait(1);
        let grow = child(&second, 0, "grow");
        assert!(grow.growth_previous.unwrap() >= 128 * 1024);
        assert_eq!(grow.growth_previous, grow.growth_start);
        let other = TempTree::new("service-other");
        service.configure(other.0.clone(), Duration::from_secs(3600));
        let third = wait(2);
        assert_eq!(third.root, sanitize_bytes(other.0.as_os_str().as_bytes()));
        assert_eq!(
            third.root_node().unwrap().growth_previous,
            None,
            "new root resets growth"
        );
        service.configure(
            PathBuf::from("/nonexistent-dwtop"),
            Duration::from_secs(3600),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while service.status().state != ScanState::Failed {
            assert!(
                Instant::now() < deadline,
                "missing root did not fail visibly"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            service
                .status()
                .error
                .unwrap()
                .contains("nonexistent-dwtop")
        );
    }
}
