//! Linux snapshot collection shared by the UI and capability helper.
//! Bounded pseudo-file reads, capacity mapping and home scans have separate
//! modules; this module owns sampling order, caches and snapshot assembly.

mod home;
mod pseudo;
mod storage;
#[cfg(test)]
mod tests;

pub use home::HomeScan;
pub use pseudo::{MAX_PSEUDO_FILE, Pseudo, read_pseudo, read_pseudo_file};
use storage::{apply_usage, filesystems};
pub use storage::{complete_storage, default_device};

use crate::{
    diskstats::{self, Info, Sysfs},
    model::{Device, HomeRequest, JSON_VERSION, Snapshot},
    mounts::{self, Mount},
    probe::{MountProber, Resolver},
    procio::ProcessTable,
    scanner::{self, Limits, RootPolicy},
    text::sanitize,
    zfs,
};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::Path,
    time::Instant,
};

const MAX_DEVICES: usize = 4096;
const MAX_DEVICE_MOUNTS: usize = 32;

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
                Path::new(scanner::PREVIOUS_SCAN_ROOTS_PATH),
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
        let mut slaves = HashMap::new();
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
                    .backing_disks_cached(&line.name, infos, &mut slaves)
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
        if process_access.omitted > 0
            || process_access
                .message
                .contains("process count limit reached")
        {
            truncated.insert("/proc process data".to_owned());
        }
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
