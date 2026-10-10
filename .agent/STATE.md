# Verified state

- Public repository `itmitalles-de/disktop`, branch `main`, Linux/Rust 1.88+,
  GPL-3.0-or-later. Overview and website PRs #1/#2 are merged. Review round 1
  merged as [PR #3](https://github.com/itmitalles-de/disktop/pull/3) on
  2026-10-10 (`7a5fc33`, including `cff30b9` and test fix `1f63fe3`). The
  review worktree and local/remote `fix/review-round-1` branch were removed.
- The main screen shows every physical disk with RD/WR/IO/US meters and
  per-disk utilization/fill graphs. RD and WR share the highest rate across
  all displayed disks within visible history; IO and US share a 0-100% scale.
  Layout degrades graphs -> meters -> one row per disk -> "+N more" with
  focus scrolling. F5/Tab/`--device` select focus; `--once` lists all disks.
  Single-disk dm/md devices are folded into their physical disk unless
  `show_stacked` is enabled. Multi-device btrfs membership is resolved.
- Views: processes, filesystems and home sizes. F1 Help, F2 Setup, F3 search,
  F4/1/2/3 views, F5 focus, F6 sort, F9 pause, F10/q quit, F12 save.
  English/German with locale default and persisted Setup language. Process
  rows, chart series, usage color and home path are configurable. Closed PTYs
  exit cleanly, including without SIGHUP.
- Review fixes include isolated mount probing with bounded waits, per-thread
  process I/O deltas, 32-bit tick wraps, refreshed sysfs facts, hidden mounts,
  bounded pseudo-file reads, width-aware text, translated backend errors,
  helper hidepid handling and `--once`/`--json` fallback. CI includes weekly
  dependency auditing and website link checks.
- The optional root-owned helper uses DAC_READ_SEARCH and SYS_PTRACE; the UI
  stays unprivileged. Helper scans are limited to root-owned
  `/etc/disktop/scan-roots` (absent file: `/home` only), opened component by
  component with O_NOFOLLOW. Refused roots fall back to an unprivileged UI
  scan. Setup refuses shared primary groups and unapproved group reassignment.
  Update UI and helper together: the new JSON device kind `stacked` is not
  decodable by an older UI, which falls back to unprivileged collection.
- Host installation verified on 2026-10-10 with `scripts/install.sh` and
  `scripts/setup-helper.sh`: installed UI/helper SHA-256 hashes match builds
  from `7a5fc33`; helper is root:tim 0750 with the expected capabilities.
  `/etc/disktop/scan-roots` is absent, so the default applies. Live `--once`
  and `--json` show both NVMe disks, correctly mark `/home`, report all 1,229
  sampled processes readable and no nonresponding mounts. Brief live/demo
  PTY sessions quit cleanly; demo meters show the common RD/WR scale.
- Validation on 2026-10-10: local fmt, Clippy with warnings denied, all 71
  Rust tests, release install builds and `tests/terminal.py` passed. The
  shared-scale regression test passes. PR CI was green before merge; merge
  CI also passed (stable/MSRV, audit, terminal/setup, isolated live I/O/helper
  tests and ShellCheck). Hung NFS and real hidepid mounts remain unverified.
- `site/` is the public dependency-free website with local licensed fonts,
  explicit DEMO media, EN/DE and persisted system-aware light/dark controls.
  Pages uses GitHub Actions, `disktop.wutz.io`, and enforced HTTPS; the merge
  Pages deployment passed. Review evidence stays outside Git in
  `../disktop-review/`; round-1 before/after terminal text captures are in
  `../disktop-review/round-1/`.

## Open work and limits

- [#4](https://github.com/itmitalles-de/disktop/issues/4): refresh website,
  README and social/demo media for the shared RD/WR scale.
- [#5](https://github.com/itmitalles-de/disktop/issues/5): ZFS pool membership
  and usage attribution across disks; pool/dataset sources may currently
  have no disk attribution.
- [#6](https://github.com/itmitalles-de/disktop/issues/6): fairness between
  home subtrees under global scanner eviction; aggregate totals are retained.
- [#7](https://github.com/itmitalles-de/disktop/issues/7): real isolated NFS
  outage and /proc hidepid validation (currently unit coverage only).
- Hardware coverage still excludes real md RAID, zram, hot-plug, HDDs and a
  real swap partition. Network/generic FUSE mounts are filtered; fuseblk is
  eligible and protected by bounded probing. Same-filesystem bind mounts in
  a home tree can be scanned twice (like `du -x`); processes exiting between
  samples are absent. Growth is per session and retained directory, with
  incomplete scans excluded as baselines. Only 64-bit Linux is supported.
