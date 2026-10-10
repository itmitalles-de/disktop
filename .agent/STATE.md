# Verified state

- Repository `itmitalles-de/disktop` (GitHub, `main` @ 3d72ddf). Rust 1.88+,
  Linux only. License GPL-3.0-or-later: the owner relicensed from MIT on
  2026-10-10; the change is part of `feat/site` until it is merged. Built on the nettop codebase (config, i18n, shutdown,
  helper protocol, privilege, installer and PTY test patterns).
- Branch `feat/all-disks-overview` (worktree `../disktop-worktrees/all-disks`,
  uncommitted at hand-off): the main screen shows every disk at once, htop
  style. Per disk a block with RD/WR/IO/US (fill level) meters and a graph of
  IO utilization and fill level on one 0-100% scale; layout from `disk_layout()`
  degrades graphs -> meters -> one row per disk -> "+N more" with focus
  scrolling; compact process strip (Setup Table: 0-20 rows, default 5). New
  settings `process_rows`, `show_io_series`, `show_usage_series`,
  `usage_color` (older files get defaults). `Device.usage` added to JSON and
  the helper reply (additive, versions unchanged); `--once` lists all disks.
  F5/Tab/`--device` now set the focus. dm/md devices on a single shown disk
  (LUKS, single-disk LVM) are folded into that disk (`Device.backing_disks`;
  Setup Device › Show stacked devices, `show_stacked`, default off). A closed
  terminal (PTY hangup without or with SIGHUP, or as controlling terminal) now
  exits cleanly with code 0 instead of spinning in crossterm's read loop.
  An older installed helper (rig has the 3d72ddf helper installed) keeps
  working: the UI fills fill levels and backing disks itself and shows a
  one-time notice to rerun scripts/setup-helper.sh (verified on rig).
  Review: `../disktop-review/all-disks/`.
- Views: processes (default), filesystems, home sizes below the disk overview.
  F1 Help, F2 Setup (General, Device, Chart, Table, Home incl. editable home
  path), F3 search, F4/1/2/3 views, F5 focus, F6 sort, F9 pause, F10/q quit,
  F12 save. English/German with locale default, Setup toggle and persistence.
- 2026-10-10 checks, all passing: `cargo fmt --check`, Clippy `-D warnings`,
  51 Rust tests on stable and on Rust 1.88.0, release build, ShellCheck,
  `cargo audit` (108 crates, no advisories), `tests/terminal.py`,
  `tests/setup.py`, and in the pinned Ubuntu 24.04 container `tests/live_io.py`
  (separate writer/reader PIDs measured at exactly 48.0 MiB each) and
  `tests/helper.py` (installer, per-thread caps 0x80004 / scanner 0x4,
  NO_NEW_PRIVS, environment LANG=C only, root processes and private home
  directories visible only through the helper, protocol bounds, SIGTERM,
  10-second timeout fallback, EOF exit after UI SIGKILL).
- Real host runs (`--once`, `--json`, `--scan-home`) work without the helper;
  a scan of /home/tim/github (2.17 M files, 543 GiB) matched `du` sizes. The
  helper was not installed system-wide; `scripts/setup-helper.sh` was tested
  only inside the container.
- Review captures (PTY + pyte, EN/DE, 36x16 to 120x36, DEMO and live) are kept
  outside Git in `../disktop-review/` (`index.html`).

- 2026-10-10 all-disks checks (branch above), all passing: fmt, Clippy
  `-D warnings`, 57 Rust tests on stable and Rust 1.88.0 (layout matrix 36x16
  to 200x60 x 1/2/4/8/16 disks x EN/DE x three views), release build,
  ShellCheck, `tests/terminal.py`, `tests/setup.py` (new overview case), and in
  the container `tests/live_io.py` (incl. usage) and `tests/helper.py`. Real
  `--once`/`--json` on this host: nvme0n1 (LUKS dm-0), nvme1n1 match `df`.

- Branch `feat/site` (worktree `../disktop-worktrees/site`, based on
  `feat/all-disks-overview` 424dd63, uncommitted at hand-off): branded README
  (banner, self-hosted badges, explicit DEMO image, website link, license
  section) and `site/`, the dependency-free project website modeled on
  nettop's: strict CSP without inline code, local Chivo/IBM Plex Mono with OFL
  notices, system-aware light/dark theme and EN/DE, both with persisted
  toggles (`disktop-theme`, `disktop-lang`), accessible tabs, skip link, live
  region and reduced motion. Assets are actual `--demo` captures at 120x36
  (PTY + pyte render, see `site/assets/README.md`) and an MP4 recording.
  `.github/workflows/pages.yml` deploys only `site/` from `main` (same pinned
  actions as nettop), checks `site/CNAME` = `disktop.wutz.io` and probes the
  public HTTPS page after deploy. Review: `../disktop-review/site/`.

- Branch `fix/review-round-1` (worktree `../disktop-worktrees/review-round-1`,
  based on `main` a074399, uncommitted at hand-off): fixes the 23 findings of
  review round 1 (hung-mount probing, per-thread process rates, 32-bit tick
  wraps, refreshed sysfs facts, btrfs members, hidden mounts, 16 MiB pseudo
  reads, one RD/WR scale for all disks, helper scan-root allowlist
  /etc/disktop/scan-roots with O_NOFOLLOW walk and unprivileged fallback,
  hidepid in the helper, shared-group/owner-change checks and safer ordering in
  setup-helper.sh, width-aware truncation, translated backend errors and
  LANGUAGE, --once/--json helper fallback, weekly audit, site link check).
  Renders in `../disktop-review/round-1/`. The new JSON enum value `stacked`
  makes an older UI fail to decode a newer helper's reply (it then falls back
  to unprivileged collection); update both. Site screenshots still show the old
  per-disk RD/WR scaling.

## Open / not yet done

- Commit/PR for `feat/all-disks-overview` after owner review; then CI run.
  Track further work as GitHub issues in `itmitalles-de/disktop`.
- Website: GitHub Pages is planned with custom domain `disktop.wutz.io`, not
  configured yet; DNS is still to be set up. After merge: enable Pages (source
  GitHub Actions) with that domain, add a DNS-only Cloudflare CNAME `disktop` ->
  `itmitalles-de.github.io` (like nettop) and enforce HTTPS once the certificate
  is issued. The repository is private (2026-10-10); nettop's Pages site is
  served from a public repository, so visibility or the org plan must allow it.
  The first deploy's HTTPS probe fails until DNS and the certificate work.
- Overview: multi-device ZFS fill level counts only on the disk named by the
  mount source (btrfs members fixed on `fix/review-round-1`); swap detection only tested via unit data, not a real swap
  partition. nettop has the same terminal-hangup spin (fixed separately there).
- Not verified on real hardware: md RAID, zram, removable hot-plug during a
  session, HDDs; a desktop install via `scripts/setup-helper.sh`.
- Known limits: network/FUSE filesystems are not listed (statvfs can hang);
  bind mounts of the same filesystem inside the home tree are scanned twice
  (like `du -x`); processes exiting between refreshes are not listed; growth is
  per session and only for retained directories; 64-bit Linux targets only.
