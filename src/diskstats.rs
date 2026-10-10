//! Block device counters from /proc/diskstats and static facts from sysfs.
//!
//! Sector counts in /proc/diskstats always use 512-byte units, independent of
//! the device's logical or hardware sector size (Documentation/block/stat.rst).
//! `hw_sector_size` is reported for information only.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use crate::{
    model::{Device, DeviceKind, DeviceRates},
    text::sanitize,
};

pub const SECTOR_BYTES: u64 = 512;
const MAX_SYSFS_READ: u64 = 4096;
const MAX_STACK_DEPTH: usize = 8;

/// Cumulative counters of one /proc/diskstats line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub reads: u64,
    pub sectors_read: u64,
    pub read_ms: u64,
    pub writes: u64,
    pub sectors_written: u64,
    pub write_ms: u64,
    pub in_flight: u64,
    pub io_ms: u64,
    pub weighted_ms: u64,
    pub discards: u64,
    pub discard_ms: u64,
    pub flushes: u64,
    pub flush_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub major: u32,
    pub minor: u32,
    pub name: String,
    pub counters: Counters,
}

/// Parse /proc/diskstats. Malformed, short or oddly named lines are skipped;
/// kernels before 4.18 have 14 fields, newer ones 18 or 20.
pub fn parse(text: &str) -> Vec<Line> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Line> {
    let fields: Vec<&str> = line.split_ascii_whitespace().collect();
    if fields.len() < 14 {
        return None;
    }
    let major = fields[0].parse().ok()?;
    let minor = fields[1].parse().ok()?;
    let name = fields[2];
    if !valid_name(name) {
        return None;
    }
    let mut numbers = [0u64; 17];
    for (slot, field) in numbers.iter_mut().zip(&fields[3..]) {
        *slot = field.parse().ok()?;
    }
    // Indexes follow Documentation/admin-guide/iostats.rst, starting at field 1.
    let optional = |index: usize| {
        if fields.len() > index + 3 {
            numbers[index]
        } else {
            0
        }
    };
    Some(Line {
        major,
        minor,
        name: name.to_owned(),
        counters: Counters {
            reads: numbers[0],
            sectors_read: numbers[2],
            read_ms: numbers[3],
            writes: numbers[4],
            sectors_written: numbers[6],
            write_ms: numbers[7],
            in_flight: numbers[8],
            io_ms: numbers[9],
            weighted_ms: numbers[10],
            discards: optional(11),
            discard_ms: optional(14),
            flushes: optional(15),
            flush_ms: optional(16),
        },
    })
}

/// Kernel block device names are short and never contain '/'. Anything else
/// is rejected before it is used to build sysfs paths.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'/' && byte != b'\\')
}

/// The kernel prints the I/O and sector counters as `unsigned long`: 32 bits
/// on 32-bit kernels. A 32-bit build most likely runs on one.
const IO_COUNTERS_WRAP: bool = cfg!(target_pointer_width = "32");

/// Increase of a cumulative counter. The kernel prints the time fields
/// (read/write/io/weighted ticks) as 32-bit `%u` on every architecture, so
/// they wrap: weighted_ms about every 1.5 days at queue depth 32. With
/// `may_wrap`, a decrease from a value that fits in 32 bits counts as a wrap
/// when the wrapped increase is plausible (below 2^31); otherwise the device
/// was re-created or reset and `None` is returned.
fn counter_delta(old: u64, new: u64, may_wrap: bool) -> Option<u64> {
    if let Some(delta) = new.checked_sub(old) {
        return Some(delta);
    }
    let wrapped = (may_wrap && old <= u64::from(u32::MAX) && new <= u64::from(u32::MAX))
        .then(|| new + (1u64 << 32) - old)?;
    (wrapped < 1u64 << 31).then_some(wrapped)
}

