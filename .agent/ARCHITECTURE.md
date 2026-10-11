# Architecture

`main.rs` owns CLI validation, source selection (helper, direct collector or
explicit demo), refresh timing, `--once`/`--json` output and terminal
restoration. `ui.rs` receives snapshots and draws a compact htop-style layout.
The default Space view shows used/free capacity and fill-level history per disk
above a filesystem table; Home shows directory sizes and growth. F4 opens the
view menu, including the secondary I/O view, which retains RD/WR/IO meters,
utilization history, detailed device statistics and the process table.
`App::overview_devices()` lists visible whole devices plus a focused partition;
dm/md devices backed by one shown disk fold into it unless `show_stacked`.
The pure `disk_layout()` reduces graph height, then meters and rows as space
shrinks; Space/Home use compact capacity blocks and short filesystem tables
release unused height to fill history. A "+N more" row and focus scrolling
expose overflow. Per-device `App::history` survives focus changes. RD/WR meters
in the I/O view share the
highest visible rate across all overview devices. Space views never substitute
I/O rates for capacity. Unavailable measurements remain explicit; `demo.rs`
contains the only synthetic data, used by explicit `--demo` and tests.

`config.rs` owns versioned preferences, XDG paths and atomic private writes;
CLI options, `NO_COLOR` and the unavailable-device fallback change only a
runtime copy, and F12 saves the file values plus fields the user touched.
Configuration precedence is dwtop, spctop, then disktop. Only an absent file
permits fallback; malformed or unsafe higher-priority files are preserved and
reported. Legacy files are read without modification. Explicit spctop I/O
preferences survive; only the old disktop process-first view migrates to Space.
Saving targets dwtop and preserves unknown fields.
`i18n.rs` provides English/German UI text; CLI, stderr and JSON stay English.
`shutdown.rs` turns SIGINT/SIGTERM/SIGHUP into cooperative exits so the
terminal is always restored. A hung-up terminal (POLLHUP/POLLERR on stdin)
ends the monitor like SIGHUP: `run()` polls stdin itself before letting
crossterm read, because crossterm 0.29 retries reads that return end of file
forever; a `dwtop-hangup` watchdog thread exits after two seconds if a
hangup slips into that window, and the dead terminal is not written to on exit; `ratatui::init` restores it on panics.

