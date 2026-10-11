//! Explicit synthetic data for `--demo` and tests. Never used in live mode;
//! every demo snapshot has `demo: true` and the UI labels it DEMO.

use crate::model::{
    Device, DeviceKind, DeviceRates, DeviceUsage, DirNode, Filesystem, Folded, HomeReport,
    HomeStatus, JSON_VERSION, ProcessAccess, ProcessRow, ScanState, Snapshot,
};

const GIB: u64 = 1024 * 1024 * 1024;

pub fn snapshot(tick: u64, elapsed: f64, home_root: &str, known_generation: u64) -> Snapshot {
    let wave = |offset: f64, max: f64| {
        let t = tick as f64 * 0.23 + offset;
        (0.12 + (t.sin() * 0.5 + 0.5) * 0.7 + (t * 2.6).sin().abs() * 0.18) * max
    };
    let read = wave(0.0, 180.0 * 1024.0 * 1024.0);
    let write = wave(1.9, 64.0 * 1024.0 * 1024.0);
    let rates = |read: f64, write: f64, scale: f64| DeviceRates {
        read_rate: read,
        write_rate: write,
        read_iops: read / 65536.0,
        write_iops: write / 32768.0,
        utilization_percent: ((read + write) / scale * 100.0).min(100.0),
        await_ms: Some(0.18 + (read + write) / scale),
        read_await_ms: Some(0.12 + read / scale),
        write_await_ms: Some(0.3 + write / scale),
        queue_depth: (read + write) / scale * 2.0,
    };
    let disk = |name: &str, kind, model: &str, size: u64, mounts: &[&str], rates| Device {
        name: name.into(),
        kind,
        model: Some(model.into()),
        size_bytes: Some(size),
        rotational: Some(kind == DeviceKind::Hdd),
        hw_sector_size: Some(512),
        mounts: mounts.iter().map(|mount| (*mount).into()).collect(),
        rates: Some(rates),
        read_bytes: 9_400 * GIB + (elapsed * 1e8) as u64,
        write_bytes: 6_100 * GIB + (elapsed * 4e7) as u64,
        reads: 81_000_000 + tick * 900,
        writes: 52_000_000 + tick * 400,
        ..Device::default()
    };
    // nvme1n1 fills up over about 100 seconds, then is cleaned up again.
    let filling = 0.40 + 0.58 * ((tick % 100) as f64 / 100.0);
    let mut devices = vec![
        disk(
            "nvme0n1",
            DeviceKind::Nvme,
            "Example NVMe SSD 2TB",
            2000 * GIB,
            &["/", "/boot/efi", "/home"],
            rates(read, write, 2.8e8),
        ),
        Device {
            name: "nvme0n1p2".into(),
            kind: DeviceKind::Nvme,
            partition: true,
            parent: Some("nvme0n1".into()),
            mounts: vec!["/home".into()],
            size_bytes: Some(1500 * GIB),
            rates: Some(rates(read * 0.8, write * 0.7, 2.8e8)),
            ..Device::default()
        },
        disk(
            "nvme1n1",
            DeviceKind::Nvme,
            "Example NVMe SSD 1TB",
            1000 * GIB,
            &["/data"],
            rates(wave(5.0, 22e6), wave(0.7, 140e6), 4.0e8),
        ),
        // LUKS on nvme1n1: hidden in the overview, its mount counts on nvme1n1.
        Device {
            name: "dm-0".into(),
            kind: DeviceKind::Dm,
            label: Some("luks-data".into()),
            mounts: vec!["/data".into()],
            size_bytes: Some(1000 * GIB),
            rates: Some(rates(wave(5.0, 22e6), wave(0.7, 140e6), 4.0e8)),
            backing_disks: vec!["nvme1n1".into()],
            ..Device::default()
        },
        disk(
            "sda",
            DeviceKind::Hdd,
            "Example HDD 8TB",
            8000 * GIB,
            &["/srv/backup"],
            rates(wave(3.0, 9e6), wave(4.0, 22e6), 3.5e7),
        ),
        Device {
            removable: true,
            ..disk(
                "sdb",
                DeviceKind::Ssd,
                "Example USB SSD 4TB",
                4000 * GIB,
                &[],
                DeviceRates::default(),
            )
        },
        Device {
            name: "loop0".into(),
            kind: DeviceKind::Loop,
            size_bytes: Some(70 * 1024 * 1024),
            rates: Some(DeviceRates::default()),
            ..Device::default()
        },
    ];
    let processes = [
        (
            2311,
            "tim",
            "cargo",
            "cargo build --release",
            read * 0.46,
            write * 0.38,
        ),
        (
            8842,
            "tim",
            "brave",
            "/opt/brave/brave --type=renderer",
            read * 0.12,
            write * 0.08,
        ),
        (
            412,
            "root",
            "jbd2/nvme0n1p2",
            "[jbd2/nvme0n1p2]",
            0.0,
            write * 0.11,
        ),
        (
            1650,
            "root",
            "restic",
            "restic backup /home",
            read * 0.31,
            write * 0.02,
        ),
        (
            977,
            "postgres",
            "postgres",
            "postgres: checkpointer",
            8192.0,
            write * 0.21,
        ),
        (3120, "tim", "rg", "rg --files /home/tim", read * 0.11, 0.0),
        (1, "root", "systemd", "/sbin/init splash", 0.0, 0.0),
    ]
    .into_iter()
    .map(|(pid, user, name, command, read, write)| ProcessRow {
        pid,
        uid: if user == "root" { 0 } else { 1000 },
        user: user.into(),
        name: name.into(),
        command: command.into(),
        read_rate: Some(read),
        write_rate: Some(write),
        cancelled_write_rate: Some(if name == "cargo" { write * 0.05 } else { 0.0 }),
        read_bytes: (read * (elapsed + 600.0)) as u64,
        write_bytes: (write * (elapsed + 600.0)) as u64,
        cancelled_write_bytes: 0,
    })
    .collect();
    let filesystem = |mount: &str,
                      source: &str,
                      fs_type: &str,
                      device: &str,
                      total: u64,
                      used: u64,
                      home: bool| {
        let rates = devices
            .iter()
            .find(|candidate| candidate.name == device)
            // Partitions without their own demo counters use their disk's.
            .or_else(|| {
                devices
                    .iter()
                    .find(|candidate| !candidate.partition && device.starts_with(&candidate.name))
            })
            .and_then(|device| device.rates.as_ref());
        Filesystem {
            mount_point: mount.into(),
            source: source.into(),
            fs_type: fs_type.into(),
            dev: "259:2".into(),
            block_device: Some(device.into()),
            read_only: false,
            total_bytes: Some(total),
            used_bytes: Some(used),
            available_bytes: Some(total.saturating_sub(used).saturating_sub(total / 50)),
            inodes_total: Some(total / 16384),
            inodes_used: Some(used / 65536),
            read_rate: rates.map(|rates| rates.read_rate),
            write_rate: rates.map(|rates| rates.write_rate),
            bind_mounts: 0,
            contains_home: home,
            ..Filesystem::default()
        }
    };
    let filesystems = vec![
        filesystem(
            "/",
            "/dev/nvme0n1p3",
            "ext4",
            "nvme0n1p3",
            480 * GIB,
            212 * GIB,
            false,
        ),
        filesystem(
            "/boot/efi",
            "/dev/nvme0n1p1",
            "vfat",
            "nvme0n1p1",
            GIB,
            GIB / 10,
            false,
        ),
        filesystem(
            "/home",
            "/dev/nvme0n1p2",
            "ext4",
            "nvme0n1p2",
            1500 * GIB,
            1310 * GIB,
            true,
        ),
        filesystem(
            "/data",
            "/dev/mapper/luks-data",
            "xfs",
            "dm-0",
            1000 * GIB,
            (1000.0 * filling) as u64 * GIB,
            false,
        ),
        filesystem(
            "/srv/backup",
            "/dev/sda1",
            "xfs",
            "sda1",
            8000 * GIB,
            5100 * GIB,
            false,
        ),
    ];
    for device in &mut devices {
        let mut members = std::collections::BTreeSet::from([device.name.clone()]);
        if device.name == "nvme1n1" {
            // The holder chain nvme1n1 -> nvme1n1p1 -> dm-0, as sysfs reports it.
            members.extend(["nvme1n1p1".to_owned(), "dm-0".to_owned()]);
        }
        let prefix = match device.name.as_str() {
            "nvme0n1" | "nvme1n1" => format!("{}p", device.name),
            "sda" | "sdb" => device.name.clone(),
            _ => String::new(),
        };
        if !prefix.is_empty() {
            members.extend(
                filesystems
                    .iter()
                    .filter_map(|fs| fs.block_device.clone())
                    .filter(|name| name.starts_with(&prefix)),
            );
        }
        device.usage = Some(DeviceUsage::from_filesystems(&filesystems, &members, false));
    }
    let generation = 1 + tick / 120;
    Snapshot {
        version: JSON_VERSION,
        demo: true,
        elapsed,
        interval: 1.0,
        devices,
        processes,
        process_access: ProcessAccess {
            privileged: true,
            readable: 7,
            unreadable: 0,
            omitted: 0,
            message: "DEMO data".into(),
        },
        filesystems,
        home: Some(HomeStatus {
            root: home_root.into(),
            state: ScanState::Done,
            generation,
            last_finished_unix: Some(1_760_000_000),
            next_scan_in_seconds: Some(240),
            privileged: true,
            ..HomeStatus::default()
        }),
        home_tree: (generation > known_generation).then(|| home_tree(home_root, generation)),
        truncated: Vec::new(),
    }
}

