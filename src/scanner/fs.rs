//! File-descriptor metadata shared by root policy and directory traversal.

use std::{ffi::CStr, io};

pub(super) fn stat_at(directory: libc::c_int, name: &CStr) -> io::Result<libc::stat> {
    // SAFETY: zeroed stat is a valid out-parameter; name is NUL-terminated.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            directory,
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

pub(super) fn stat_fd(descriptor: libc::c_int) -> io::Result<libc::stat> {
    // SAFETY: zeroed stat is a valid out-parameter for fstat.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(descriptor, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}
