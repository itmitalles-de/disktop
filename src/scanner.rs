//! Bounded background scanner for the home view.
//!
//! The scan stays on the root's filesystem (st_dev), never follows symlinks
//! (every directory is opened relative to its parent with O_NOFOLLOW), counts
//! hard-linked files once while its bounded (dev, inode) set has room, and
//! never reads file contents. Only directory names and sizes are retained,
//! in a tree limited by depth, children per directory and total node count.

mod fs;
mod policy;
mod service;
mod walk;

#[cfg(test)]
mod tests;

pub use policy::AllowedRoot;
pub use policy::RootError;
pub use policy::RootNotAllowed;
pub use policy::RootPolicy;
pub use policy::is_not_allowed;
pub use policy::load_scan_roots;
pub use policy::load_scan_roots_with_legacy;
pub use policy::validate_root;
pub use service::HomeService;
pub use walk::scan;
pub use walk::scan_with;

use std::time::Duration;

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
