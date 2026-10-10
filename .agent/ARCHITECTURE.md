# Architecture

`main.rs` owns CLI validation, source selection (helper, direct collector or
explicit demo), refresh timing, `--once`/`--json` output and terminal
restoration. `ui.rs` receives snapshots and draws the nvtop-style layout:
title with view tabs, a four-line device header (summary, read, write and
utilization meters sharing one bar width), the stepped or Braille graph, an
optional status row and one of three tables (processes, filesystems, home). It
never derives or invents measurements. `demo.rs` holds the explicit synthetic
data for `--demo` and tests.

`config.rs` owns versioned preferences, XDG paths and atomic private writes;
CLI options, `NO_COLOR` and the unavailable-device fallback change only a
runtime copy, and F12 saves the file values plus fields the user touched.
`i18n.rs` provides English/German UI text; CLI, stderr and JSON stay English.
`shutdown.rs` turns SIGINT/SIGTERM/SIGHUP into cooperative exits so the
terminal is always restored; `ratatui::init` restores it on panics.

`collector.rs` samples everything for one snapshot: `diskstats.rs` parses
/proc/diskstats (14/18/20 fields, odd lines skipped), computes rates from
checked deltas (any decrease = reset, rates `None`), reads static device facts
from sysfs once per device name (hot-plug aware) and resolves partitions,
holders and slaves to map mounts to devices and the home path to a whole disk.
"All" aggregates non-virtual whole disks only. `mounts.rs` parses mountinfo
(octal escapes decoded), filters to storage filesystems (no pseudo, network or
FUSE mounts, no file bind mounts), dedupes bind mounts by (dev, root) and calls
statvfs. `procio.rs` reads /proc/PID/stat, io and cmdline with bounded reads,
keys baselines by (pid, start time), counts EACCES as hidden, and resolves user
names only outside the helper.

`scanner.rs` runs the home scan in a `disktop-scan` thread (idle I/O priority,
nice 10, only DAC_READ_SEARCH). It walks with fdopendir/readdir, fstatat
(AT_SYMLINK_NOFOLLOW) and openat (O_DIRECTORY|O_NOFOLLOW), re-checks dev/inode
after opening, skips other filesystems, dedupes hard links in a bounded set and
stops at entry, time, depth or cancellation limits. Retained directories live
in a slab: per directory an exact streaming top-K, and globally an eviction
that keeps shallower, then larger finished directories, so visit order does not
decide what is shown; everything evicted is folded into the parent's summary.
`HomeService` rescans periodically or on request, numbers reports by
generation (only newer trees are sent), and annotates growth per retained path.

`helper.rs` and `bin/disktop-collector.rs` implement the optional root-owned
helper: fixed path and ownership checks, cleared environment, `--stdio` only,
versioned newline-delimited JSON with request (8 KiB) and response (32 MiB)
bounds, unknown fields rejected, a 10-second reply deadline with cancellation,
and kill+reap on drop. `privilege.rs` keeps DAC_READ_SEARCH+SYS_PTRACE in the
sampling thread and only DAC_READ_SEARCH in scanner threads, all with
NO_NEW_PRIVS. `scripts/setup-helper.sh` builds unprivileged and installs with
`cap_dac_read_search,cap_sys_ptrace=ep` after digest and ownership checks.
`text.rs` sanitizes every kernel- or user-controlled string for the terminal and
formats units.
