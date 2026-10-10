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
    /// Other devices stacked on block devices or backed by something else:
    /// bcache, DRBD, ZFS volumes (zd*), network block devices (nbd), or any
    /// device with slaves. Older releases do not know this value.
    Stacked,
    #[default]
    Other,
}

impl DeviceKind {
    /// Device mapper and MD devices are stacked on other block devices.
    pub fn is_stacked(self) -> bool {
        matches!(self, Self::Dm | Self::Md | Self::Stacked)
    }

    /// Loop, ram and zram devices are hidden unless the user asks for them.
    pub fn hidden_by_default(self) -> bool {
        matches!(self, Self::Loop | Self::Zram | Self::Ram)
    }

    /// Stacked or memory-backed devices whose I/O reappears on other devices.
    pub fn is_virtual(self) -> bool {
        matches!(
            self,
            Self::Dm | Self::Md | Self::Stacked | Self::Loop | Self::Zram | Self::Ram
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
            Self::Stacked => "VIRT",
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
    /// Actual block device names of the filesystems counted above. Empty
    /// from older helpers; synthetic accounting IDs never appear here.
    #[serde(default)]
    pub block_devices: Vec<String>,
    /// Accounting IDs for deduplicating filesystems across disks: block device
    /// names, or `zfs:<pool>` for a pool shared by datasets and disks. Older
    /// helpers omit this field; consumers fall back to `block_devices`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounting_keys: Vec<String>,
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
            let Some(device) = filesystem.accounting_key() else {
                continue;
            };
            // Multi-device btrfs counts on every member disk.
            let on_member = filesystem
                .block_device
                .as_ref()
                .is_some_and(|name| members.contains(name))
                || filesystem
                    .member_devices
                    .iter()
                    .any(|member| members.contains(member));
            if !on_member || !seen.insert(device.clone()) {
                continue;
            }
            usage.accounting_keys.push(device);
            if let Some(name) = &filesystem.block_device
                && !usage.block_devices.contains(name)
            {
                usage.block_devices.push(name.clone());
            }
            match filesystem.accounting_usage() {
                Some((total, used, available)) => {
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
    /// All known devices of a multi-device btrfs filesystem or ZFS pool, including
    /// `block_device`; empty for single-device filesystems.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_devices: Vec<String>,
    /// Pool-wide physical allocation, separate from this dataset's statvfs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zfs: Option<ZfsPool>,
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
    /// stat/statvfs did not answer in time (hung NFS-like or FUSE storage,
    /// failing disk); sizes are unknown until it answers again.
    #[serde(default)]
    pub not_responding: bool,
}

impl Filesystem {
    pub fn accounting_key(&self) -> Option<String> {
        if self.fs_type == "zfs" {
            return self.zfs.as_ref().map(|pool| format!("zfs:{}", pool.name));
        }
        self.block_device.clone()
    }

    pub fn accounting_usage(&self) -> Option<(u64, u64, u64)> {
        if self.fs_type == "zfs" {
            let pool = self.zfs.as_ref()?;
            return Some((pool.total_bytes?, pool.used_bytes?, pool.available_bytes?));
        }
        Some((self.total_bytes?, self.used_bytes?, self.available_bytes?))
    }
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZfsPool {
    pub name: String,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    /// Membership/usage unavailable or incomplete; never silently infer it
    /// from a dataset's quota-dependent statvfs values.
    pub error: Option<String>,
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
    /// Machine-readable kind of `error`, for translation and for falling back
    /// to an unprivileged scan of roots the helper may not scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<HomeErrorKind>,
    /// The helper may not scan this root, so the UI scans it unprivileged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub helper_refused: bool,
}

/// Why the home view has no scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HomeErrorKind {
    /// Not 1 to 4096 bytes.
    Length,
    Relative,
    ControlCharacters,
    /// Trailing or double slashes.
    NotNormalized,
    /// "." or ".." components.
    DotComponents,
    /// The helper may scan only the roots in /etc/spctop/scan-roots, and
    /// never through a symlink below them.
    NotAllowed,
    /// The scanner thread could not be started or prepared.
    StartFailed,
    /// The root could not be opened.
    ScanFailed,
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
    /// Stable identity of this component, independent of display truncation.
    /// New scans use the raw filename bytes as lowercase hex (at most 510
    /// ASCII characters on Linux). Missing in reports from older helpers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
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
    pub fn identity(&self) -> &str {
        self.key.as_deref().unwrap_or(&self.name)
    }

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

    /// Follow component identities from the root; stops at the deepest existing
    /// node. Reports from older helpers use their sanitized names as identities.
    pub fn resolve(&self, path: &[String]) -> (u32, usize) {
        let mut current = 0u32;
        for (depth, name) in path.iter().enumerate() {
            let Some(node) = self.node(current) else {
                return (0, 0);
            };
            match node.children.iter().copied().find(|child| {
                self.node(*child)
                    .is_some_and(|child| child.identity() == name)
            }) {
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
    /// Kernel files that exceeded the read limit and were cut at the last
    /// complete line, for example "/proc/self/mountinfo".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncated: Vec<String>,
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
    fn home_identities_survive_json_and_legacy_reports_use_names() {
        let report = HomeReport {
            nodes: vec![
                DirNode {
                    name: "/home".into(),
                    children: vec![1, 2, 3],
                    ..DirNode::default()
                },
                DirNode {
                    name: "same~".into(),
                    key: Some("61".into()),
                    ..DirNode::default()
                },
                DirNode {
                    name: "same~".into(),
                    key: Some("62".into()),
                    ..DirNode::default()
                },
                DirNode {
                    name: "legacy".into(),
                    ..DirNode::default()
                },
            ],
            ..HomeReport::default()
        };
        let json = serde_json::to_value(&report).unwrap();
        assert!(json["nodes"][3].get("key").is_none());
        let decoded: HomeReport = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.resolve(&["61".into()]), (1, 1));
        assert_eq!(decoded.resolve(&["62".into()]), (2, 1));
        assert_eq!(decoded.resolve(&["legacy".into()]), (3, 1));
        assert_eq!(decoded.node(3).unwrap().identity(), "legacy");
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
        assert_eq!(usage.accounting_keys, usage.block_devices);
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
        let legacy: DeviceUsage = serde_json::from_str(
            r#"{"filesystems":1,"unmeasured":0,"total_bytes":100,"used_bytes":40,"available_bytes":60,"swap":false,"block_devices":["sda1"]}"#
        ).unwrap();
        assert_eq!(legacy.block_devices, ["sda1"]);
        assert!(legacy.accounting_keys.is_empty());
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("accounting_keys")
                .is_none()
        );
    }

    #[test]
    fn zfs_counts_each_pool_once_across_datasets_on_every_member() {
        let dataset = |source: &str, pool: &str, used: u64| Filesystem {
            source: source.into(),
            fs_type: "zfs".into(),
            block_device: Some("sda1".into()),
            member_devices: vec!["sda1".into(), "sdb1".into()],
            // Deliberately contradictory quota-dependent dataset values.
            total_bytes: Some(80),
            used_bytes: Some(10),
            available_bytes: Some(70),
            zfs: Some(ZfsPool {
                name: pool.into(),
                total_bytes: Some(1000),
                used_bytes: Some(used),
                available_bytes: Some(1000 - used),
                error: None,
            }),
            ..Filesystem::default()
        };
        let rows = vec![
            dataset("tank/home", "tank", 400),
            dataset("tank", "tank", 400),
            dataset("tank/home/user", "tank", 400),
            dataset("other/child", "other", 200),
        ];
        for disk in ["sda1", "sdb1"] {
            let usage = DeviceUsage::from_filesystems(&rows, &BTreeSet::from([disk.into()]), false);
            assert_eq!(usage.filesystems, 2);
            assert_eq!(
                (usage.total_bytes, usage.used_bytes, usage.available_bytes),
                (2000, 600, 1400)
            );
            assert_eq!(usage.accounting_keys, ["zfs:tank", "zfs:other"]);
            assert_eq!(usage.block_devices, ["sda1"]);
        }
        let mut unavailable = dataset("tank/home", "tank", 400);
        unavailable.zfs.as_mut().unwrap().used_bytes = None;
        let usage =
            DeviceUsage::from_filesystems([&unavailable], &BTreeSet::from(["sdb1".into()]), false);
        assert_eq!(
            (usage.filesystems, usage.unmeasured, usage.used_bytes),
            (0, 1, 0)
        );
        unavailable.zfs = None; // An older helper must not count a dataset as a pool.
        assert!(unavailable.accounting_usage().is_none());
        unavailable.member_devices.clear();
        unavailable.block_device = None;
        let usage =
            DeviceUsage::from_filesystems([&unavailable], &BTreeSet::from(["sdb1".into()]), false);
        assert_eq!(usage.filesystems, 0);
    }
}
