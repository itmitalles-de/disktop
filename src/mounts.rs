//! Mounted filesystems from /proc/self/mountinfo and their usage via statvfs.

use std::{
    ffi::{CString, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
};

use crate::text::sanitize_bytes;

const MAX_MOUNTS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub major: u32,
    pub minor: u32,
    /// Root of the mount within its filesystem ("/" unless a bind mount).
    pub root: PathBuf,
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub source: PathBuf,
    pub read_only: bool,
}

/// Parse mountinfo. Octal escapes (\040 space, \011 tab, \012 newline, \134
/// backslash) are decoded to the original bytes; malformed lines are skipped.
pub fn parse(text: &[u8]) -> Vec<Mount> {
    text.split(|byte| *byte == b'\n')
        .filter_map(parse_line)
        .take(MAX_MOUNTS)
        .collect()
}

fn parse_line(line: &[u8]) -> Option<Mount> {
    let fields: Vec<&[u8]> = line
        .split(|byte| *byte == b' ')
        .filter(|field| !field.is_empty())
        .collect();
    // id parent major:minor root mount-point options [optional...] - type source super
    let separator = fields.iter().position(|field| *field == b"-")?;
    if separator < 6 || fields.len() < separator + 3 {
        return None;
    }
    let devnum = std::str::from_utf8(fields[2]).ok()?;
    let (major, minor) = devnum.split_once(':')?;
    let options = fields[5];
    Some(Mount {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
        root: PathBuf::from(OsString::from_vec(unescape(fields[3]))),
        mount_point: PathBuf::from(OsString::from_vec(unescape(fields[4]))),
        fs_type: String::from_utf8_lossy(&unescape(fields[separator + 1])).into_owned(),
        source: PathBuf::from(OsString::from_vec(unescape(fields[separator + 2]))),
        read_only: options
            .split(|byte| *byte == b',')
            .any(|flag| flag == b"ro"),
    })
}

pub fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\'
            && index + 3 < field.len()
            && field[index + 1..index + 4]
                .iter()
                .all(|digit| (b'0'..=b'7').contains(digit))
        {
            let value = field[index + 1..index + 4]
                .iter()
                .fold(0u32, |acc, digit| acc * 8 + u32::from(digit - b'0'));
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                index += 4;
                continue;
            }
        }
        out.push(field[index]);
        index += 1;
    }
    out
}

/// Pseudo and memory filesystems that do not live on a storage device.
const PSEUDO: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "overlay",
    "proc",
    "pstore",
    "ramfs",
    "rpc_pipefs",
    "securityfs",
    "selinuxfs",
    "sysfs",
    "tmpfs",
    "tracefs",
];

/// Network and FUSE filesystems are skipped because statvfs on an
/// unreachable server can block the monitor indefinitely.
const REMOTE: &[&str] = &[
    "9p",
    "afs",
    "ceph",
    "cifs",
    "davfs",
    "glusterfs",
    "ncpfs",
    "nfs",
    "nfs4",
    "smb3",
    "smbfs",
    "sshfs",
    "virtiofs",
];

/// Filesystems on storage: block-backed mounts plus ZFS datasets.
pub fn is_storage(mount: &Mount) -> bool {
    let fs_type = mount.fs_type.as_str();
    if PSEUDO.contains(&fs_type)
        || REMOTE.contains(&fs_type)
        || fs_type.starts_with("fuse.")
        || fs_type == "fuse"
    {
        return false;
    }
    mount.major != 0 || fs_type == "zfs" || fs_type == "btrfs" || fs_type == "fuseblk"
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub inodes_total: u64,
    pub inodes_free: u64,
}

pub fn statvfs(path: &Path) -> Option<Usage> {
    let name = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed statvfs is a valid out-parameter; name is NUL-terminated.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(name.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let unit = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    } as u64;
    let blocks = stat.f_blocks as u64;
    let free = (stat.f_bfree as u64).min(blocks);
    Some(Usage {
        total: blocks.saturating_mul(unit),
        used: (blocks - free).saturating_mul(unit),
        available: (stat.f_bavail as u64).min(free).saturating_mul(unit),
        inodes_total: stat.f_files as u64,
        inodes_free: (stat.f_ffree as u64).min(stat.f_files as u64),
    })
}

/// The mount whose mount point is the longest prefix of `path`. Paths are
/// compared component-wise, so /home2 never matches /home.
pub fn containing<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    let path = normalize(path);
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.mount_point))
        .max_by_key(|mount| mount.mount_point.components().count())
}