/// Rates between two samples, or `None` if a cumulative counter went
/// backwards by more than a 32-bit wrap (device re-created, driver reset).
pub fn rates(previous: &Counters, current: &Counters, seconds: f64) -> Option<DeviceRates> {
    if !(seconds.is_finite() && seconds > 0.0) {
        return None;
    }
    let count = |old, new| counter_delta(old, new, IO_COUNTERS_WRAP);
    let ticks = |old, new| counter_delta(old, new, true);
    let reads = count(previous.reads, current.reads)?;
    let writes = count(previous.writes, current.writes)?;
    let sectors_read = count(previous.sectors_read, current.sectors_read)?;
    let sectors_written = count(previous.sectors_written, current.sectors_written)?;
    let read_ms = ticks(previous.read_ms, current.read_ms)?;
    let write_ms = ticks(previous.write_ms, current.write_ms)?;
    let io_ms = ticks(previous.io_ms, current.io_ms)?;
    let weighted_ms = ticks(previous.weighted_ms, current.weighted_ms)?;
    let milliseconds = seconds * 1000.0;
    let average = |ms: u64, count: u64| (count > 0).then(|| ms as f64 / count as f64);
    Some(DeviceRates {
        read_rate: (sectors_read.saturating_mul(SECTOR_BYTES)) as f64 / seconds,
        write_rate: (sectors_written.saturating_mul(SECTOR_BYTES)) as f64 / seconds,
        read_iops: reads as f64 / seconds,
        write_iops: writes as f64 / seconds,
        utilization_percent: (io_ms as f64 / milliseconds * 100.0).clamp(0.0, 100.0),
        await_ms: average(
            read_ms.saturating_add(write_ms),
            reads.saturating_add(writes),
        ),
        read_await_ms: average(read_ms, reads),
        write_await_ms: average(write_ms, writes),
        queue_depth: weighted_ms as f64 / milliseconds,
    })
}

/// Combined rates of physical whole disks. Partitions and stacked devices
/// (dm, md, loop, zram) are excluded because their I/O reappears below them.
/// Utilization is the busiest disk's value; await is weighted by I/O counts.
pub fn aggregate<'a>(devices: impl IntoIterator<Item = &'a Device>) -> Option<DeviceRates> {
    let mut total: Option<DeviceRates> = None;
    let (mut weighted, mut read_weighted, mut write_weighted) = (0.0, 0.0, 0.0);
    for device in devices
        .into_iter()
        .filter(|device| !device.partition && !device.kind.is_virtual())
    {
        let Some(rates) = &device.rates else {
            continue;
        };
        let sum = total.get_or_insert_with(DeviceRates::default);
        sum.read_rate += rates.read_rate;
        sum.write_rate += rates.write_rate;
        sum.read_iops += rates.read_iops;
        sum.write_iops += rates.write_iops;
        sum.queue_depth += rates.queue_depth;
        sum.utilization_percent = sum.utilization_percent.max(rates.utilization_percent);
        weighted += rates.await_ms.unwrap_or(0.0) * (rates.read_iops + rates.write_iops);
        read_weighted += rates.read_await_ms.unwrap_or(0.0) * rates.read_iops;
        write_weighted += rates.write_await_ms.unwrap_or(0.0) * rates.write_iops;
    }
    if let Some(sum) = &mut total {
        let ios = sum.read_iops + sum.write_iops;
        sum.await_ms = (ios > 0.0).then(|| weighted / ios);
        sum.read_await_ms = (sum.read_iops > 0.0).then(|| read_weighted / sum.read_iops);
        sum.write_await_ms = (sum.write_iops > 0.0).then(|| write_weighted / sum.write_iops);
    }
    total
}

/// Device facts from sysfs. `size_bytes` and `label` are refreshed every
/// sample; the rest is cached per device number.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub kind: DeviceKind,
    pub partition: bool,
    pub parent: Option<String>,
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub label: Option<String>,
    pub size_bytes: Option<u64>,
    pub rotational: Option<bool>,
    pub removable: bool,
    pub hw_sector_size: Option<u32>,
}

pub struct Sysfs {
    root: PathBuf,
}

impl Default for Sysfs {
    fn default() -> Self {
        Self::at("/sys")
    }
}

impl Sysfs {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn block(&self, name: &str) -> PathBuf {
        self.root.join("class/block").join(name)
    }

    fn read(&self, name: &str, file: &str) -> Option<String> {
        let value = read_bounded(&self.block(name).join(file))?;
        let value = value.trim();
        (!value.is_empty()).then(|| sanitize(value))
    }

