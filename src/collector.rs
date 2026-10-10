//! Linux collection: /proc/diskstats, sysfs, mountinfo, statvfs, /proc/PID/io
//! and the optional home scanner. The same code runs in the unprivileged UI
//! and in the helper; only the helper sees every process and directory.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    io::Read,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::{
    diskstats::{self, Info, Sysfs},
    model::{
        Device, DeviceRates, DeviceUsage, Filesystem, HomeErrorKind, HomeReport, HomeRequest,
        HomeStatus, JSON_VERSION, ScanState, Snapshot,
    },
    mounts::{self, Mount},
    privilege,
    probe::{self, MountProber, Outcome, Resolver},
    procio::ProcessTable,
    scanner::{self, HomeService, Limits, RootPolicy},
    text::sanitize,
    zfs,
};

/// Large enough for mountinfo of container hosts with tens of thousands of
/// mounts; longer files are cut at their last complete line and reported.
pub const MAX_PSEUDO_FILE: usize = 16 * 1024 * 1024;
const MAX_DEVICES: usize = 4096;
const MAX_DEVICE_MOUNTS: usize = 32;

/// A kernel pseudo-file read to its end, or to `MAX_PSEUDO_FILE`.
pub struct Pseudo {
    pub bytes: Vec<u8>,
    /// The file was longer than the limit; `bytes` ends at the last complete
    /// line, so no number is cut in the middle.
    pub truncated: bool,
}

pub fn read_pseudo_file(path: &str) -> Result<Pseudo> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("opening {path}"))?
        .take(MAX_PSEUDO_FILE as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {path}"))?;
    let truncated = bytes.len() > MAX_PSEUDO_FILE;
    if truncated {
        bytes.truncate(MAX_PSEUDO_FILE);
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        bytes.truncate(end);
    }
    Ok(Pseudo { bytes, truncated })
}

pub fn read_pseudo(path: &str) -> Result<Vec<u8>> {
    Ok(read_pseudo_file(path)?.bytes)
}

struct Previous {
    major: u32,
    minor: u32,
    counters: diskstats::Counters,
}

pub struct Collector {
    sysfs: Sysfs,
    started: Instant,
    last: Instant,
    previous: HashMap<String, Previous>,
    /// Cached sysfs facts per device name, valid for `numbers`.
    infos: HashMap<String, Info>,
    numbers: HashMap<String, (u32, u32)>,
    processes: ProcessTable,
    privileged: bool,
    home: HomeScan,
    prober: MountProber,
    resolver: Resolver,
    zfs: zfs::Pools,
}

impl Collector {
    /// `helper` is true only inside the capability helper. Collection is
    /// privileged there or when the UI itself runs as root; user names are
    /// resolved only outside the helper.
    pub fn new(helper: bool) -> Result<Self> {
        Self::with_limits(helper, Limits::default())
    }

    pub fn with_limits(helper: bool, limits: Limits) -> Result<Self> {
        // SAFETY: geteuid has no preconditions.
        let privileged = helper || unsafe { libc::geteuid() } == 0;
        // Only the helper is restricted: root can read everything anyway.
        let (policy, problem) = if helper {
            scanner::load_scan_roots_with_legacy(
                Path::new(scanner::SCAN_ROOTS_PATH),
                Path::new(scanner::LEGACY_SCAN_ROOTS_PATH),
            )
        } else {
            (RootPolicy::Follow, None)
        };
        let mut collector = Self {
            sysfs: Sysfs::default(),
            started: Instant::now(),
            last: Instant::now(),
            previous: HashMap::new(),
            infos: HashMap::new(),
            numbers: HashMap::new(),
            processes: if helper {
                ProcessTable::for_helper("/proc")
            } else {
                ProcessTable::new("/proc", true)
            },
            privileged,
            home: HomeScan::new(limits, privileged, policy, problem),
            prober: MountProber::default(),
            resolver: Resolver::default(),
            zfs: zfs::Pools::default(),
        };
        // Establish baselines so the first visible sample has rates.
        collector
            .sample(&HomeRequest::default())
            .context("cannot read Linux block device counters")?;
        Ok(collector)
    }

