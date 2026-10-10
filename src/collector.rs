//! Linux collection: /proc/diskstats, sysfs, mountinfo, statvfs, /proc/PID/io
//! and the optional home scanner. The same code runs in the unprivileged UI
//! and in the helper; only the helper sees every process and directory.

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::Read,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::{
    diskstats::{self, Counters, Info, Sysfs},
    model::{
        Device, DeviceUsage, Filesystem, HomeRequest, HomeStatus, JSON_VERSION, ScanState, Snapshot,
    },
    mounts::{self, Mount},
    privilege,
    procio::ProcessTable,
    scanner::{self, HomeService, Limits},
    text::sanitize,
};

const MAX_PSEUDO_FILE: u64 = 1024 * 1024;
const MAX_DEVICES: usize = 4096;
const MAX_DEVICE_MOUNTS: usize = 32;

pub fn read_pseudo(path: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("opening {path}"))?
        .take(MAX_PSEUDO_FILE)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {path}"))?;
    Ok(bytes)
}

struct Previous {
    major: u32,
    minor: u32,
    counters: Counters,
}

pub struct Collector {
    sysfs: Sysfs,
    started: Instant,
    last: Instant,
    previous: HashMap<String, Previous>,
    infos: HashMap<String, Info>,
    processes: ProcessTable,
    privileged: bool,
    home: Option<HomeService>,
    limits: Limits,
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
        let mut collector = Self {
            sysfs: Sysfs::default(),
            started: Instant::now(),
            last: Instant::now(),
            previous: HashMap::new(),
            infos: HashMap::new(),
            processes: ProcessTable::new("/proc", !helper),
            privileged,
            home: None,
            limits,
        };
        // Establish baselines so the first visible sample has rates.
        collector
            .sample(&HomeRequest::default())
            .context("cannot read Linux block device counters")?;
        Ok(collector)
    }

    pub fn sample(&mut self, request: &HomeRequest) -> Result<Snapshot> {
        let stats = read_pseudo("/proc/diskstats")?;
        let now = Instant::now();
        let seconds = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        let mut lines = diskstats::parse(&String::from_utf8_lossy(&stats));
        lines.truncate(MAX_DEVICES);
        // Hot-plug: static facts are read once per device name.
        self.infos
            .retain(|name, _| lines.iter().any(|line| &line.name == name));
        for line in &lines {
            if !self.infos.contains_key(&line.name) {
                self.infos
                    .insert(line.name.clone(), self.sysfs.info(&line.name));
            }
        }
        let mount_list = read_pseudo("/proc/self/mountinfo")
            .map(|bytes| mounts::parse(&bytes))
            .unwrap_or_default();
        let by_number: HashMap<(u32, u32), &str> = lines
            .iter()
            .map(|line| ((line.major, line.minor), line.name.as_str()))
            .collect();
        let mount_device = |mount: &Mount| -> Option<String> {
            if mount.major != 0 {
                return by_number
                    .get(&(mount.major, mount.minor))
                    .map(|name| (*name).to_owned());
            }
            // btrfs and other multi-device filesystems report an anonymous
            // major 0; their source names the block device.
            mount
                .source
                .starts_with("/dev/")
                .then(|| fs::canonicalize(&mount.source).ok())
                .flatten()
                .and_then(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .filter(|name| self.infos.contains_key(name))
        };
        let mut mounts_by_device: HashMap<String, BTreeSet<String>> = HashMap::new();
        for mount in &mount_list {
            if let Some(device) = mount_device(mount) {
                mounts_by_device
                    .entry(device)
                    .or_default()
                    .insert(mounts::display(&mount.mount_point));
            }
        }
        let mut devices = Vec::with_capacity(lines.len());
        let mut members = Vec::with_capacity(lines.len());
        let mut current = HashMap::with_capacity(lines.len());
        for line in &lines {
            let info = self.infos.get(&line.name).cloned().unwrap_or_default();
            let rates = self
                .previous
                .get(&line.name)
                .filter(|old| old.major == line.major && old.minor == line.minor)
                .and_then(|old| diskstats::rates(&old.counters, &line.counters, seconds));
            let partitions: Vec<String> = self
                .infos
                .iter()
                .filter(|(_, other)| other.parent.as_deref() == Some(line.name.as_str()))
                .map(|(name, _)| name.clone())
                .collect();
            let mut device_mounts = BTreeSet::new();
            let descendants = self.sysfs.descendants(&line.name, &partitions);
            for name in &descendants {
                if let Some(points) = mounts_by_device.get(name) {
                    device_mounts.extend(points.iter().cloned());
                }
            }
            members.push(descendants);
            let backing_disks = if info.kind.is_stacked() {
                self.sysfs
                    .backing_disks(&line.name, &self.infos)
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
        let filesystems = filesystems(&mount_list, &devices, &request.root, &mount_device);
        apply_usage(&mut devices, &members, &filesystems);
        let (processes, process_access) = self.processes.sample(self.privileged);
        let (home, home_tree) = self.home(request);
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
        })
    }

    fn home(
        &mut self,
        request: &HomeRequest,
    ) -> (Option<HomeStatus>, Option<crate::model::HomeReport>) {
        if request.root.is_empty() {
            return (None, None);
        }
        if let Err(error) = scanner::validate_root(&request.root) {
            return (
                Some(HomeStatus {
                    root: sanitize(&request.root),
                    state: ScanState::Failed,
                    error: Some(format!("{error:#}")),
                    privileged: self.privileged,
                    ..HomeStatus::default()
                }),
                None,
            );
        }
        let root = PathBuf::from(&request.root);
        let interval = Duration::from_secs(u64::from(request.interval_minutes.clamp(1, 1440)) * 60);
        if request.scan && self.home.is_none() {
            match HomeService::start(
                root.clone(),
                interval,
                self.limits.clone(),
                self.privileged,
                privilege::prepare_scan_thread,
            ) {
                Ok(service) => self.home = Some(service),
                Err(error) => {
                    return (
                        Some(HomeStatus {
                            root: sanitize(&request.root),
                            state: ScanState::Failed,
                            error: Some(format!("cannot start scanner: {error}")),
                            privileged: self.privileged,
                            ..HomeStatus::default()
                        }),
                        None,
                    );
                }
            }
        }
        let Some(service) = &self.home else {
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

fn filesystems(
    mount_list: &[Mount],
    devices: &[Device],
    home_root: &str,
    mount_device: &dyn Fn(&Mount) -> Option<String>,
) -> Vec<Filesystem> {
    let storage: Vec<&Mount> = mount_list
        .iter()
        .filter(|mount| mounts::is_storage(mount))
        // Docker and others bind individual files such as /etc/hosts.
        .filter(|mount| {
            fs::symlink_metadata(&mount.mount_point).is_ok_and(|metadata| metadata.is_dir())
        })
        .collect();
    let home_mount = (!home_root.is_empty())
        .then(|| {
            let owned: Vec<Mount> = storage.iter().map(|mount| (*mount).clone()).collect();
            mounts::containing(&owned, Path::new(home_root)).map(|mount| mount.mount_point.clone())
        })
        .flatten();
    let mut seen: HashMap<(u32, u32, Vec<u8>), usize> = HashMap::new();
    let mut rows: Vec<Filesystem> = Vec::new();
    for mount in storage {
        let key = (
            mount.major,
            mount.minor,
            mount.root.as_os_str().as_bytes().to_vec(),
        );
        if let Some(index) = seen.get(&key) {
            // A bind mount of the same filesystem root; keep the shorter path
            // unless the duplicate is the mount that contains the home path.
            let row = &mut rows[*index];
            row.bind_mounts += 1;
            if home_mount.as_ref() == Some(&mount.mount_point) {
                row.mount_point = mounts::display(&mount.mount_point);
                row.contains_home = true;
            }
            continue;
        }
        let block_device = mount_device(mount);
        let usage = mounts::statvfs(&mount.mount_point);
        let rates = block_device
            .as_ref()
            .and_then(|name| devices.iter().find(|device| &device.name == name))
            .and_then(|device| device.rates.as_ref());
        seen.insert(key, rows.len());
        rows.push(Filesystem {
            mount_point: mounts::display(&mount.mount_point),
            source: mounts::display(&mount.source),
            fs_type: sanitize(&mount.fs_type),
            dev: format!("{}:{}", mount.major, mount.minor),
            block_device,
            read_only: mount.read_only,
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
            bind_mounts: 0,
            contains_home: home_mount.as_ref() == Some(&mount.mount_point),
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
        let members: Vec<BTreeSet<String>> = devices
            .iter()
            .map(|device| {
                let partitions: Vec<String> = devices
                    .iter()
                    .filter(|other| other.parent.as_deref() == Some(device.name.as_str()))
                    .map(|other| other.name.clone())
                    .collect();
                sysfs.descendants(&device.name, &partitions)
            })
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
    let storage: Vec<Mount> = mount_list.into_iter().filter(mounts::is_storage).collect();
    let mount = mounts::containing(&storage, Path::new(home_root))
        .or_else(|| mounts::containing(&storage, Path::new("/")))?;
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
    if !diskstats::valid_name(&name) {
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
            if let (Some(device), Some(_)) = (device, filesystem.total_bytes) {
                assert!(device.usage.as_ref().unwrap().filesystems >= 1, "{name}");
            }
        }
        // JSON round-trips through the helper protocol types.
        let json = serde_json::to_string(&snapshot.devices).unwrap();
        let back: Vec<Device> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), snapshot.devices.len());
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
        let root = std::env::temp_dir().join(format!("disktop-collector-{}", std::process::id()));
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