    pub fn info(&self, name: &str) -> Info {
        let partition = self.block(name).join("partition").exists();
        let parent = if partition {
            fs::canonicalize(self.block(name))
                .ok()
                .and_then(|path| {
                    path.parent()
                        .and_then(Path::file_name)
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .filter(|parent| valid_name(parent))
        } else {
            None
        };
        // Partitions have no queue; their disk's queue describes them.
        let disk = parent.as_deref().unwrap_or(name);
        let rotational =
            self.read(disk, "queue/rotational")
                .and_then(|value| match value.as_str() {
                    "0" => Some(false),
                    "1" => Some(true),
                    _ => None,
                });
        let kind = classify(disk, rotational, !self.slaves(disk).is_empty());
        let mut info = Info {
            kind,
            partition,
            model: self.read(disk, "device/model"),
            vendor: self
                .read(disk, "device/vendor")
                .filter(|vendor| vendor != "ATA"),
            label: None,
            size_bytes: None,
            rotational,
            removable: self.read(disk, "removable").as_deref() == Some("1"),
            hw_sector_size: self
                .read(disk, "queue/hw_sector_size")
                .and_then(|value| value.parse().ok()),
            parent,
        };
        self.refresh(name, &mut info);
        info
    }

    /// Re-read the facts that change while a device exists: media inserted
    /// into a card reader, lvextend or mdadm --grow, losetup on an existing
    /// loop device, a renamed dm device. Two small sysfs reads per device.
    pub fn refresh(&self, name: &str, info: &mut Info) {
        info.size_bytes = self
            .read(name, "size")
            .and_then(|value| value.parse::<u64>().ok())
            .map(|sectors| sectors.saturating_mul(SECTOR_BYTES));
        info.label = match info.kind {
            DeviceKind::Dm => self.read(name, "dm/name"),
            DeviceKind::Md => self.read(name, "md/level"),
            _ => None,
        };
    }

    /// Devices stacked on top of `name` (device mapper, MD), not including it.
    pub fn holders(&self, name: &str) -> Vec<String> {
        self.list(name, "holders")
    }

    /// Devices below `name`, for example a LUKS mapping's partition.
    pub fn slaves(&self, name: &str) -> Vec<String> {
        self.list(name, "slaves")
    }

    fn list(&self, name: &str, directory: &str) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.block(name).join(directory)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .take(256)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| valid_name(name))
            .collect();
        names.sort();
        names
    }

    /// Members of every multi-device btrfs filesystem, from
    /// /sys/fs/btrfs/<uuid>/devices: member name -> all members, sorted.
    /// Single-device filesystems are left out.
    pub fn btrfs_members(&self) -> HashMap<String, Vec<String>> {
        let mut members = HashMap::new();
        let Ok(filesystems) = fs::read_dir(self.root.join("fs/btrfs")) else {
            return members;
        };
        for filesystem in filesystems.filter_map(Result::ok).take(256) {
            let Ok(devices) = fs::read_dir(filesystem.path().join("devices")) else {
                continue; // "features" and other non-filesystem entries
            };
            let mut names: Vec<String> = devices
                .filter_map(Result::ok)
                .take(256)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| valid_name(name))
                .collect();
            if names.len() < 2 {
                continue;
            }
            names.sort();
            for name in &names {
                members.insert(name.clone(), names.clone());
            }
        }
        members
    }

    /// Physical whole disks below a device: a partition's disk, or the disks
    /// below a dm/md stack. Depth and fan-out are bounded.
    pub fn backing_disks(&self, name: &str, infos: &HashMap<String, Info>) -> Vec<String> {
        let mut found = BTreeSet::new();
        self.collect_backing(name, infos, 0, &mut found);
        found.into_iter().collect()
    }

    fn collect_backing(
        &self,
        name: &str,
        infos: &HashMap<String, Info>,
        depth: usize,
        found: &mut BTreeSet<String>,
    ) {
        if depth > MAX_STACK_DEPTH || found.len() > 64 {
            return;
        }
        if let Some(parent) = infos.get(name).and_then(|info| info.parent.as_deref()) {
            // A partition of a whole disk, or of an MD array (md0p1), whose
            // own disks lie further below.
            self.collect_backing(parent, infos, depth + 1, found);
            return;
        }
        let slaves = self.slaves(name);
        if slaves.is_empty() {
            found.insert(name.to_owned());
            return;
        }
        for slave in slaves {
            self.collect_backing(&slave, infos, depth + 1, found);
        }
    }

    /// The device itself, its partitions and everything stacked above them,
    /// including partitions of intermediate holders. `partitions` covers all
    /// devices in the sample, not just the starting disk.
    pub fn descendants(
        &self,
        name: &str,
        partitions: &HashMap<&str, Vec<String>>,
    ) -> BTreeSet<String> {
        self.descendants_cached(name, partitions, &mut HashMap::new())
    }

    /// `descendants` with a per-sample cache of holders listings, so each
    /// device's holders directory is read once per sample.
    pub fn descendants_cached(
        &self,
        name: &str,
        partitions: &HashMap<&str, Vec<String>>,
        holders: &mut HashMap<String, Vec<String>>,
    ) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let mut queued = BTreeSet::from([name.to_owned()]);
        let mut pending = VecDeque::from([(name.to_owned(), 0)]);
        while let Some((device, depth)) = pending.pop_front() {
            found.insert(device.clone());
            if depth >= MAX_STACK_DEPTH {
                continue;
            }
            let above = holders
                .entry(device.clone())
                .or_insert_with(|| self.holders(&device));
            let parts = partitions
                .get(device.as_str())
                .map_or(&[][..], Vec::as_slice);
            // Breadth first reaches shared devices through their shortest
            // path; cycles and the pending queue are bounded along with visits.
            for next in parts.iter().take(256).chain(above.iter().take(256)) {
                if queued.len() < 256 && queued.insert(next.clone()) {
                    pending.push_back((next.clone(), depth + 1));
                }
            }
        }
        found
    }
}