    pub fn sample(&mut self, request: &HomeRequest) -> Result<Snapshot> {
        let stats = read_pseudo_file("/proc/diskstats")?;
        let now = Instant::now();
        let seconds = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        let mut truncated = BTreeSet::new();
        let mut lines = diskstats::parse(&String::from_utf8_lossy(&stats.bytes));
        if stats.truncated || lines.len() > MAX_DEVICES {
            truncated.insert("/proc/diskstats".to_owned());
        }
        lines.truncate(MAX_DEVICES);
        // Hot-plug: facts are cached per device name and number; a name that
        // now belongs to another device number is read anew. Size and label
        // change while a device exists and are re-read every sample.
        let names: HashSet<&str> = lines.iter().map(|line| line.name.as_str()).collect();
        self.infos.retain(|name, _| names.contains(name.as_str()));
        self.numbers.retain(|name, _| names.contains(name.as_str()));
        for line in &lines {
            let number = (line.major, line.minor);
            match self.infos.get_mut(&line.name) {
                Some(info) if self.numbers.get(&line.name) == Some(&number) => {
                    self.sysfs.refresh(&line.name, info);
                }
                _ => {
                    self.infos
                        .insert(line.name.clone(), self.sysfs.info(&line.name));
                    self.numbers.insert(line.name.clone(), number);
                }
            }
        }
        let mount_list = match read_pseudo_file("/proc/self/mountinfo") {
            Ok(file) => {
                let list = mounts::parse(&file.bytes);
                if file.truncated || list.len() >= mounts::MAX_MOUNTS {
                    truncated.insert("/proc/self/mountinfo".to_owned());
                }
                list
            }
            Err(_) => Vec::new(),
        };
        let by_number: HashMap<(u32, u32), &str> = lines
            .iter()
            .map(|line| ((line.major, line.minor), line.name.as_str()))
            .collect();
        if mount_list.iter().any(|mount| mount.fs_type == "zfs") {
            self.zfs.refresh();
        }
        let infos = &self.infos;
        let mount_device = |mount: &Mount| -> Option<String> {
            if mount.major != 0 {
                return by_number
                    .get(&(mount.major, mount.minor))
                    .map(|name| (*name).to_owned());
            }
            // btrfs and other multi-device filesystems report an anonymous
            // major 0; their source names the block device (in /dev, which
            // never blocks).
            mount
                .source
                .starts_with("/dev/")
                .then(|| fs::canonicalize(&mount.source).ok())
                .flatten()
                .and_then(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .filter(|name| infos.contains_key(name))
        };
        // Multi-device btrfs: every member disk holds the filesystem.
        let btrfs = self.sysfs.btrfs_members();
        let members_of = |device: &str| -> Vec<String> {
            btrfs
                .get(device)
                .map(|members| {
                    members
                        .iter()
                        .filter(|name| infos.contains_key(*name))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut mounts_by_device: HashMap<String, BTreeSet<String>> = HashMap::new();
        for mount in &mount_list {
            if mount.fs_type == "zfs" {
                let pool = self.zfs.get(&mount.source.to_string_lossy());
                let point = mounts::display(&mount.mount_point);
                for member in pool
                    .members
                    .into_iter()
                    .filter(|name| infos.contains_key(name))
                {
                    mounts_by_device
                        .entry(member)
                        .or_default()
                        .insert(point.clone());
                }
                continue;
            }
            if let Some(device) = mount_device(mount) {
                let point = mounts::display(&mount.mount_point);
                for member in members_of(&device) {
                    mounts_by_device
                        .entry(member)
                        .or_default()
                        .insert(point.clone());
                }
                mounts_by_device.entry(device).or_default().insert(point);
            }
        }
        // One pass over the devices instead of one per device.
        let mut partitions: HashMap<&str, Vec<String>> = HashMap::new();
        for (name, info) in infos {
            if let Some(parent) = &info.parent {
                partitions
                    .entry(parent.as_str())
                    .or_default()
                    .push(name.clone());
            }
        }
        let mut holders = HashMap::new();
        let mut devices = Vec::with_capacity(lines.len());
        let mut members = Vec::with_capacity(lines.len());
        let mut current = HashMap::with_capacity(lines.len());
        for line in &lines {
            let info = infos.get(&line.name).cloned().unwrap_or_default();
            let rates = self
                .previous
                .get(&line.name)
                .filter(|old| old.major == line.major && old.minor == line.minor)
                .and_then(|old| diskstats::rates(&old.counters, &line.counters, seconds));
            let mut device_mounts = BTreeSet::new();
            let descendants = self
                .sysfs
                .descendants_cached(&line.name, &partitions, &mut holders);
            for name in &descendants {
                if let Some(points) = mounts_by_device.get(name) {
                    device_mounts.extend(points.iter().cloned());
                }
            }
            members.push(descendants);
            let backing_disks = if info.kind.is_stacked() {
                self.sysfs
                    .backing_disks(&line.name, infos)
                    .into_iter()
                    .map(|name| sanitize(&name))
                    .collect()
            } else {
                Vec::new()
            };
            devices.push(Device {
                name: sanitize(&line.name),
                kind: info.kind,
                partition: info.partition,
                parent: info.parent,
                model: info.model,
                vendor: info.vendor,
                label: info.label,
                size_bytes: info.size_bytes,
                rotational: info.rotational,
                removable: info.removable,
                hw_sector_size: info.hw_sector_size,
                mounts: device_mounts.into_iter().take(MAX_DEVICE_MOUNTS).collect(),
                rates,
                read_bytes: line
                    .counters
                    .sectors_read
                    .saturating_mul(diskstats::SECTOR_BYTES),
                write_bytes: line
                    .counters
                    .sectors_written
                    .saturating_mul(diskstats::SECTOR_BYTES),
                reads: line.counters.reads,
                writes: line.counters.writes,
                in_flight: line.counters.in_flight,
                usage: None,
                backing_disks,
            });
            current.insert(
                line.name.clone(),
                Previous {
                    major: line.major,
                    minor: line.minor,
                    counters: line.counters,
                },
            );
        }
        // Removed devices drop out, so a re-added device starts a new baseline.
        self.previous = current;
        // The home path's symlinks are resolved off this thread and cached;
        // mount matching itself is lexical.
        let home_path =
            (!request.root.is_empty()).then(|| self.resolver.resolve(Path::new(&request.root)));
        let mut filesystems = filesystems(
            &mount_list,
            &devices,
            home_path.as_deref(),
            &mount_device,
            &members_of,
            &mut self.prober,
        );
        for filesystem in &mut filesystems {
            if filesystem.fs_type == "zfs" {
                let mut pool = self.zfs.get(&filesystem.source);
                let known: Vec<_> = pool
                    .members
                    .iter()
                    .filter(|name| infos.contains_key(*name))
                    .cloned()
                    .collect();
                if known.len() != pool.members.len() {
                    pool.usage.error = Some("ZFS pool has members absent from diskstats".into());
                }
                filesystem.block_device = known.first().cloned();
                filesystem.member_devices = known;
                filesystem.zfs = Some(pool.usage);
                // Process/device I/O cannot be attributed to a dataset.
                filesystem.read_rate = None;
                filesystem.write_rate = None;
            }
        }
        apply_usage(&mut devices, &members, &filesystems);
        let (processes, process_access) = self.processes.sample(self.privileged);
        let (home, home_tree) = self.home.sample(request);
        Ok(Snapshot {
            version: JSON_VERSION,
            demo: false,
            elapsed: now.duration_since(self.started).as_secs_f64(),
            interval: seconds,
            devices,
            processes,
            process_access,
            filesystems,
            home,
            home_tree,
            truncated: truncated.into_iter().collect(),
        })
    }
}

/// The home scanner as the collector drives it from requests: validation,
/// the helper's root policy, starting and reconfiguring the service.
pub struct HomeScan {
    service: Option<HomeService>,
    limits: Limits,
    privileged: bool,
    policy: RootPolicy,
    /// Why the helper's scan-roots file was rejected, if it was.
    policy_problem: Option<String>,
}

impl HomeScan {
    pub fn new(
        limits: Limits,
        privileged: bool,
        policy: RootPolicy,
        policy_problem: Option<String>,
    ) -> Self {
        Self {
            service: None,
            limits,
            privileged,
            policy,
            policy_problem,
        }
    }

    /// The UI's own scanner, used without the helper or for roots the helper
    /// may not scan.
    pub fn unprivileged() -> Self {
        Self::new(Limits::default(), false, RootPolicy::Follow, None)
    }

    pub fn sample(&mut self, request: &HomeRequest) -> (Option<HomeStatus>, Option<HomeReport>) {
        if request.root.is_empty() {
            return (None, None);
        }
        let privileged = self.privileged;
        let failed = |kind: HomeErrorKind, error: String| {
            (
                Some(HomeStatus {
                    root: sanitize(&request.root),
                    state: ScanState::Failed,
                    error: Some(sanitize(&error)),
                    error_kind: Some(kind),
                    privileged,
                    ..HomeStatus::default()
                }),
                None,
            )
        };
        if let Err(error) = scanner::validate_root(&request.root) {
            return failed(error.0, error.to_string());
        }
        let root = PathBuf::from(&request.root);
        if !self.policy.allows(&root) {
            // Stop scanning the previous root with privileges.
            self.service = None;
            let problem = self
                .policy_problem
                .as_ref()
                .map_or_else(String::new, |problem| format!(" ({problem})"));
            return failed(
                HomeErrorKind::NotAllowed,
                format!(
                    "{} is not below a helper scan root in {}{problem}",
                    request.root,
                    scanner::SCAN_ROOTS_PATH
                ),
            );
        }
        let interval = Duration::from_secs(u64::from(request.interval_minutes.clamp(1, 1440)) * 60);
        if request.scan && self.service.is_none() {
            match HomeService::start(
                root.clone(),
                interval,
                self.limits.clone(),
                self.privileged,
                self.policy.clone(),
                privilege::prepare_scan_thread,
            ) {
                Ok(service) => self.service = Some(service),
                Err(error) => {
                    return failed(
                        HomeErrorKind::StartFailed,
                        format!("cannot start scanner: {error}"),
                    );
                }
            }
        }
        let Some(service) = &self.service else {
            return (
                Some(HomeStatus {
                    root: sanitize(&request.root),
                    privileged: self.privileged,
                    ..HomeStatus::default()
                }),
                None,
            );
        };
        service.configure(root, interval);
        if request.rescan {
            service.rescan();
        }
        (
            Some(service.status()),
            service.report_if_newer(request.known_generation),
        )
    }
}

/// Storage filesystems with their usage. `home` is the resolved home path.
/// Blocking calls (lstat, statvfs) go through `prober`, so a hung mount is
/// listed as not responding instead of stopping the sample.
fn filesystems(
    mount_list: &[Mount],
    devices: &[Device],
    home: Option<&Path>,
    mount_device: &dyn Fn(&Mount) -> Option<String>,
    members_of: &dyn Fn(&str) -> Vec<String>,
    prober: &mut MountProber,
) -> Vec<Filesystem> {
    // Over-mounted filesystems are unreachable by path: statvfs on their
    // mount point would describe the covering mount.
    let visible = mounts::visible(mount_list);
    // The home path may be on NFS, tmpfs, overlay or FUSE; then no storage
    // filesystem contains it and none is marked.
    let home_mount = home
        .and_then(|home| mounts::containing(visible.iter().copied(), home))
        .filter(|mount| mounts::is_storage(mount))
        .map(|mount| mount.mount_point.clone());
    // Bind mounts of the same filesystem root are one row.
    let mut groups: Vec<Vec<&Mount>> = Vec::new();
    let mut seen: HashMap<(u32, u32, &[u8]), usize> = HashMap::new();
    for mount in visible
        .iter()
        .copied()
        .filter(|mount| mounts::is_storage(mount))
    {
        let key = (mount.major, mount.minor, mount.root.as_os_str().as_bytes());
        match seen.get(&key) {
            Some(index) => groups[*index].push(mount),
            None => {
                seen.insert(key, groups.len());
                groups.push(vec![mount]);
            }
        }
    }
    let paths: Vec<PathBuf> = groups
        .iter()
        .map(|group| group[0].mount_point.clone())
        .collect();
    let outcomes = prober.probe_all(&paths);
    let rates_by_name: HashMap<&str, &DeviceRates> = devices
        .iter()
        .filter_map(|device| Some((device.name.as_str(), device.rates.as_ref()?)))
        .collect();
    let mut rows: Vec<Filesystem> = Vec::new();
    for (group, outcome) in groups.iter().zip(outcomes) {
        let (usage, not_responding) = match outcome {
            // Docker and others bind individual files such as /etc/hosts.
            Outcome::Done(probe) if !probe.is_dir => continue,
            Outcome::Done(probe) => (probe.usage, false),
            Outcome::NotResponding => (None, true),
        };
        let first = group[0];
        // Keep the first path unless a bind mount is the one containing home.
        let shown = group
            .iter()
            .find(|mount| home_mount.as_ref() == Some(&mount.mount_point))
            .copied()
            .unwrap_or(first);
        let block_device = mount_device(first);
        let member_devices = block_device.as_deref().map(members_of).unwrap_or_default();
        let rates = block_device
            .as_deref()
            .and_then(|name| rates_by_name.get(name).copied());
        rows.push(Filesystem {
            mount_point: mounts::display(&shown.mount_point),
            source: mounts::display(&first.source),
            fs_type: sanitize(&first.fs_type),
            dev: format!("{}:{}", first.major, first.minor),
            block_device,
            member_devices,
            zfs: None,
            read_only: first.read_only,
            total_bytes: usage.map(|usage| usage.total),
            used_bytes: usage.map(|usage| usage.used),
            available_bytes: usage.map(|usage| usage.available),
            inodes_total: usage
                .map(|usage| usage.inodes_total)
                .filter(|total| *total > 0),
            inodes_used: usage
                .filter(|usage| usage.inodes_total > 0)
                .map(|usage| usage.inodes_total - usage.inodes_free),
            read_rate: rates.map(|rates| rates.read_rate),
            write_rate: rates.map(|rates| rates.write_rate),
            bind_mounts: group.len() - 1,
            contains_home: home_mount.as_ref() == Some(&shown.mount_point),
            not_responding,
        });
    }
    rows.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    rows
}

/// Capacity use per device from the filesystems backed by its members (the
/// device, its partitions and holders). Members are raw kernel names, as are
/// the filesystems' block devices.
fn apply_usage(devices: &mut [Device], members: &[BTreeSet<String>], filesystems: &[Filesystem]) {
    let swaps = swap_devices();
    for (device, members) in devices.iter_mut().zip(members) {
        let swap = members.iter().any(|name| swaps.contains(name));
        device.usage = Some(DeviceUsage::from_filesystems(filesystems, members, swap));
    }
}

/// Fill storage fields that a helper from an older release does not send:
/// fill levels and the disks below dm/md devices. Everything used here is
/// readable without privileges: the snapshot's own filesystems (with their
/// statvfs values), world-readable sysfs links and /proc/swaps. Returns true
/// when something was missing, so the UI can suggest updating the helper.
pub fn complete_storage(sysfs: &Sysfs, snapshot: &mut Snapshot) -> bool {
    let devices = &snapshot.devices;
    let missing_usage = devices.iter().any(|device| device.usage.is_none());
    let missing_backing = devices
        .iter()
        .any(|device| device.kind.is_stacked() && device.backing_disks.is_empty());
    if !missing_usage && !missing_backing {
        return false;
    }
    if missing_usage {
        let mut partitions: HashMap<&str, Vec<String>> = HashMap::new();
        for device in devices {
            if let Some(parent) = &device.parent {
                partitions
                    .entry(parent.as_str())
                    .or_default()
                    .push(device.name.clone());
            }
        }
        let mut holders = HashMap::new();
        let members: Vec<BTreeSet<String>> = devices
            .iter()
            .map(|device| sysfs.descendants_cached(&device.name, &partitions, &mut holders))
            .collect();
        apply_usage(&mut snapshot.devices, &members, &snapshot.filesystems);
    }
    if missing_backing {
        let infos: HashMap<String, Info> = snapshot
            .devices
            .iter()
            .map(|device| {
                (
                    device.name.clone(),
                    Info {
                        kind: device.kind,
                        partition: device.partition,
                        parent: device.parent.clone(),
                        ..Info::default()
                    },
                )
            })
            .collect();
        for device in &mut snapshot.devices {
            if device.kind.is_stacked() && device.backing_disks.is_empty() {
                device.backing_disks = sysfs.backing_disks(&device.name, &infos);
            }
        }
    }
    true
}

/// Block device names with active swap, from /proc/swaps. Swap files are
/// skipped; they live on a filesystem that is already counted.
fn swap_devices() -> BTreeSet<String> {
    let Ok(bytes) = read_pseudo("/proc/swaps") else {
        return BTreeSet::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let path = fields.next()?;
            (fields.next()? == "partition").then_some(path)
        })
        .take(64)
        .filter_map(|path| {
            let decoded = mounts::unescape(path.as_bytes());
            let path = Path::new(std::ffi::OsStr::from_bytes(&decoded));
            let resolved = fs::canonicalize(path).ok()?;
            let name = resolved.file_name()?.to_string_lossy().into_owned();
            diskstats::valid_name(&name).then_some(name)
        })
        .collect()
}

/// The physical whole disk that holds `home_root`, for the automatic device
/// choice. Reads only world-readable mountinfo and sysfs.
pub fn default_device(home_root: &str) -> Option<String> {
    let mount_list = mounts::parse(&read_pseudo("/proc/self/mountinfo").ok()?);
    // Resolving the path may block on a hung mount; give it a second.
    let home = probe::resolve_with_timeout(Path::new(home_root), Duration::from_secs(1))
        .unwrap_or_else(|| mounts::lexical(Path::new(home_root)));
    // Home on NFS, tmpfs, overlay or FUSE has no disk to focus.
    let mount = mounts::containing(mounts::visible(&mount_list), &home)
        .filter(|mount| mounts::is_storage(mount))?;
    // A pool/dataset source is not a local path. Pool discovery happens
    // asynchronously during sampling; use the all-disks overview initially.
    if mount.fs_type == "zfs" {
        return None;
    }
    let sysfs = Sysfs::default();
    let name = if mount.major != 0 {
        let link = fs::read_link(format!("/sys/dev/block/{}:{}", mount.major, mount.minor)).ok()?;
        link.file_name()?.to_string_lossy().into_owned()
    } else {
        fs::canonicalize(&mount.source)
            .ok()?
            .file_name()?
            .to_string_lossy()
            .into_owned()
    };
    if !diskstats::valid_name(&name) || sysfs.btrfs_members().contains_key(&name) {
        // A multi-device btrfs filesystem spans several disks.
        return None;
    }
    let mut infos = HashMap::new();
    let mut pending = vec![name.clone()];
    while let Some(device) = pending.pop() {
        if infos.len() > 64 || infos.contains_key(&device) {
            continue;
        }
        pending.extend(sysfs.slaves(&device));
        let info = sysfs.info(&device);
        infos.insert(device, info);
    }
    let disks = sysfs.backing_disks(&name, &infos);
    // A filesystem spanning several disks has no single device; use all.
    (disks.len() == 1).then(|| disks[0].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceKind;

    #[test]
    fn live_sample_has_devices_filesystems_and_own_process() {
        let mut collector = Collector::new(false).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let request = HomeRequest {
            root: "/".into(),
            ..HomeRequest::default()
        };
        let snapshot = collector.sample(&request).unwrap();
        assert_eq!(snapshot.version, JSON_VERSION);
        assert!(!snapshot.demo);
        assert!(snapshot.interval > 0.0);
        // Containers and CI runners still expose host block devices.
        if !snapshot.devices.is_empty() {
            assert!(snapshot.devices.iter().any(|device| device.rates.is_some()));
        }
        assert!(
            snapshot
                .processes
                .iter()
                .any(|row| row.pid == std::process::id())
        );
        assert!(
            snapshot
                .filesystems
                .iter()
                .filter(|fs| fs.contains_home)
                .count()
                <= 1
        );
        let home = snapshot.home.unwrap();
        assert_eq!(home.state, ScanState::Idle, "no scan until requested");
        assert!(snapshot.home_tree.is_none());
        let invalid = collector
            .sample(&HomeRequest {
                root: "relative".into(),
                scan: true,
                ..HomeRequest::default()
            })
            .unwrap();
        assert_eq!(invalid.home.unwrap().state, ScanState::Failed);
        // Stacked devices name the whole disks below them.
        for device in &snapshot.devices {
            assert_eq!(
                device.kind.is_stacked(),
                !device.backing_disks.is_empty(),
                "{}",
                device.name
            );
        }
        // Every device reports its capacity use; mounted ones have totals.
        assert!(snapshot.devices.iter().all(|device| device.usage.is_some()));
        for filesystem in &snapshot.filesystems {
            let Some(name) = &filesystem.block_device else {
                continue;
            };
            let device = snapshot.devices.iter().find(|device| &device.name == name);
            if let (Some(device), Some(_)) = (device, filesystem.accounting_usage()) {
                assert!(device.usage.as_ref().unwrap().filesystems >= 1, "{name}");
            }
        }
        // JSON round-trips through the helper protocol types.
        let json = serde_json::to_string(&snapshot.devices).unwrap();
        let back: Vec<Device> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), snapshot.devices.len());
    }

    /// Stand-in for lstat/statvfs: /hung blocks until released, /etc/hosts
    /// is a file bind mount, everything else is a 100-byte filesystem.
    static HUNG_RELEASED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn fake_probe(path: &Path) -> probe::Probe {
        if path.starts_with("/hung") {
            while !HUNG_RELEASED.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let is_dir = path != Path::new("/etc/hosts");
        probe::Probe {
            is_dir,
            usage: is_dir.then_some(mounts::Usage {
                total: 100,
                used: 40,
                available: 60,
                ..mounts::Usage::default()
            }),
        }
    }

    #[test]
    fn filesystems_skip_hidden_and_file_mounts_and_report_hung_ones() {
        let list = mounts::parse(
            b"1 0 8:1 / / rw - ext4 /dev/sda1 rw\n\
2 1 8:17 / /data rw - ext4 /dev/sdb1 rw\n\
3 2 8:33 / /data rw - xfs /dev/sdc1 rw\n\
4 1 8:1 /etc/hosts /etc/hosts rw - ext4 /dev/sda1 rw\n\
5 1 0:40 / /net/home rw - nfs4 server:/home rw\n\
6 1 8:49 / /hung rw - fuseblk /dev/sdd1 rw\n\
7 1 0:41 / /pool rw - btrfs /dev/sde rw\n\
8 1 8:1 /srv /srv rw - ext4 /dev/sda1 rw\n",
        );
        let device = |name: &str| Device {
            name: name.into(),
            ..Device::default()
        };
        let devices: Vec<Device> = ["sda1", "sdb1", "sdc1", "sdd1", "sde", "sdf"]
            .map(device)
            .into();
        let mount_device = |mount: &Mount| -> Option<String> {
            match (mount.major, mount.minor) {
                (8, 1) => Some("sda1".into()),
                (8, 17) => Some("sdb1".into()),
                (8, 33) => Some("sdc1".into()),
                (8, 49) => Some("sdd1".into()),
                (0, 41) => Some("sde".into()),
                _ => None,
            }
        };
        let members_of = |name: &str| -> Vec<String> {
            if name == "sde" {
                vec!["sde".into(), "sdf".into()]
            } else {
                Vec::new()
            }
        };
        let mut prober = MountProber::new(fake_probe, Duration::from_millis(50));
        // Home on NFS: no storage filesystem contains it, not even "/".
        let rows = filesystems(
            &list,
            &devices,
            Some(Path::new("/net/home/tim")),
            &mount_device,
            &members_of,
            &mut prober,
        );
        let points: Vec<&str> = rows.iter().map(|row| row.mount_point.as_str()).collect();
        assert_eq!(points, ["/", "/data", "/hung", "/pool", "/srv"]);
        assert!(rows.iter().all(|row| !row.contains_home));
        // The visible /data is the xfs filesystem that covers the ext4 one.
        assert_eq!(rows[1].fs_type, "xfs");
        let hung = &rows[2];
        assert!(hung.not_responding && hung.total_bytes.is_none());
        assert!(rows.iter().filter(|row| row.not_responding).count() == 1);
        assert_eq!(rows[3].member_devices, ["sde", "sdf"]);
        // The second sample skips the hung mount without waiting again.
        let started = Instant::now();
        let again = filesystems(
            &list,
            &devices,
            Some(Path::new("/srv/x")),
            &mount_device,
            &members_of,
            &mut prober,
        );
        assert!(started.elapsed() < Duration::from_millis(45));
        assert!(again[2].not_responding);
        assert!(again[4].contains_home && !again[0].contains_home);
        HUNG_RELEASED.store(true, std::sync::atomic::Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let rows = filesystems(
                &list,
                &devices,
                None,
                &mount_device,
                &members_of,
                &mut prober,
            );
            if !rows[2].not_responding {
                assert_eq!(rows[2].total_bytes, Some(100));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "answering mount stays not responding"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // btrfs counts on both member disks, once in totals.
        let members: Vec<BTreeSet<String>> = ["sde", "sdf"]
            .map(|name| BTreeSet::from([name.to_owned()]))
            .into();
        let mut disks: Vec<Device> = ["sde", "sdf"].map(device).into();
        apply_usage(
            &mut disks,
            &members,
            &rows_for_usage(&list, &devices, &mut prober),
        );
        for disk in &disks {
            let usage = disk.usage.as_ref().unwrap();
            assert_eq!(
                (usage.filesystems, usage.used_bytes),
                (1, 40),
                "{}",
                disk.name
            );
            assert_eq!(usage.block_devices, ["sde"]);
        }
    }

    fn rows_for_usage(
        list: &[Mount],
        devices: &[Device],
        prober: &mut MountProber,
    ) -> Vec<Filesystem> {
        filesystems(
            list,
            devices,
            None,
            &|mount: &Mount| (mount.minor == 41).then(|| "sde".to_owned()),
            &|name: &str| {
                if name == "sde" {
                    vec!["sde".into(), "sdf".into()]
                } else {
                    Vec::new()
                }
            },
            prober,
        )
    }

    #[test]
    fn long_pseudo_files_are_cut_at_a_complete_line() {
        let path = std::env::temp_dir().join(format!("spctop-pseudo-{}", std::process::id()));
        let line = "8 0 sda 1 2 3 4 5 6 7 8 9 10 11\n";
        let count = MAX_PSEUDO_FILE / line.len() + 10;
        fs::write(&path, line.repeat(count)).unwrap();
        let file = read_pseudo_file(path.to_str().unwrap()).unwrap();
        assert!(file.truncated);
        assert!(file.bytes.len() <= MAX_PSEUDO_FILE);
        assert!(file.bytes.ends_with(b"\n") && file.bytes.len().is_multiple_of(line.len()));
        fs::write(&path, line).unwrap();
        let short = read_pseudo_file(path.to_str().unwrap()).unwrap();
        assert!(!short.truncated && short.bytes == line.as_bytes());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn older_helper_capacity_follows_partitions_of_every_holder() {
        let root =
            std::env::temp_dir().join(format!("spctop-helper-stacks-{}", std::process::id()));
        let block = root.join("class/block");
        for name in [
            "sda", "sda1", "sdb", "sdb1", "md0", "md0p1", "dm-0", "dm-0p1",
        ] {
            fs::create_dir_all(block.join(name).join("holders")).unwrap();
        }
        for (below, above) in [("sda1", "md0"), ("sdb1", "md0"), ("md0p1", "dm-0")] {
            fs::create_dir_all(block.join(below).join("holders").join(above)).unwrap();
            fs::create_dir_all(block.join(above).join("slaves").join(below)).unwrap();
        }
        let mut snapshot = Snapshot {
            devices: [
                ("sda", DeviceKind::Hdd, None),
                ("sda1", DeviceKind::Hdd, Some("sda")),
                ("sdb", DeviceKind::Hdd, None),
                ("sdb1", DeviceKind::Hdd, Some("sdb")),
                ("md0", DeviceKind::Md, None),
                ("md0p1", DeviceKind::Md, Some("md0")),
                ("dm-0", DeviceKind::Dm, None),
                ("dm-0p1", DeviceKind::Dm, Some("dm-0")),
            ]
            .into_iter()
            .map(|(name, kind, parent)| Device {
                name: name.into(),
                kind,
                partition: parent.is_some(),
                parent: parent.map(str::to_owned),
                ..Default::default()
            })
            .collect(),
            filesystems: ["md0p1", "dm-0p1"]
                .map(|name| Filesystem {
                    block_device: Some(name.into()),
                    total_bytes: Some(100),
                    used_bytes: Some(50),
                    available_bytes: Some(50),
                    ..Default::default()
                })
                .into(),
            ..Default::default()
        };
        assert!(complete_storage(&Sysfs::at(&root), &mut snapshot));
        for disk in snapshot
            .devices
            .iter()
            .filter(|device| !device.partition && !device.kind.is_stacked())
        {
            let usage = disk.usage.as_ref().unwrap();
            assert_eq!((usage.filesystems, usage.used_bytes), (2, 100));
            assert_eq!(usage.accounting_keys, ["md0p1", "dm-0p1"]);
        }
        assert_eq!(snapshot.devices[4].backing_disks, ["sda", "sdb"]);
        assert_eq!(snapshot.devices[6].backing_disks, ["sda", "sdb"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn older_helper_snapshots_get_fill_levels_and_backing_disks() {
        let mut collector = Collector::new(false).unwrap();
        let complete = collector.sample(&HomeRequest::default()).unwrap();
        let sysfs = Sysfs::default();
        let mut current = complete.clone();
        assert!(!complete_storage(&sysfs, &mut current), "nothing to add");
        // A reply from an older helper: the device fields did not exist.
        let json = serde_json::to_value(&complete).unwrap();
        let mut old = json.clone();
        for device in old["devices"].as_array_mut().unwrap() {
            let device = device.as_object_mut().unwrap();
            device.remove("usage");
            device.remove("backing_disks");
        }
        let mut snapshot: Snapshot = serde_json::from_value(old).unwrap();
        if snapshot.devices.is_empty() {
            return;
        }
        assert!(snapshot.devices.iter().all(|device| device.usage.is_none()));
        assert!(complete_storage(&sysfs, &mut snapshot));
        for (filled, expected) in snapshot.devices.iter().zip(&complete.devices) {
            assert_eq!(filled.name, expected.name);
            assert_eq!(filled.usage, expected.usage, "{}", filled.name);
            assert_eq!(
                filled.backing_disks, expected.backing_disks,
                "{}",
                filled.name
            );
        }
    }

    #[test]
    fn requested_scan_delivers_a_tree_once_per_generation() {
        let root = std::env::temp_dir().join(format!("spctop-collector-{}", std::process::id()));
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/file"), vec![1u8; 8192]).unwrap();
        let mut collector = Collector::new(false).unwrap();
        let mut request = HomeRequest {
            root: root.to_string_lossy().into_owned(),
            scan: true,
            interval_minutes: 60,
            ..HomeRequest::default()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let tree = loop {
            let snapshot = collector.sample(&request).unwrap();
            if let Some(tree) = snapshot.home_tree {
                break tree;
            }
            assert!(Instant::now() < deadline, "scan did not finish");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(tree.generation, 1);
        assert!(tree.root_node().unwrap().files >= 1);
        request.known_generation = tree.generation;
        let again = collector.sample(&request).unwrap();
        assert!(again.home_tree.is_none(), "known trees are not resent");
        assert_eq!(again.home.unwrap().generation, 1);
        fs::remove_dir_all(&root).unwrap();
    }
}