`collector.rs` samples everything for one snapshot: `diskstats.rs` parses
/proc/diskstats (14/18/20 fields, odd lines skipped), computes rates from
deltas (32-bit tick fields wrap; a decrease beyond a plausible wrap = reset,
rates `None`), caches sysfs facts per device name and major:minor but re-reads
size and dm/md label every sample, and resolves partitions (one map per
sample), holders (cached per sample) and slaves to map mounts to devices and
the home path to a whole disk. A bounded breadth-first traversal follows both
partitions and holders at every level, including partitioned md/dm holders.
Devices with slaves and bcache/drbd/zd/nbd are
kind `Stacked` (virtual). Multi-device btrfs maps to every member from
/sys/fs/btrfs/<uuid>/devices. `zfs.rs` discovers mounted pools asynchronously
through a trusted root-owned `zpool list -H -p -L -P -v` executable, with a
cleared environment, no child capabilities, NO_NEW_PRIVS and parent-death kill.
Queries have a 500 ms/1 MiB bound, one outstanding worker and a 5 s refresh.
Known leaf members map through the same partition/holder logic; pool allocation
is separate from quota-dependent dataset statvfs. Pending/failed refreshes keep
last known membership with unknown capacity. File-backed/unresolved vdevs are
reported, not inferred. No subprocess runs without a mounted ZFS dataset.
`DeviceUsage.accounting_keys` deduplicates block-backed filesystems and
`zfs:<pool>` allocations across disks/datasets. `block_devices` retains real
device names for compatibility; the UI falls back to it for older helpers.
Pseudo-files are read up to 16 MiB, cut at
the last complete line and listed in `Snapshot.truncated`. `probe.rs` keeps
blocking calls off the sampling thread: lstat/statvfs of storage mounts run in a
worker with a shared 0.5 s budget per sample. Each path has at most one
outstanding call; late results survive to the next sample. Launches rotate,
with eight calls within their deadline and 64 outstanding overall. Hung or
not-yet-probed mounts become `not_responding`. The home
path is canonicalized off-thread and cached; mount matching is lexical.
"All" aggregates non-virtual whole disks only. Each device's `usage`
(`model::DeviceUsage::from_filesystems`) sums the statvfs values of the
filesystems whose block device is the device, a partition or a holder, each
block device once; swap partitions come from /proc/swaps. `usage: None` means
not measured (older helper replies deserialize this way), so the helper
protocol and JSON stay at version 1. `collector::complete_storage` fills
missing `usage`/`backing_disks` in a helper reply without privileges (the
reply's own statvfs values, sysfs, /proc/swaps); `main.rs` then shows a
one-time "helper is older" notice. `mounts.rs` parses mountinfo
(octal escapes decoded, IDs and superblock options kept), drops over-mounted
mounts (`visible`), filters to storage filesystems (no pseudo, network or
generic FUSE mounts (fuseblk is eligible), no file bind mounts), dedupes bind mounts by (dev, root) and calls
statvfs. The home filesystem is the longest visible mount prefix of any type,
marked only if it is storage. `procio.rs` reads /proc/PID/stat, io and cmdline
with bounded reads, keys baselines by (pid, start time), computes rates from
per-thread /proc/PID/task/TID/io deltas keyed by (tid, start time) (read only
when the process total changed; >10,000 threads fall back to the total), counts
EACCES as hidden, resolves user names only outside the helper, and in the
helper applies /proc's hidepid to other users (1: comm only, 2: omitted).
Direct collection also detects hidepid: stat EACCES counts as unreadable,
while invisible processes have no fabricated total. The TUI warns even when
the kernel hides enumeration; gid exemptions retain the kernel's behavior.

`scanner.rs` runs the home scan in a `dwtop-scan` thread (idle I/O priority,
nice 10, only DAC_READ_SEARCH). It walks with fdopendir/readdir, fstatat
(AT_SYMLINK_NOFOLLOW) and openat (O_DIRECTORY|O_NOFOLLOW), re-checks dev/inode
after opening, skips other filesystems, dedupes hard links in a bounded set and
stops at entry, time, depth or cancellation limits. Retained directories live
in a slab: per directory a streaming top-K; global eviction divides nodes
equally between root-child subtrees, lending unused shares to subtrees with
more detail. Within each share, shallower then larger finished directories
win. Active ancestors count against their subtree; a keep_depth allowance
during the walk protects finished neighbours. Output obeys max_nodes; the
working slab stays below 2*max_nodes+keep_depth+1. Evicted totals fold into
parent summaries. If root children exceed top_children/max_nodes, some are
necessarily summarized. Streaming evictions are irreversible, so surviving
descendant names can still vary with traversal order.
`HomeService` rescans periodically or on request, numbers reports by
generation (only newer trees are sent), and annotates growth per retained path
(only for complete scans; budget stops, partial roots and hard-link tracking
exhaustion neither annotate growth nor replace a baseline). Folding detail
with exact totals remains eligible. Display names are capped at 256 characters;
an optional hex-encoded raw filename key (at most 510 ASCII bytes) identifies
paths for growth/navigation. Legacy helper nodes fall back to their names.
Breadcrumbs use display names, and new trees truncate navigation to the deepest
surviving prefix. `RootPolicy` decides how roots
open: `Follow` (UI, like du) or `Allowed` (helper: roots from the root-owned
/etc/dwtop/scan-roots, then /etc/spctop/scan-roots, then
/etc/disktop/scan-roots. Only a safely absent policy permits fallback; /home is
the default only if all three are absent. Invalid higher-priority policies fail
closed without consulting legacy files. Policy reads probe beyond 64 KiB and
reject oversized files, invalid UTF-8, invalid paths and more than 64 roots in
their entirety. A selected policy disappearing during loading denies scans
instead of enabling defaults. Roots are opened component by component with
O_NOFOLLOW). `collector::HomeScan` drives the service from requests; a root the
helper refuses (`HomeErrorKind::NotAllowed`) is scanned by the UI's own
unprivileged `HomeScan` (`Source::Helper`), marked `helper_refused`. Home and
helper errors carry kinds that `i18n.rs` translates.

`helper.rs` and `bin/dwtop-collector.rs` implement the optional root-owned
helper: fixed path and ownership checks, cleared environment, `--stdio` only,
versioned newline-delimited JSON with request (8 KiB) and response (32 MiB)
bounds, unknown fields rejected, a 10-second reply deadline with cancellation,
and kill+reap on drop. `privilege.rs` keeps DAC_READ_SEARCH+SYS_PTRACE in the
sampling thread and only DAC_READ_SEARCH in scanner threads, all with
NO_NEW_PRIVS. `scripts/setup-helper.sh` builds unprivileged and installs with
`cap_dac_read_search,cap_sys_ptrace=ep`: root copies the build root-only, checks
its digest, then sets group, mode and capabilities; it refuses shared primary
groups (`--allow-shared-group`) and moving access to another group
(`--allow-owner-change`). The administrator must trust the user's checkout.
`text.rs` sanitizes every kernel- or user-controlled string for the terminal and
formats units.

The public project website is a dependency-free static site in `site/`, deployed
by `.github/workflows/pages.yml` from `main`, independently of the Rust CI. Only
`site/` is uploaded to Pages; the custom domain is `disktop.wutz.io` (`site/CNAME`)
and each deployment probes the public HTTPS endpoint. The site uses local fonts
and explicit DEMO captures as intentional public product assets; local QA
evidence stays outside Git.