/// `has_slaves`: the device is built on other block devices (sysfs slaves/).
fn classify(name: &str, rotational: Option<bool>, has_slaves: bool) -> DeviceKind {
    let digits_after = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
    };
    if name.starts_with("loop") {
        DeviceKind::Loop
    } else if name.starts_with("zram") {
        DeviceKind::Zram
    } else if digits_after("ram") {
        DeviceKind::Ram
    } else if name.starts_with("dm-") {
        DeviceKind::Dm
    } else if digits_after("md") || name.starts_with("md_") {
        DeviceKind::Md
    } else if digits_after("sr") {
        DeviceKind::Optical
    } else if has_slaves
        || digits_after("zd")
        || digits_after("bcache")
        || digits_after("drbd")
        || digits_after("nbd")
    {
        // bcache, DRBD, ZFS volumes and network block devices: their I/O
        // reappears on the disks below them or is not local disk I/O.
        DeviceKind::Stacked
    } else if name.starts_with("nvme") {
        DeviceKind::Nvme
    } else {
        match rotational {
            Some(true) => DeviceKind::Hdd,
            Some(false) => DeviceKind::Ssd,
            None => DeviceKind::Other,
        }
    }
}

/// Read a small pseudo-file without trusting its reported length.
pub fn read_bounded(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_SYSFS_READ)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
 259       0 nvme0n1 1000 10 20000 300 2000 20 40000 600 0 800 900 0 0 0 0 50 5
 259       1 nvme0n1p1 900 0 18000 280 1900 0 38000 580 0 780 860 0 0 0 0 0 0
   8       0 sda 10 0 80 5 0 0 0 0 1 4 5
   7       0 loop0 1 0 2 0 0 0 0 0 0 0 0 0 0 0 0 0 0
 253       0 dm-0 5 0 10 1 6 0 12 2 0 3 3 0 0 0 0 0 0 0 0
   x       1 bad 1 2 3 4 5 6 7 8 9 10 11
   8      16 sdb 1 2 3
   8      32 ../evil 1 2 3 4 5 6 7 8 9 10 11
   8      48 sdc 1 2 3 4 5 6 7 8 9 10 -11

