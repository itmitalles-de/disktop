//! Values shared by the collector, helper protocol, terminal UI and JSON output.
//!
//! Field names are part of the stable `--json` output; add fields rather than
//! renaming them. `None` always means "not measured", never zero.

use serde::{Deserialize, Serialize};

/// Version of the `--json` document layout.
pub const JSON_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Nvme,
    Ssd,
    Hdd,
    /// Device mapper (LVM, LUKS, multipath).
    Dm,
    /// Linux software RAID.
    Md,
    Loop,
    Zram,
    Ram,
    Optical,
    #[default]
    Other,
}

impl DeviceKind {
    /// Device mapper and MD devices are stacked on other block devices.
    pub fn is_stacked(self) -> bool {
        matches!(self, Self::Dm | Self::Md)
    }

    /// Loop, ram and zram devices are hidden unless the user asks for them.
    pub fn hidden_by_default(self) -> bool {
        matches!(self, Self::Loop | Self::Zram | Self::Ram)
    }

    /// Stacked or memory-backed devices whose I/O reappears on other devices.
    pub fn is_virtual(self) -> bool {
        matches!(
            self,
            Self::Dm | Self::Md | Self::Loop | Self::Zram | Self::Ram
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Nvme => "NVMe",
            Self::Ssd => "SSD",
            Self::Hdd => "HDD",
            Self::Dm => "DM",
            Self::Md => "MD",
            Self::Loop => "LOOP",
            Self::Zram => "ZRAM",
            Self::Ram => "RAM",
            Self::Optical => "OPTICAL",
            Self::Other => "DISK",
        }
    }
}

/// Rates over the last refresh interval, from /proc/diskstats deltas.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceRates {
    pub read_rate: f64,
    pub write_rate: f64,
    pub read_iops: f64,
    pub write_iops: f64,
    /// Share of wall time with at least one request in flight (io_ticks).
    pub utilization_percent: f64,
    /// Average request time in milliseconds; `None` without completed I/O.
    pub await_ms: Option<f64>,
    pub read_await_ms: Option<f64>,
    pub write_await_ms: Option<f64>,
    /// Average number of requests in flight (weighted io_ticks).
    pub queue_depth: f64,
}

/// Capacity use of the filesystems mounted from a device, its partitions and
/// stacked devices (LVM, LUKS, MD), each block device counted once.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceUsage {
    /// Mounted filesystems with statvfs values; 0 means nothing is mounted.
    pub filesystems: u32,
    /// Mounted filesystems whose statvfs failed; they are not in the sums.
    pub unmeasured: u32,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    /// The device, a partition or a stacked device is active swap.
    pub swap: bool,
}

impl DeviceUsage {
    /// Usage like df: used / (used + available), so reserved blocks count as full.
    pub fn percent(&self) -> Option<f64> {
        let used = self.used_bytes as f64;
        let room = used + self.available_bytes as f64;
        (self.filesystems > 0 && room > 0.0).then(|| used / room * 100.0)
    }

