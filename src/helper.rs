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

pub const HELPER_PATH: &str = "/usr/local/libexec/disktop-collector";
pub const PROTOCOL_VERSION: u32 = 1;
/// A request carries at most a 4 KiB home path plus small fields.
pub const MAX_REQUEST: usize = 8 * 1024;
/// Snapshots with a full home tree stay far below this bound.
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const REPLY_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub home: HomeRequest,
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        if self.version != PROTOCOL_VERSION {
            bail!("helper protocol mismatch; rerun scripts/setup-helper.sh after updating disktop");
        }
        if !self.home.root.is_empty() {
            scanner::validate_root(&self.home.root)?;
        }
        if self.home.interval_minutes > 1440 {
            bail!("invalid rescan interval");
        }
        Ok(())
    }
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
        bail!("helper response exceeds limit");
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
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
            bail!("helper is not installed");
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
        serde_json::to_writer(&mut *input, &request)?;
        input.write_all(b"\n")?;
        input.flush()?;
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
            _ => bail!("helper protocol mismatch; rerun scripts/setup-helper.sh"),
        }
    }

    fn read_response(&mut self, interrupted: impl Fn() -> bool) -> Result<Vec<u8>> {
        let deadline = Instant::now() + REPLY_DEADLINE;
        let mut bytes = Vec::new();
        loop {
            if interrupted() {
                bail!("helper interrupted");
            }
            if Instant::now() >= deadline {
                bail!("helper timed out");
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
                bail!("helper exited; rerun scripts/setup-helper.sh after updating");
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.len() > MAX_RESPONSE {
                bail!("helper response exceeds limit");
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
}
