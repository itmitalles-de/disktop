//! Bounded reads of kernel pseudo-files.

use anyhow::{Context, Result};
use std::{fs, io::Read};

/// Large enough for mountinfo of container hosts with tens of thousands of
/// mounts; longer files are cut at their last complete line and reported.
pub const MAX_PSEUDO_FILE: usize = 16 * 1024 * 1024;

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
