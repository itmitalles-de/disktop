//! Filesystem capacity, device membership and older-helper completion.

use super::pseudo::read_pseudo;
use crate::{
    diskstats::{self, Info, Sysfs},
    model::{Device, DeviceRates, DeviceUsage, Filesystem, Snapshot},
    mounts::{self, Mount},
    probe::{self, MountProber, Outcome},
    text::sanitize,
};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::Duration,
};

/// Storage filesystems with their usage. `home` is the resolved home path.
/// Blocking calls (lstat, statvfs) go through `prober`, so a hung mount is
/// listed as not responding instead of stopping the sample.
pub(super) fn filesystems(
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
            Outcome::Done(probe) if probe.is_dir == Some(false) => continue,
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
pub(super) fn apply_usage(
    devices: &mut [Device],
    members: &[BTreeSet<String>],
    filesystems: &[Filesystem],
) {
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
        let mut slaves = HashMap::new();
        for device in &mut snapshot.devices {
            if device.kind.is_stacked() && device.backing_disks.is_empty() {
                device.backing_disks =
                    sysfs.backing_disks_cached(&device.name, &infos, &mut slaves);
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
