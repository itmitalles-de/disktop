# Verified state

- New local repository `/home/tim/github/itmitalles-de/disktop` (branch `main`,
  no commits yet, no GitHub repository). MIT, Rust 1.88+, Linux only. Built on
  the nettop codebase (config, i18n, shutdown, helper protocol, privilege,
  installer and PTY test patterns); nettop itself was not modified.
- Views: processes (default), filesystems, home sizes; device header with
  read/write/utilization meters and a stepped or Braille graph. F1 Help, F2
  Setup (General, Device, Chart, Table, Home incl. editable home path), F3
  search, F4/1/2/3 views, F5 device, F6 sort, F9 pause, F10/q quit, F12 save.
  English/German with locale default, Setup toggle and persistence.
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

## Open / not yet done

- Create the GitHub repository in the `itmitalles-de` organization, first
  commit and CI run (workflow `.github/workflows/ci.yml` uses nettop's pinned
  action SHAs). Track further work as GitHub issues there.
- Not verified on real hardware: md RAID, zram, removable hot-plug during a
  session, HDDs; a desktop install via `scripts/setup-helper.sh`.
- Known limits: network/FUSE filesystems are not listed (statvfs can hang);
  bind mounts of the same filesystem inside the home tree are scanned twice
  (like `du -x`); processes exiting between refreshes are not listed; growth is
  per session and only for retained directories; 64-bit Linux targets only.