/// Resolve symlinks when possible (a /home symlink to /var/home is common);
/// otherwise keep the lexical path.
pub fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect()
    })
}

pub fn display(path: &Path) -> String {
    sanitize_bytes(path.as_os_str().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"\
22 1 259:4 / / rw,relatime shared:1 - ext4 /dev/nvme1n1p4 rw\n\
23 22 259:1 / /home rw,relatime shared:2 - ext4 /dev/nvme1n1p1 rw\n\
24 22 0:5 / /proc rw,nosuid - proc proc rw\n\
25 23 259:1 /tim/My\\040Drive /mnt/My\\040Drive\\012x ro,relatime - ext4 /dev/nvme1n1p1 rw\n\
26 22 0:33 / /btr rw - btrfs /dev/sdb1 rw\n\
27 22 0:40 / /tmp rw - tmpfs tmpfs rw\n\
28 22 0:41 / /net rw - nfs4 server:/export rw\n\
29 22 0:42 / /tank rw - zfs tank/data rw\n\
garbage line without separator\n\
30 22 x:y / /bad rw - ext4 /dev/sdc rw\n\
31 22 7:3 / /snap/core/1 ro shared:9 master:1 - squashfs /dev/loop3 ro\n\
32 22 259:5 / /back\\134slash rw - xfs /dev/nvme0n1p5 rw\n";

    #[test]
    fn parses_escapes_optional_fields_and_skips_garbage() {
        let mounts = parse(SAMPLE);
        let points: Vec<_> = mounts
            .iter()
            .map(|mount| mount.mount_point.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            points,
            [
                "/",
                "/home",
                "/proc",
                "/mnt/My Drive\nx",
                "/btr",
                "/tmp",
                "/net",
                "/tank",
                "/snap/core/1",
                "/back\\slash"
            ]
        );
        let bind = &mounts[3];
        assert_eq!(bind.root, Path::new("/tim/My Drive"));
        assert!(bind.read_only);
        assert_eq!(display(&bind.mount_point), "/mnt/My Drive\\x0ax");
        assert_eq!((mounts[1].major, mounts[1].minor), (259, 1));
        let snap = &mounts[8];
        assert_eq!(snap.fs_type, "squashfs");
        assert_eq!(snap.source, Path::new("/dev/loop3"));
        assert!(snap.read_only);
        assert_eq!(unescape(b"a\\040b\\1"), b"a b\\1");
        assert_eq!(
            unescape(b"\\777x"),
            b"\\777x",
            "out-of-range octal is literal"
        );
        assert_eq!(unescape(b"end\\04"), b"end\\04");
    }

    #[test]
    fn storage_filter_and_longest_prefix_match() {
        let mounts = parse(SAMPLE);
        let storage: Vec<_> = mounts
            .iter()
            .filter(|mount| is_storage(mount))
            .map(|mount| mount.mount_point.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            storage,
            [
                "/",
                "/home",
                "/mnt/My Drive\nx",
                "/btr",
                "/tank",
                "/snap/core/1",
                "/back\\slash"
            ]
        );
        // Lexical fallback for paths that do not exist on this machine.
        let root = containing(&mounts[..2], Path::new("/nonexistent-disktop/home/x")).unwrap();
        assert_eq!(root.mount_point, Path::new("/"), "not below /home");
        let lexical = [
            Mount {
                mount_point: "/".into(),
                ..mounts[0].clone()
            },
            Mount {
                mount_point: "/disktop-test-home".into(),
                ..mounts[1].clone()
            },
        ];
        let home = containing(&lexical, Path::new("/disktop-test-home/tim/./x")).unwrap();
        assert_eq!(home.mount_point, Path::new("/disktop-test-home"));
        let other = containing(&lexical, Path::new("/disktop-test-home2/tim")).unwrap();
        assert_eq!(other.mount_point, Path::new("/"));
    }

    #[test]
    fn statvfs_reports_consistent_usage_for_the_root_filesystem() {
        let usage = statvfs(Path::new("/")).expect("statvfs on /");
        assert!(usage.used <= usage.total);
        assert!(usage.available <= usage.total);
        assert!(statvfs(Path::new("/nonexistent-disktop-path")).is_none());
        assert!(statvfs(Path::new("a\0b")).is_none());
    }
}
