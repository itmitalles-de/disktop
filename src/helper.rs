//! Private, bounded stdio protocol between the unprivileged UI and helper.

use std::{
    fs,
    io::{BufRead, Read, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    model::{HomeRequest, Snapshot},
    procio::UserCache,
    scanner,
};

pub const HELPER_PATH: &str = "/usr/local/libexec/dwtop-collector";
pub const PROTOCOL_VERSION: u32 = 1;
/// Every valid UTF-8 home path fits, including worst-case JSON escaping of
/// quotes/backslashes, maximum integer fields and the newline terminator.
pub const MAX_REQUEST: usize = 2 * scanner::MAX_ROOT_BYTES + 512;
/// Snapshots with a full home tree stay far below this bound.
pub const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const REPLY_DEADLINE: Duration = Duration::from_secs(10);

/// Failures of the helper connection that the UI explains in its language.
/// `Display` is the stable English text for stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperError {
    NotInstalled,
    TimedOut,
    Exited,
    Interrupted,
    ResponseTooLarge,
    ProtocolMismatch,
}

impl std::fmt::Display for HelperError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotInstalled => "helper is not installed",
            Self::TimedOut => "helper timed out",
            Self::Exited => "helper exited; rerun scripts/setup-helper.sh after updating",
            Self::Interrupted => "helper interrupted",
            Self::ResponseTooLarge => "helper response exceeds limit",
            Self::ProtocolMismatch => {
                "helper protocol mismatch; rerun scripts/setup-helper.sh after updating dwtop"
            }
        })
    }
}

impl std::error::Error for HelperError {}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub home: HomeRequest,
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        if self.version != PROTOCOL_VERSION {
            bail!(HelperError::ProtocolMismatch);
        }
        if !self.home.root.is_empty() {
            scanner::validate_root(&self.home.root)?;
        }
        if self.home.interval_minutes > 1440 {
            bail!("invalid rescan interval");
        }
        if serde_json::to_vec(self)?.len() >= MAX_REQUEST {
            bail!("helper request exceeds limit");
        }
        Ok(())
    }
}