    /// Sum the filesystems backed by any of `members` (a device and its
    /// partitions and holders). A block device holds one filesystem, so each
    /// is counted once even when bind mounts or subvolumes list it repeatedly.
    pub fn from_filesystems<'a>(
        filesystems: impl IntoIterator<Item = &'a Filesystem>,
        members: &std::collections::BTreeSet<String>,
        swap: bool,
    ) -> Self {
        let mut usage = Self {
            swap,
            ..Self::default()
        };
        let mut seen = std::collections::BTreeSet::new();
        for filesystem in filesystems {
            let Some(device) = filesystem.block_device.as_ref() else {
                continue;
            };
            if !members.contains(device) || !seen.insert(device.clone()) {
                continue;
            }
            match (
                filesystem.total_bytes,
                filesystem.used_bytes,
                filesystem.available_bytes,
            ) {
                (Some(total), Some(used), Some(available)) => {
                    usage.filesystems += 1;
                    usage.total_bytes = usage.total_bytes.saturating_add(total);
                    usage.used_bytes = usage.used_bytes.saturating_add(used);
                    usage.available_bytes = usage.available_bytes.saturating_add(available);
                }
                _ => usage.unmeasured += 1,
            }
        }
        usage
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    pub kind: DeviceKind,
    pub partition: bool,
    /// Whole disk containing this partition.
    pub parent: Option<String>,
    pub model: Option<String>,
    pub vendor: Option<String>,
    /// Device-mapper name or MD RAID level.
    pub label: Option<String>,
    pub size_bytes: Option<u64>,
    pub rotational: Option<bool>,
    pub removable: bool,
    pub hw_sector_size: Option<u32>,
    /// Mount points of this device, its partitions and stacked devices.
    pub mounts: Vec<String>,
    /// `None` on the first sample or after a counter reset.
    pub rates: Option<DeviceRates>,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub reads: u64,
    pub writes: u64,
    pub in_flight: u64,
    /// `None` when not measured (for example from an older helper).
    #[serde(default)]
    pub usage: Option<DeviceUsage>,
    /// Whole disks below a device-mapper or MD device, through partitions and
    /// further stacked devices; empty for other devices or when unknown.
    #[serde(default)]
    pub backing_disks: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessRow {
    pub pid: u32,
    pub uid: u32,
    pub user: String,
    /// Kernel task name (`comm`), sanitized.
    pub name: String,
    /// Command line, sanitized; empty for kernel threads.
    pub command: String,
    /// Storage bytes per second from /proc/PID/io; `None` while unknown.
    pub read_rate: Option<f64>,
    pub write_rate: Option<f64>,
    pub cancelled_write_rate: Option<f64>,
    /// Cumulative kernel counters since the process started.
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled_write_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessAccess {
    /// True when the root-owned helper collected the data.
    pub privileged: bool,
    pub readable: usize,
    /// Processes whose /proc/PID/io could not be read (other users, or
    /// non-dumpable). They are omitted from the table, never shown as idle.
    pub unreadable: usize,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Filesystem {
    pub mount_point: String,
    /// Mount source, typically /dev/...
    pub source: String,
    pub fs_type: String,
    /// "major:minor" from mountinfo.
    pub dev: String,
    /// Block device in /proc/diskstats that backs this filesystem.
    pub block_device: Option<String>,
    pub read_only: bool,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub inodes_total: Option<u64>,
    pub inodes_used: Option<u64>,
    pub read_rate: Option<f64>,
    pub write_rate: Option<f64>,
    /// Other mount points of the same filesystem root (bind mounts).
    pub bind_mounts: usize,
    /// This filesystem contains the configured home path.
    pub contains_home: bool,
}

impl Filesystem {
    /// Usage like df: used / (used + available), so reserved blocks count as full.
    pub fn use_percent(&self) -> Option<f64> {
        let used = self.used_bytes? as f64;
        let available = self.available_bytes? as f64;
        (used + available > 0.0).then(|| used / (used + available) * 100.0)
    }

    pub fn inode_percent(&self) -> Option<f64> {
        let total = self.inodes_total.filter(|total| *total > 0)? as f64;
        Some(self.inodes_used? as f64 / total * 100.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    /// Not requested yet; scans start when the home view is opened.
    #[default]
    Idle,
    Scanning,
    Done,
    Failed,
}

/// Lightweight scan progress sent with every snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HomeStatus {
    pub root: String,
    pub state: ScanState,
    /// Increases with every finished scan.
    pub generation: u64,
    /// Entries visited by the running scan.
    pub progress_entries: u64,
    pub progress_seconds: f64,
    pub last_finished_unix: Option<u64>,
    pub next_scan_in_seconds: Option<u64>,
    pub privileged: bool,
    pub error: Option<String>,
}

/// Why a scan stopped before visiting everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EntryBudget,
    TimeBudget,
    Cancelled,
}

/// Directories that were not kept individually, summarized per parent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folded {
    pub dirs: u64,
    pub allocated: u64,
    pub apparent: u64,
    pub files: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DirNode {
    /// Sanitized display name; the root node has the sanitized root path.
    pub name: String,
    pub children: Vec<u32>,
    /// Allocated bytes (st_blocks * 512) of the whole subtree.
    pub allocated: u64,
    /// Apparent size (st_size) of the whole subtree.
    pub apparent: u64,
    /// Non-directory entries in the whole subtree.
    pub files: u64,
    pub dirs: u64,
    /// Files directly in this directory.
    pub own_files: Folded,
    /// Subdirectories that were summarized instead of kept as nodes.
    pub other: Folded,
    /// Unreadable directories or entries in the subtree; > 0 means partial.
    pub unreadable: u64,
    /// Subtree deeper than the scan depth limit was not visited.
    pub truncated: bool,
    /// Allocated growth since the previous scan / the first scan.
    pub growth_previous: Option<i64>,
    pub growth_start: Option<i64>,
}

impl DirNode {
    pub fn partial(&self) -> bool {
        self.unreadable > 0 || self.truncated
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HomeReport {
    pub root: String,
    pub generation: u64,
    pub started_unix: u64,
    pub duration_ms: u64,
    pub entries: u64,
    /// `None` when the scan visited everything within its limits.
    pub stopped: Option<StopReason>,
    /// Mount points of other filesystems that were skipped (one-fs scan).
    pub other_filesystems: u64,
    /// Hard-link tracking ran out of room; later links may be counted twice.
    pub hardlink_limit_reached: bool,
    pub privileged: bool,
    /// nodes[0] is the root; children reference indexes in this vector.
    pub nodes: Vec<DirNode>,
}

impl HomeReport {
    pub fn root_node(&self) -> Option<&DirNode> {
        self.nodes.first()
    }

    pub fn node(&self, index: u32) -> Option<&DirNode> {
        self.nodes.get(index as usize)
    }

    /// Follow sanitized names from the root; stops at the deepest existing node.
    pub fn resolve(&self, path: &[String]) -> (u32, usize) {
        let mut current = 0u32;
        for (depth, name) in path.iter().enumerate() {
            let Some(node) = self.node(current) else {
                return (0, 0);
            };
            match node
                .children
                .iter()
                .copied()
                .find(|child| self.node(*child).is_some_and(|child| &child.name == name))
            {
                Some(child) => current = child,
                None => return (current, depth),
            }
        }
        (current, path.len())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub demo: bool,
    /// Seconds since the collector started.
    pub elapsed: f64,
    /// Seconds covered by the rates in this snapshot.
    pub interval: f64,
    pub devices: Vec<Device>,
    pub processes: Vec<ProcessRow>,
    pub process_access: ProcessAccess,
    pub filesystems: Vec<Filesystem>,
    pub home: Option<HomeStatus>,
    /// Present only when a newer scan finished than the requester knows.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub home_tree: Option<HomeReport>,
}

/// What the UI asks the collector (in-process or helper) for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeRequest {
    pub root: String,
    /// Start or keep the background scanner running.
    pub scan: bool,
    pub interval_minutes: u16,
    /// Start a new scan now.
    pub rescan: bool,
    /// Highest report generation the requester already has.
    pub known_generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn filesystem(device: &str, total: Option<u64>, used: u64) -> Filesystem {
        Filesystem {
            block_device: Some(device.into()),
            total_bytes: total,
            used_bytes: total.map(|_| used),
            available_bytes: total.map(|total| total - used),
            ..Filesystem::default()
        }
    }

    #[test]
    fn usage_sums_each_backing_filesystem_once_including_holders() {
        let filesystems = [
            filesystem("nvme0n1p2", Some(100), 40),
            // A bind mount or subvolume of the same block device.
            filesystem("nvme0n1p2", Some(100), 40),
            // A LUKS holder of the disk's other partition.
            filesystem("dm-0", Some(300), 280),
            filesystem("dm-1", None, 0),
            filesystem("sda1", Some(1000), 1),
        ];
        let members: BTreeSet<String> = ["nvme0n1", "nvme0n1p1", "nvme0n1p2", "dm-0", "dm-1"]
            .map(String::from)
            .into();
        let usage = DeviceUsage::from_filesystems(&filesystems, &members, false);
        assert_eq!(usage.filesystems, 2);
        assert_eq!(usage.unmeasured, 1);
        assert_eq!((usage.used_bytes, usage.total_bytes), (320, 400));
        assert_eq!(usage.percent(), Some(80.0));
        let none =
            DeviceUsage::from_filesystems(&filesystems, &BTreeSet::from(["sdb".into()]), true);
        assert_eq!(none.filesystems, 0);
        assert!(none.swap && none.percent().is_none());
        // Older helpers without the field deserialize as "not measured".
        let device: Device = serde_json::from_str(
            r#"{"name":"sda","kind":"hdd","partition":false,
            "parent":null,"model":null,"vendor":null,"label":null,"size_bytes":null,
            "rotational":null,"removable":false,"hw_sector_size":null,"mounts":[],"rates":null,
            "read_bytes":0,"write_bytes":0,"reads":0,"writes":0,"in_flight":0}"#,
        )
        .unwrap();
        assert!(device.usage.is_none());
    }
}