";

    #[test]
    fn parses_all_kernel_layouts_and_skips_odd_lines() {
        let lines = parse(SAMPLE);
        let names: Vec<_> = lines.iter().map(|line| line.name.as_str()).collect();
        assert_eq!(names, ["nvme0n1", "nvme0n1p1", "sda", "loop0", "dm-0"]);
        let nvme = &lines[0].counters;
        assert_eq!(nvme.reads, 1000);
        assert_eq!(nvme.sectors_read, 20000);
        assert_eq!(nvme.write_ms, 600);
        assert_eq!(nvme.io_ms, 800);
        assert_eq!(nvme.flushes, 50);
        assert_eq!(nvme.flush_ms, 5);
        // The 14-field layout of old kernels has no discard or flush fields.
        assert_eq!(lines[2].counters.in_flight, 1);
        assert_eq!(lines[2].counters.discards, 0);
        assert_eq!(parse("").len(), 0);
        assert_eq!(parse("\n\n   \n").len(), 0);
        assert!(!valid_name(".."));
        assert!(!valid_name("a/b"));
        assert!(valid_name("nvme0n1p1"));
        assert!(valid_name("cciss!c0d0"));
    }

    #[test]
    fn rates_use_512_byte_sectors_and_reject_counter_resets() {
        let previous = Counters {
            reads: 100,
            sectors_read: 1000,
            read_ms: 50,
            writes: 10,
            sectors_written: 2000,
            write_ms: 40,
            io_ms: 100,
            weighted_ms: 90,
            ..Counters::default()
        };
        let current = Counters {
            reads: 300,
            sectors_read: 1000 + 2048,
            read_ms: 150,
            writes: 110,
            sectors_written: 2000 + 4096,
            write_ms: 440,
            io_ms: 600,
            weighted_ms: 1090,
            ..Counters::default()
        };
        let rates = rates(&previous, &current, 2.0).unwrap();
        assert_eq!(rates.read_rate, 2048.0 * 512.0 / 2.0);
        assert_eq!(rates.write_rate, 4096.0 * 512.0 / 2.0);
        assert_eq!(rates.read_iops, 100.0);
        assert_eq!(rates.write_iops, 50.0);
        assert_eq!(rates.utilization_percent, 25.0);
        assert_eq!(rates.await_ms, Some(500.0 / 300.0));
        assert_eq!(rates.read_await_ms, Some(0.5));
        assert_eq!(rates.write_await_ms, Some(4.0));
        assert_eq!(rates.queue_depth, 0.5);
        // A reset (device re-created) or a 32-bit wrap is never a negative rate.
        let reset = Counters {
            sectors_written: 5,
            ..current
        };
        assert_eq!(super::rates(&current, &reset, 1.0), None);
        let wrapped = Counters {
            reads: u64::from(u32::MAX) - 1,
            ..Counters::default()
        };
        assert_eq!(
            super::rates(&wrapped, &Counters::default(), 1.0),
            None,
            "a wrapped 32-bit counter is a reset"
        );
        // Idle intervals have no await rather than a division by zero.
        let idle = super::rates(&current, &current, 1.0).unwrap();
        assert_eq!(idle.await_ms, None);
        assert_eq!(idle.utilization_percent, 0.0);
        for seconds in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(super::rates(&current, &current, seconds), None);
        }
        // io_ticks can exceed the interval slightly due to timing; clamp.
        let busy = Counters {
            io_ms: current.io_ms + 1500,
            ..current
        };
        assert_eq!(
            super::rates(&current, &busy, 1.0)
                .unwrap()
                .utilization_percent,
            100.0
        );
    }

    #[test]
    fn aggregate_counts_only_physical_whole_disks() {
        let device = |name: &str, kind, partition, read| Device {
            name: name.into(),
            kind,
            partition,
            rates: Some(DeviceRates {
                read_rate: read,
                read_iops: 10.0,
                await_ms: Some(2.0),
                read_await_ms: Some(2.0),
                utilization_percent: read / 100.0,
                ..DeviceRates::default()
            }),
            ..Device::default()
        };
        let devices = [
            device("nvme0n1", DeviceKind::Nvme, false, 1000.0),
            device("nvme0n1p1", DeviceKind::Nvme, true, 1000.0),
            device("sda", DeviceKind::Hdd, false, 3000.0),
            device("dm-0", DeviceKind::Dm, false, 900.0),
            device("loop0", DeviceKind::Loop, false, 500.0),
            Device {
                name: "sdb".into(),
                kind: DeviceKind::Ssd,
                ..Device::default()
            },
        ];
        let total = aggregate(&devices).unwrap();
        assert_eq!(total.read_rate, 4000.0);
        assert_eq!(total.read_iops, 20.0);
        assert_eq!(total.utilization_percent, 30.0);
        assert_eq!(total.await_ms, Some(2.0));
        assert_eq!(total.write_await_ms, None);
        assert_eq!(aggregate(&devices[3..5]), None);
    }

    #[test]
    fn sysfs_facts_partitions_and_stacks_from_a_fake_tree() {
        let root = std::env::temp_dir().join(format!("spctop-sysfs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let disk = root.join("devices/pci/nvme/nvme0n1");
        let part = disk.join("nvme0n1p2");
        let dm = root.join("devices/virtual/block/dm-0");
        for directory in [
            disk.join("queue"),
            disk.join("device"),
            disk.join("holders"),
            part.join("holders"),
            dm.join("dm"),
            dm.join("slaves"),
            dm.join("holders"),
            root.join("class/block"),
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(disk.join("queue/rotational"), "0\n").unwrap();
        fs::write(disk.join("queue/hw_sector_size"), "4096\n").unwrap();
        fs::write(disk.join("device/model"), "Fast SSD \x1b[2J  \n").unwrap();
        fs::write(disk.join("size"), "2048\n").unwrap();
        fs::write(disk.join("removable"), "0\n").unwrap();
        fs::write(part.join("partition"), "2\n").unwrap();
        fs::write(part.join("size"), "1024\n").unwrap();
        fs::write(dm.join("dm/name"), "luks-home\n").unwrap();
        std::os::unix::fs::symlink(&disk, root.join("class/block/nvme0n1")).unwrap();
        std::os::unix::fs::symlink(&part, root.join("class/block/nvme0n1p2")).unwrap();
        std::os::unix::fs::symlink(&dm, root.join("class/block/dm-0")).unwrap();
        std::os::unix::fs::symlink(&dm, part.join("holders/dm-0")).unwrap();
        std::os::unix::fs::symlink(&part, dm.join("slaves/nvme0n1p2")).unwrap();
        let sysfs = Sysfs::at(&root);
        let info = sysfs.info("nvme0n1");
        assert_eq!(info.kind, DeviceKind::Nvme);
        assert_eq!(info.model.as_deref(), Some("Fast SSD \\x1b[2J"));
        assert_eq!(info.size_bytes, Some(1024 * 1024));
        assert_eq!(info.hw_sector_size, Some(4096));
        let partition = sysfs.info("nvme0n1p2");
        assert!(partition.partition);
        assert_eq!(partition.parent.as_deref(), Some("nvme0n1"));
        assert_eq!(partition.rotational, Some(false));
        let mapper = sysfs.info("dm-0");
        assert_eq!(mapper.kind, DeviceKind::Dm);
        assert_eq!(mapper.label.as_deref(), Some("luks-home"));
        let infos: HashMap<_, _> = [
            ("nvme0n1".to_owned(), info),
            ("nvme0n1p2".to_owned(), partition),
            ("dm-0".to_owned(), mapper),
        ]
        .into();
        assert_eq!(sysfs.backing_disks("dm-0", &infos), ["nvme0n1"]);
        assert_eq!(sysfs.backing_disks("nvme0n1", &infos), ["nvme0n1"]);
        let descendants = sysfs.descendants(
            "nvme0n1",
            &HashMap::from([("nvme0n1", vec!["nvme0n1p2".into()])]),
        );
        assert_eq!(
            descendants.into_iter().collect::<Vec<_>>(),
            ["dm-0", "nvme0n1", "nvme0n1p2"]
        );
        // A missing device yields defaults instead of errors.
        assert_eq!(sysfs.info("sdz").kind, DeviceKind::Other);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(classify("md127", None, true), DeviceKind::Md);
        assert_eq!(classify("mdx", None, false), DeviceKind::Other);
        assert_eq!(classify("ram0", None, false), DeviceKind::Ram);
        assert_eq!(classify("zram0", None, false), DeviceKind::Zram);
        assert_eq!(classify("sda", Some(true), false), DeviceKind::Hdd);
        // Stacked devices without a dm/md name are virtual too.
        for name in ["zd0", "zd16", "bcache0", "drbd1", "nbd0"] {
            assert_eq!(
                classify(name, Some(false), false),
                DeviceKind::Stacked,
                "{name}"
            );
            assert!(DeviceKind::Stacked.is_virtual());
        }
        assert_eq!(classify("sdq", Some(false), true), DeviceKind::Stacked);
        assert_eq!(classify("nbdx", Some(false), false), DeviceKind::Ssd);
    }

    #[test]
    fn time_fields_wrap_at_32_bits_without_dropping_throughput() {
        let near = u64::from(u32::MAX) - 999;
        let previous = Counters {
            reads: 10,
            sectors_read: 100,
            read_ms: near,
            io_ms: near,
            weighted_ms: near,
            ..Counters::default()
        };
        let current = Counters {
            reads: 20,
            sectors_read: 300,
            // Wrapped past 2^32: 1000 to the top, then 500.
            read_ms: 500,
            io_ms: 500,
            weighted_ms: 1500,
            ..Counters::default()
        };
        let rates = rates(&previous, &current, 2.0).expect("a tick wrap is not a reset");
        assert_eq!(rates.read_rate, 200.0 * 512.0 / 2.0);
        assert_eq!(rates.read_await_ms, Some(150.0));
        assert_eq!(rates.utilization_percent, 75.0);
        assert_eq!(rates.queue_depth, 1.25);
        // An implausible decrease (more than half the 32-bit range) is a reset.
        let reset = Counters {
            weighted_ms: 5,
            ..previous
        };
        assert_eq!(
            super::rates(
                &Counters {
                    weighted_ms: 1_000_000_000,
                    ..previous
                },
                &reset,
                1.0
            ),
            None
        );
        assert_eq!(counter_delta(u64::from(u32::MAX), 9, true), Some(10));
        assert_eq!(counter_delta(u64::from(u32::MAX), 9, false), None);
        assert_eq!(
            counter_delta(1 << 40, 9, true),
            None,
            "64-bit values never wrap"
        );
        assert_eq!(counter_delta(1000, 10, true), None);
    }

    #[test]
    fn partitions_of_md_arrays_reach_the_disks_and_btrfs_members_are_listed() {
        let root = std::env::temp_dir().join(format!("spctop-sysfs-md-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let block = root.join("class/block");
        let md = root.join("devices/virtual/block/md0");
        let part = md.join("md0p1");
        let dm = root.join("devices/virtual/block/dm-0");
        let dm_part = dm.join("dm-0p1");
        let mut disks = Vec::new();
        for name in ["sda", "sdb"] {
            let disk = root.join("devices/pci/ata").join(name);
            fs::create_dir_all(disk.join("holders")).unwrap();
            fs::write(disk.join("size"), "100\n").unwrap();
            disks.push(disk);
        }
        for directory in [
            md.join("slaves"),
            md.join("holders"),
            md.join("md"),
            part.join("holders"),
            dm.join("slaves"),
            dm.join("holders"),
            dm.join("dm"),
            dm_part.join("holders"),
            block.clone(),
            root.join("fs/btrfs/features"),
            root.join("fs/btrfs/1234-uuid/devices"),
            root.join("fs/btrfs/5678-uuid/devices"),
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        for (name, disk) in ["sda", "sdb"].iter().zip(&disks) {
            let partition = disk.join(format!("{name}1"));
            fs::create_dir_all(partition.join("holders")).unwrap();
            fs::write(partition.join("partition"), "1\n").unwrap();
            std::os::unix::fs::symlink(disk, block.join(name)).unwrap();
            std::os::unix::fs::symlink(&partition, block.join(format!("{name}1"))).unwrap();
            std::os::unix::fs::symlink(&partition, md.join("slaves").join(format!("{name}1")))
                .unwrap();
            std::os::unix::fs::symlink(&md, partition.join("holders/md0")).unwrap();
            std::os::unix::fs::symlink(disk, root.join("fs/btrfs/1234-uuid/devices").join(name))
                .unwrap();
        }
        std::os::unix::fs::symlink(&disks[0], root.join("fs/btrfs/5678-uuid/devices/sda")).unwrap();
        fs::write(part.join("partition"), "1\n").unwrap();
        fs::write(dm_part.join("partition"), "1\n").unwrap();
        fs::write(md.join("md/level"), "raid1\n").unwrap();
        fs::write(md.join("size"), "100\n").unwrap();
        fs::write(dm.join("dm/name"), "vg-home\n").unwrap();
        fs::write(dm.join("size"), "50\n").unwrap();
        std::os::unix::fs::symlink(&md, block.join("md0")).unwrap();
        std::os::unix::fs::symlink(&part, block.join("md0p1")).unwrap();
        std::os::unix::fs::symlink(&dm, block.join("dm-0")).unwrap();
        std::os::unix::fs::symlink(&dm_part, block.join("dm-0p1")).unwrap();
        std::os::unix::fs::symlink(&part, dm.join("slaves/md0p1")).unwrap();
        std::os::unix::fs::symlink(&dm, part.join("holders/dm-0")).unwrap();
        let sysfs = Sysfs::at(&root);
        let infos: HashMap<String, Info> = [
            "sda", "sda1", "sdb", "sdb1", "md0", "md0p1", "dm-0", "dm-0p1",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), sysfs.info(name)))
        .collect();
        assert_eq!(infos["md0p1"].parent.as_deref(), Some("md0"));
        assert_eq!(infos["md0"].kind, DeviceKind::Md);
        // LVM on md0p1 is backed by the array's disks, not by md0.
        assert_eq!(sysfs.backing_disks("dm-0", &infos), ["sda", "sdb"]);
        assert_eq!(sysfs.backing_disks("md0p1", &infos), ["sda", "sdb"]);
        assert_eq!(sysfs.backing_disks("dm-0p1", &infos), ["sda", "sdb"]);
        let partitions = HashMap::from([
            ("sda", vec!["sda1".into()]),
            ("sdb", vec!["sdb1".into()]),
            ("md0", vec!["md0p1".into()]),
            ("dm-0", vec!["dm-0p1".into()]),
        ]);
        let mut holders = HashMap::new();
        for (disk, partition) in [("sda", "sda1"), ("sdb", "sdb1")] {
            let descendants = sysfs.descendants_cached(disk, &partitions, &mut holders);
            assert_eq!(
                descendants,
                [disk, partition, "md0", "md0p1", "dm-0", "dm-0p1"]
                    .map(str::to_owned)
                    .into()
            );
            let usage = crate::model::DeviceUsage::from_filesystems(
                &[
                    crate::model::Filesystem {
                        block_device: Some("md0p1".into()),
                        total_bytes: Some(100),
                        used_bytes: Some(50),
                        available_bytes: Some(50),
                        ..Default::default()
                    },
                    crate::model::Filesystem {
                        block_device: Some("dm-0p1".into()),
                        total_bytes: Some(40),
                        used_bytes: Some(10),
                        available_bytes: Some(30),
                        ..Default::default()
                    },
                ],
                &descendants,
                false,
            );
            assert_eq!((usage.filesystems, usage.used_bytes), (2, 60));
            assert_eq!(usage.block_devices, ["md0p1", "dm-0p1"]);
            assert_eq!(usage.accounting_keys, ["md0p1", "dm-0p1"]);
        }
        // Size and dm name follow resizes and renames without a new device.
        let mut mapper = infos["dm-0"].clone();
        assert_eq!(
            (mapper.size_bytes, mapper.label.as_deref()),
            (Some(50 * 512), Some("vg-home"))
        );
        fs::write(dm.join("size"), "80\n").unwrap();
        fs::write(dm.join("dm/name"), "vg-data\n").unwrap();
        sysfs.refresh("dm-0", &mut mapper);
        assert_eq!(
            (mapper.size_bytes, mapper.label.as_deref()),
            (Some(80 * 512), Some("vg-data"))
        );
        // Only multi-device btrfs filesystems are listed, per member.
        let members = sysfs.btrfs_members();
        assert_eq!(members.len(), 2);
        assert_eq!(members["sdb"], ["sda", "sdb"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn descendant_traversal_bounds_cycles_and_uses_shortest_shared_paths() {
        let sysfs = Sysfs::at(Path::new("/nonexistent-spctop-sysfs"));
        let mut partitions = HashMap::from([
            ("root", vec!["shortcut".to_owned()]),
            ("shortcut", vec!["shared".to_owned()]),
            ("shared", vec!["tail1".to_owned()]),
            ("tail1", vec!["tail2".to_owned()]),
            ("tail2", vec!["tail3".to_owned()]),
            ("tail3", vec!["tail4".to_owned()]),
            ("tail4", vec!["tail5".to_owned()]),
            ("tail5", vec!["tail6".to_owned()]),
            ("tail6", vec!["too-deep".to_owned()]),
        ]);
        let mut holders = HashMap::from([
            ("root".into(), vec!["deep1".into()]),
            ("deep1".into(), vec!["deep2".into()]),
            ("deep2".into(), vec!["deep3".into()]),
            ("deep3".into(), vec!["deep4".into()]),
            ("deep4".into(), vec!["deep5".into()]),
            ("deep5".into(), vec!["deep6".into()]),
            ("deep6".into(), vec!["deep7".into()]),
            ("deep7".into(), vec!["shared".into()]),
            ("shared".into(), vec!["root".into()]),
        ]);
        let descendants = sysfs.descendants_cached("root", &partitions, &mut holders);
        assert_eq!(descendants.len(), 16);
        assert!(descendants.contains("tail6"));
        assert!(!descendants.contains("too-deep"));
        partitions.insert(
            "root",
            (0..1000).map(|index| format!("part{index}")).collect(),
        );
        let bounded = sysfs.descendants_cached("root", &partitions, &mut holders);
        assert_eq!(bounded.len(), 256);
    }
}