/// Validate the complete outgoing frame before writing even its first byte.
pub fn write_request(writer: &mut impl Write, request: &Request) -> Result<()> {
    request.validate()?;
    let bytes = serde_json::to_vec(request)?;
    if bytes.len() >= MAX_REQUEST {
        bail!("helper request exceeds limit");
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Snapshot {
        version: u32,
        snapshot: Box<Snapshot>,
    },
    Error {
        version: u32,
        message: String,
    },
}

/// Consume at most max+1 bytes, including the required newline terminator.
pub fn read_frame(reader: &mut impl BufRead, max: usize) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let read = reader
        .take((max + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if read > max || bytes.last() != Some(&b'\n') {
        bail!("helper protocol frame exceeds limit or is incomplete");
    }
    bytes.pop();
    Ok(Some(bytes))
}

pub fn write_reply(writer: &mut impl Write, reply: &Reply) -> Result<()> {
    let bytes = serde_json::to_vec(reply)?;
    if bytes.len() >= MAX_RESPONSE {
        bail!(HelperError::ResponseTooLarge);
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// Count serialized bytes without allocating a potentially oversized frame.
#[derive(Default)]
struct JsonSize(usize);

impl Write for JsonSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_size(value: &impl Serialize) -> Result<usize> {
    let mut size = JsonSize::default();
    serde_json::to_writer(&mut size, value)?;
    Ok(size.0)
}

/// Keep a complete Home report while fitting process and capacity rows into
/// the aggregate frame budget. Omitted rows are explicitly disclosed; collection
/// continues on the same connection instead of dropping into helper fallback.
pub fn bounded_snapshot(snapshot: Snapshot) -> Result<Reply> {
    let mut reply = Reply::Snapshot {
        version: PROTOCOL_VERSION,
        snapshot: Box::new(snapshot),
    };
    let size = json_size(&reply)?;
    if size < MAX_RESPONSE {
        return Ok(reply);
    }
    // Keep headroom for omission counters and source/message annotations.
    // Removing rows accounts for their exact escaped JSON size; the final
    // complete serialization is checked again before writing.
    let mut excess = size.saturating_sub(MAX_RESPONSE - 512);
    let Reply::Snapshot { snapshot, .. } = &mut reply else {
        unreachable!()
    };
    fn trim<T: Serialize>(rows: &mut Vec<T>, excess: &mut usize) -> Result<usize> {
        let original = rows.len();
        while *excess > 0 {
            let Some(row) = rows.pop() else {
                break;
            };
            *excess = excess.saturating_sub(json_size(&row)?.saturating_add(1));
        }
        Ok(original - rows.len())
    }
    let omitted = trim(&mut snapshot.processes, &mut excess)?;
    if omitted > 0 {
        snapshot.process_access.omitted = snapshot.process_access.omitted.saturating_add(omitted);
        snapshot.process_access.message.push_str(&format!(
            "; process data truncated: {omitted} rows omitted by response budget"
        ));
        snapshot.truncated.push("/proc process data".into());
    }
    // A very large mount table can itself exhaust the remaining budget
    // alongside Home. Preserve the complete bounded scan, disclose missing
    // metadata and keep the helper session alive in that case too.
    if trim(&mut snapshot.filesystems, &mut excess)? > 0 {
        snapshot.truncated.push("/proc/self/mountinfo".into());
    }
    if trim(&mut snapshot.devices, &mut excess)? > 0 {
        snapshot.truncated.push("/proc/diskstats".into());
    }
    snapshot.truncated.sort();
    snapshot.truncated.dedup();
    if json_size(&reply)? >= MAX_RESPONSE {
        // The maximum bounded Home tree fits by itself; a future expansion
        // of that contract must not emit an invalid frame.
        bail!(HelperError::ResponseTooLarge);
    }
    Ok(reply)
}

/// Only the fixed administrator-installed path is eligible. No environment or
/// PATH override can select an executable to receive read privileges.
pub fn trusted_helper() -> Result<bool> {
    let path = Path::new(HELPER_PATH);
    if !path.try_exists()? {
        return Ok(false);
    }
    for component in path.ancestors() {
        let metadata = fs::symlink_metadata(component)?;
        if metadata.file_type().is_symlink() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0
        {
            bail!(
                "helper path must be root-owned and not writable by other users: {}",
                component.display()
            );
        }
        if component == path && !metadata.is_file() {
            bail!("helper is not a regular file");
        }
    }
    Ok(true)
}

pub struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    users: UserCache,
}

impl Client {
    pub fn start() -> Result<Self> {
        if !trusted_helper()? {
            bail!(HelperError::NotInstalled);
        }
        let mut child = Command::new(HELPER_PATH)
            .arg("--stdio")
            // Helper configuration never comes from the user's environment.
            .env_clear()
            .env("LANG", "C")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting helper; rerun scripts/setup-helper.sh if access was revoked")?;
        let input = child.stdin.take().context("opening helper input")?;
        let output = child.stdout.take().context("opening helper output")?;
        Ok(Self {
            child,
            input: Some(input),
            output,
            users: UserCache::default(),
        })
    }

    pub fn sample(
        &mut self,
        home: &HomeRequest,
        interrupted: impl Fn() -> bool,
    ) -> Result<Snapshot> {
        let request = Request {
            version: PROTOCOL_VERSION,
            home: home.clone(),
        };
        request.validate()?;
        let input = self.input.as_mut().context("helper input closed")?;
        write_request(input, &request)?;
        let frame = self.read_response(interrupted)?;
        let reply: Reply = serde_json::from_slice(&frame).context("decoding helper response")?;
        match reply {
            Reply::Snapshot { version, snapshot } if version == PROTOCOL_VERSION => {
                let mut snapshot = *snapshot;
                // Names are resolved here, so NSS never runs with capabilities.
                self.users.resolve(&mut snapshot.processes);
                Ok(snapshot)
            }
            Reply::Error { version, message } if version == PROTOCOL_VERSION => bail!("{message}"),
            _ => bail!(HelperError::ProtocolMismatch),
        }
    }

    fn read_response(&mut self, interrupted: impl Fn() -> bool) -> Result<Vec<u8>> {
        let deadline = Instant::now() + REPLY_DEADLINE;
        let mut bytes = Vec::new();
        loop {
            if interrupted() {
                bail!(HelperError::Interrupted);
            }
            if Instant::now() >= deadline {
                bail!(HelperError::TimedOut);
            }
            let mut descriptor = libc::pollfd {
                fd: self.output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // A single reader owns this pipe. After readiness, read returns the
            // available bytes without waiting for a complete frame.
            let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 {
                continue;
            }
            let mut chunk = [0u8; 65536];
            let count = self.output.read(&mut chunk)?;
            if count == 0 {
                bail!(HelperError::Exited);
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.len() > MAX_RESPONSE {
                bail!(HelperError::ResponseTooLarge);
            }
            if let Some(end) = chunk[..count].iter().position(|&byte| byte == b'\n') {
                if end + 1 != count {
                    bail!("unexpected data after helper response");
                }
                bytes.pop();
                return Ok(bytes);
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.input.take();
        // This is our exclusively owned, headless child. Never leave a helper
        // running when its UI goes away, including on malformed responses.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn request(root: &str, version: u32) -> Request {
        Request {
            version,
            home: HomeRequest {
                root: root.into(),
                ..HomeRequest::default()
            },
        }
    }

    #[test]
    fn protocol_bounds_and_request_validation() {
        assert!(read_frame(&mut Cursor::new(b"12345\n"), 5).is_err());
        assert!(read_frame(&mut Cursor::new(b"{}"), 10).is_err());
        assert_eq!(
            read_frame(&mut Cursor::new(b"{}\n"), 3).unwrap(),
            Some(b"{}".to_vec())
        );
        assert!(read_frame(&mut Cursor::new(b""), 3).unwrap().is_none());
        for root in ["home", "/home/../etc", "/home\nx", "/home/"] {
            assert!(request(root, 1).validate().is_err(), "{root:?}");
        }
        assert!(request("/home", 1).validate().is_ok());
        assert!(request("", 1).validate().is_ok());
        assert!(request("/home", 2).validate().is_err());
        let mut long = request("/home", 1);
        long.home.interval_minutes = 1441;
        assert!(long.validate().is_err());
        // Unknown fields are rejected at both levels.
        assert!(
            serde_json::from_str::<Request>(r#"{"version":1,"home":{"root":"/","scan":false,"interval_minutes":5,"rescan":false,"known_generation":0},"command":"x"}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<Request>(r#"{"version":1,"home":{"root":"/","scan":false,"interval_minutes":5,"rescan":false,"known_generation":0,"follow":true}}"#)
                .is_err()
        );
        // A maximal valid request fits the frame limit.
        let maximal = Request {
            version: 1,
            home: HomeRequest {
                root: format!("/{}", "a".repeat(scanner::MAX_ROOT_BYTES - 1)),
                scan: true,
                interval_minutes: 1440,
                rescan: true,
                known_generation: u64::MAX,
            },
        };
        assert!(maximal.validate().is_ok());
        assert!(serde_json::to_vec(&maximal).unwrap().len() < MAX_REQUEST);
    }

    #[test]
    fn escaped_maximum_paths_fit_and_invalid_outgoing_frames_write_nothing() {
        // Components fit Linux NAME_MAX, but the valid 4096-byte path almost
        // doubles in size when serialized (the old 8192-byte frame failed).
        let mut root = String::from("/");
        while root.len() < scanner::MAX_ROOT_BYTES {
            if root.len() > 1 {
                root.push('/');
            }
            let left = scanner::MAX_ROOT_BYTES - root.len();
            root.push_str(&"\"".repeat(left.min(254)));
        }
        let mut value = request(&root, PROTOCOL_VERSION);
        value.home.known_generation = u64::MAX;
        value.home.interval_minutes = 1440;
        let mut output = Vec::new();
        write_request(&mut output, &value).unwrap();
        assert!(output.len() > 8192 && output.len() <= MAX_REQUEST);
        let bytes = read_frame(&mut Cursor::new(output), MAX_REQUEST)
            .unwrap()
            .unwrap();
        let parsed: Request = serde_json::from_slice(&bytes).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.home.root, root);
        let mut output = Vec::new();
        assert!(write_request(&mut output, &request("relative", 1)).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn oversized_process_replies_remain_partial_and_allow_another_frame() {
        let row = crate::model::ProcessRow {
            command: "\\xff".repeat(4096),
            ..crate::model::ProcessRow::default()
        };
        let snapshot = Snapshot {
            processes: vec![row; 1650],
            process_access: crate::model::ProcessAccess {
                privileged: true,
                readable: 1650,
                ..crate::model::ProcessAccess::default()
            },
            ..Snapshot::default()
        };
        let reply = bounded_snapshot(snapshot).unwrap();
        let Reply::Snapshot { snapshot, .. } = &reply else {
            unreachable!()
        };
        assert!(!snapshot.processes.is_empty() && snapshot.processes.len() < 1650);
        assert_eq!(
            snapshot.process_access.omitted + snapshot.processes.len(),
            1650
        );
        assert_eq!(snapshot.process_access.readable, 1650);
        let mut stream = Vec::new();
        write_reply(&mut stream, &reply).unwrap();
        write_reply(&mut stream, &bounded_snapshot(Snapshot::default()).unwrap()).unwrap();
        let mut reader = Cursor::new(stream);
        for _ in 0..2 {
            let bytes = read_frame(&mut reader, MAX_RESPONSE).unwrap().unwrap();
            assert!(matches!(
                serde_json::from_slice::<Reply>(&bytes).unwrap(),
                Reply::Snapshot { .. }
            ));
        }
        assert!(read_frame(&mut reader, MAX_RESPONSE).unwrap().is_none());
    }

    #[test]
    fn largest_home_tree_reply_with_identity_keys_fits_the_response_limit() {
        use crate::model::{DirNode, Folded, HomeReport};
        // Worst case per node: a name of invalid bytes at the name cap, every
        // number at its maximum, a full 255-byte filename identity, and the
        // default node budget. Identity does not depend on the display cap.
        let limits = scanner::Limits::default();
        let name = "\\xff".repeat(scanner::MAX_NAME_CHARS / 4);
        let folded = Folded {
            dirs: u64::MAX,
            allocated: u64::MAX,
            apparent: u64::MAX,
            files: u64::MAX,
        };
        let count = limits.max_nodes;
        let nodes: Vec<DirNode> = (0..count)
            .map(|index| DirNode {
                name: name.clone(),
                key: Some("ff".repeat(255)),
                children: if index == 0 {
                    (1..count as u32).collect()
                } else {
                    Vec::new()
                },
                allocated: u64::MAX,
                apparent: u64::MAX,
                files: u64::MAX,
                dirs: u64::MAX,
                own_files: folded.clone(),
                other: folded.clone(),
                unreadable: u64::MAX,
                truncated: true,
                growth_previous: Some(i64::MIN),
                growth_start: Some(i64::MIN),
            })
            .collect();
        let snapshot = Snapshot {
            home_tree: Some(HomeReport {
                root: format!("/{}", "a".repeat(scanner::MAX_ROOT_BYTES - 1)),
                nodes,
                ..HomeReport::default()
            }),
            ..Snapshot::default()
        };
        let reply = Reply::Snapshot {
            version: PROTOCOL_VERSION,
            snapshot: Box::new(snapshot),
        };
        // Keep headroom for the rest of the snapshot without raising the
        // protocol limit. Uncapped display names would exceed that limit.
        let size = serde_json::to_vec(&reply).unwrap().len();
        assert!(size < MAX_RESPONSE * 9 / 10, "{size} bytes");
        let mut out = Vec::new();
        write_reply(&mut out, &reply).unwrap();
        let Reply::Snapshot { mut snapshot, .. } = reply else {
            unreachable!()
        };
        let row = crate::model::ProcessRow {
            command: "\\xff".repeat(4096),
            ..crate::model::ProcessRow::default()
        };
        snapshot.processes = vec![row; 1650];
        snapshot.process_access.readable = snapshot.processes.len();
        snapshot.process_access.privileged = true;
        // Capacity metadata can also overflow the residual space. This must
        // disclose missing mounts instead of sacrificing the Home tree.
        snapshot.filesystems = vec![
            crate::model::Filesystem {
                mount_point: "\"".repeat(4096),
                ..crate::model::Filesystem::default()
            };
            2000
        ];
        assert!(json_size(&snapshot).unwrap() > MAX_RESPONSE);
        let bounded = bounded_snapshot(*snapshot).unwrap();
        let mut output = Vec::new();
        write_reply(&mut output, &bounded).unwrap();
        assert!(output.len() <= MAX_RESPONSE);
        let decoded: Reply = serde_json::from_slice(&output).unwrap();
        let Reply::Snapshot { snapshot, .. } = decoded else {
            unreachable!()
        };
        assert!(snapshot.processes.len() < 1650);
        assert!(snapshot.filesystems.len() < 2000);
        assert!(
            snapshot
                .truncated
                .iter()
                .any(|source| source == "/proc/self/mountinfo")
        );
        assert_eq!(
            snapshot.home_tree.unwrap().nodes.len(),
            count,
            "keep the whole Home tree"
        );
        assert!(snapshot.process_access.privileged);
        assert!(snapshot.process_access.message.contains("rows omitted"));
        assert_eq!(snapshot.process_access.readable, 1650);
        assert_eq!(
            snapshot.process_access.omitted,
            1650 - snapshot.processes.len()
        );
        assert!(
            snapshot
                .truncated
                .iter()
                .any(|source| source == "/proc process data")
        );
    }
}
