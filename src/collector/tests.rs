use super::*;
use crate::{
    model::{DeviceKind, DeviceUsage, Filesystem, ScanState},
    probe,
};
use std::time::Duration;

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
        is_dir: Some(is_dir),
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

fn rows_for_usage(list: &[Mount], devices: &[Device], prober: &mut MountProber) -> Vec<Filesystem> {
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
    let path = std::env::temp_dir().join(format!("dwtop-pseudo-{}", std::process::id()));
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
    let root = std::env::temp_dir().join(format!("dwtop-helper-stacks-{}", std::process::id()));
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
    let root = std::env::temp_dir().join(format!("dwtop-collector-{}", std::process::id()));
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

#[test]
fn inaccessible_mounts_remain_visible_without_helper_measurements() {
    fn unavailable(path: &Path) -> probe::Probe {
        probe::Probe {
            is_dir: (path == Path::new("/file")).then_some(false),
            usage: None,
        }
    }
    let list = mounts::parse(
        b"1 0 8:1 / /inaccessible rw - ext4 /dev/sda1 rw\n\
2 0 8:2 / /file rw - ext4 /dev/sda2 rw\n",
    );
    let mut prober = MountProber::new(unavailable, Duration::from_millis(100));
    let rows = filesystems(
        &list,
        &[],
        Some(Path::new("/inaccessible/user")),
        &|mount| Some(format!("sda{}", mount.minor)),
        &|_| Vec::new(),
        &mut prober,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].mount_point, "/inaccessible");
    assert!(rows[0].contains_home);
    assert!(!rows[0].not_responding);
    assert!(rows[0].accounting_usage().is_none());
    let usage = DeviceUsage::from_filesystems(&rows, &BTreeSet::from(["sda1".into()]), false);
    assert_eq!((usage.filesystems, usage.unmeasured), (0, 1));
}
