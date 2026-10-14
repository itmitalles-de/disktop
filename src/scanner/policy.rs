//! Validate and securely open permitted scan roots and policy files.

use super::fs::{stat_at, stat_fd};
use super::{
    DEFAULT_SCAN_ROOTS, MAX_POLICY_BYTES, MAX_ROOT_BYTES, MAX_SCAN_ROOTS, SCAN_ROOTS_PATH,
};
use crate::model::HomeErrorKind;
use anyhow::Result;
use std::collections::VecDeque;
use std::ffi::{CString, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// Resolve administrator-owned aliases (for example /home -> /var/home)
/// without letting symlinks under user-writable ancestors enlarge the policy.
/// Each lookup is relative to an opened directory, and symlink targets are
/// read only while every ancestor is root-owned and not group/world-writable.
pub(super) fn resolve_trusted_root(root: &Path) -> io::Result<PathBuf> {
    let parts = |path: &Path| -> VecDeque<OsString> {
        path.components()
            .map(|part| part.as_os_str().to_owned())
            .collect()
    };
    let mut pending = parts(root);
    let mut directory = open_component(None, Path::new("/"))?;
    let trusted = |fd: &OwnedFd| -> io::Result<bool> {
        let stat = stat_fd(fd.as_raw_fd())?;
        Ok(stat.st_uid == 0 && stat.st_mode & 0o022 == 0)
    };
    let mut safe = trusted(&directory)?;
    let mut resolved = PathBuf::from("/");
    let mut aliases = 0;
    while let Some(part) = pending.pop_front() {
        if part == "/" {
            directory = open_component(None, Path::new("/"))?;
            safe = trusted(&directory)?;
            resolved = PathBuf::from("/");
            continue;
        }
        if part == "." {
            continue;
        }
        if part == ".." {
            // A symlink target traversing a user-replaceable directory is not
            // a trustworthy alias. Normal configured roots forbid '..'.
            if !safe {
                return Err(not_allowed());
            }
            directory = open_component(Some(&directory), Path::new(".."))?;
            resolved.pop();
            safe = trusted(&directory)?;
            continue;
        }
        let name = CString::new(part.as_bytes()).map_err(|_| not_allowed())?;
        let stat = stat_at(directory.as_raw_fd(), &name)?;
        if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            // procfs magic links such as self/cwd and self/fd are influenced
            // by process state despite looking root-owned. They are not
            // administrator-controlled filesystem aliases.
            let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstatfs(directory.as_raw_fd(), &mut filesystem) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if filesystem.f_type == libc::PROC_SUPER_MAGIC {
                return Err(not_allowed());
            }
            aliases += 1;
            if !safe || stat.st_uid != 0 || aliases > 40 {
                return Err(not_allowed());
            }
            let mut bytes = vec![0u8; MAX_ROOT_BYTES + 1];
            // SAFETY: name is terminated; bytes has the declared capacity.
            let length = unsafe {
                libc::readlinkat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if length < 0 {
                return Err(io::Error::last_os_error());
            }
            if length as usize > MAX_ROOT_BYTES {
                return Err(not_allowed());
            }
            bytes.truncate(length as usize);
            let target = PathBuf::from(OsString::from_vec(bytes));
            for component in parts(&target).into_iter().rev() {
                pending.push_front(component);
            }
            continue;
        }
        directory = open_component(Some(&directory), Path::new(&part))?;
        safe &= trusted(&directory)?;
        resolved.push(part);
        if resolved.as_os_str().len() > MAX_ROOT_BYTES {
            return Err(not_allowed());
        }
    }
    Ok(resolved)
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

pub(super) fn load_scan_roots_inner(
    path: &Path,
    allow_default: bool,
) -> (RootPolicy, Option<String>) {
    let parse_roots = |entries: Vec<String>| {
        // A policy with many slow/offline roots shares one deadline instead
        // of multiplying a blocking startup delay by the number of entries.
        let deadline = Instant::now() + Duration::from_secs(1);
        RootPolicy::Allowed(
            entries
                .into_iter()
                .filter(|entry| validate_root(entry).is_ok())
                .take(MAX_SCAN_ROOTS)
                .map(|entry| {
                    let configured = PathBuf::from(&entry);
                    // Never authorize a canonical alias controlled by a user.
                    // An unresolved/unsafe root remains lexical; open() will
                    // refuse any symlink on that path.
                    let path = configured.clone();
                    let canonical = crate::probe::run_with_timeout(
                        move || resolve_trusted_root(&path),
                        deadline.saturating_duration_since(Instant::now()),
                    )
                    .and_then(std::result::Result::ok)
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

pub(super) fn read_scan_roots(path: &Path) -> io::Result<Vec<String>> {
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