pub fn home_tree(root: &str, generation: u64) -> HomeReport {
    let node = |name: &str, gib: f64, files: u64, growth: i64, children: Vec<u32>| DirNode {
        name: name.into(),
        children,
        allocated: (gib * GIB as f64) as u64,
        apparent: (gib * 0.97 * GIB as f64) as u64,
        files,
        dirs: files / 12,
        own_files: Folded {
            files: 14,
            allocated: 3 * 1024 * 1024,
            apparent: 3 * 1024 * 1024,
            dirs: 0,
        },
        growth_previous: Some(growth),
        growth_start: Some(growth * 4),
        ..DirNode::default()
    };
    let mut nodes = vec![
        node(root, 1310.0, 2_400_000, 3_200_000_000, vec![1, 2, 3, 4]),
        node("tim", 902.5, 1_800_000, 2_900_000_000, vec![5, 6, 7, 8, 9]),
        node("anna", 301.2, 420_000, 280_000_000, vec![]),
        node("shared", 96.0, 160_000, 0, vec![]),
        node("guest", 0.4, 900, -12_000_000, vec![]),
        node(".cache", 212.0, 900_000, 1_700_000_000, vec![]),
        node("github", 188.4, 610_000, 640_000_000, vec![]),
        node("Videos", 301.0, 2100, 0, vec![]),
        node("Downloads", 96.6, 8_400, 420_000_000, vec![]),
        node(".local", 61.1, 120_000, 140_000_000, vec![]),
    ];
    nodes[3].unreadable = 3;
    nodes[1].other = Folded {
        dirs: 41,
        allocated: 40 * GIB,
        apparent: 39 * GIB,
        files: 160_000,
    };
    HomeReport {
        root: root.into(),
        generation,
        started_unix: 1_760_000_000,
        duration_ms: 41_200,
        entries: 2_560_000,
        stopped: None,
        other_filesystems: 1,
        hardlink_limit_reached: false,
        privileged: true,
        nodes,
    }
}
