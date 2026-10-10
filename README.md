<p align="center">
  <a href="https://disktop.wutz.io">
    <img src="site/assets/readme-banner.svg" alt="disktop — your disks, in view" width="100%">
  </a>
</p>

<p align="center">
  <strong>Linux disk activity and storage use. Right in your terminal.</strong><br>
  All disks at once, per-process I/O, filesystems, <code>/home</code> sizes and F2 Setup —
  inspired by htop and nvtop, the disk counterpart of
  <a href="https://github.com/itmitalles-de/nettop">nettop</a>.
</p>

<p align="center">
  <img src="site/assets/badge-linux.svg" alt="Platform: Linux">
  <a href="Cargo.toml"><img src="site/assets/badge-rust.svg" alt="Rust 1.88 or newer"></a>
  <a href="LICENSE"><img src="site/assets/badge-license.svg" alt="License: GPL-3.0-or-later"></a>
</p>

<p align="center">
  <a href="https://disktop.wutz.io">Website</a> ·
  <a href="#install">Install</a> ·
  <a href="#keyboard">Keyboard</a> ·
  <a href="https://github.com/itmitalles-de/disktop">Source</a>
</p>

**disktop** shows **all your disks at once**, like htop shows every CPU core:
each disk gets its own block with read, write, utilization and fill-level
meters and a history graph. Below them, a compact strip lists the processes
causing storage I/O; every mounted filesystem and where the space in `/home`
goes are one key away. Green for read, yellow for write, cyan for utilization,
magenta for fill level, stepped history graphs, and your terminal's own ANSI
palette. The layout works in a split terminal, including 36 × 16.

**Start without sudo.** The interface runs as your regular user and shows your
own processes and the directories you can read. An optional one-time helper
setup adds all users' processes and home directories; the interface itself never
gains privileges.

<p align="center">
  <img src="site/assets/disktop-demo.png" alt="disktop terminal showing four disks side by side, each with read, write, utilization and fill-level meters and a utilization and fill-level graph, above a process I/O table, with F2 Setup and F12 Save in the function-key bar" width="100%">
  <br>
  <sub>Actual terminal output in explicit DEMO mode. Disks, rates and processes are synthetic.</sub>
</p>

| At a glance | Under your control |
| --- | --- |
| **All disks** | RD, WR, IO and fill-level (US) meters plus a graph per disk |
| **I/O over time** | Utilization and fill level per disk; IOPS, await, queue depth |
| **Processes** | READ/s, WRITE/s, cancelled writes and totals per PID; sort and search |
| **Filesystems** | Use, free space, inodes and backing-device rates; `*` marks home |
| **Home sizes** | Background scan, drill-down, share of parent, growth between scans |
| **F2 Setup** | Focus, refresh rate, graph series, colors, view, process rows, home path; save with F12 |

## Install

You need **Linux** and **Rust 1.88+**. The source is licensed under the
[GNU GPL v3 or later](LICENSE).

```bash
git clone https://github.com/itmitalles-de/disktop.git
cd disktop
./scripts/install.sh
./scripts/setup-helper.sh   # Optional, one-time administrator authentication.
~/.local/bin/disktop
```

The installer builds from source and installs the UI into `~/.local/bin` as your
regular user. Add that directory to your `PATH`, then start with `disktop`.

<details>
<summary><strong>Installation, updates, and permissions</strong></summary>

`scripts/install.sh` runs `cargo build --release --locked`. It only replaces an
unchanged executable previously installed by that script; unrelated files,
symlinks and modified executables are preserved.

Without the helper, Linux only lets you read `/proc/PID/io` of your own
processes and only scan directories you may read. disktop shows exactly that and
says how many processes are hidden; it never fills the gap with estimates.

`scripts/setup-helper.sh` builds as your regular user and requests
administrator authentication through `pkexec` (or `sudo`) only to install
`/usr/local/libexec/disktop-collector`, owned by root, mode `0750`, executable
by root and your primary group. **Members of that group can see all processes'
storage I/O and the names and sizes of all directories** (never file names or
contents). Its file capabilities are only `CAP_DAC_READ_SEARCH` and
`CAP_SYS_PTRACE`. The sampling thread keeps both to read other users'
`/proc/PID/io`; the home scanner thread keeps only `CAP_DAC_READ_SEARCH`. Both
set `NO_NEW_PRIVS`; core dumps are disabled and the inherited environment is
cleared at start.

The UI starts the fixed helper over private stdin/stdout pipes with a bounded,
versioned protocol: no service, port, or arbitrary file/command API. Replies have
a deadline; a stalled, crashed, outdated or revoked helper is replaced by direct,
unprivileged collection at startup or at runtime, with a visible notice.

Repeat the helper setup after updating disktop. A helper from an older release
keeps working: the interface adds what it does not send (fill levels and the
disks below dm/md devices, which need no privilege) and suggests rerunning the
setup once. `--no-helper` bypasses an
installed helper. An administrator revokes access by removing
`/usr/local/libexec/disktop-collector` and its receipt
`/usr/local/libexec/.disktop-collector.sha256`.

</details>

## Use

```bash
disktop                          # All disks; focus on the disk of /home.
disktop --device sda             # Focus a block device (also /dev/sda).
disktop --device all             # No focus: summary of physical whole disks.
disktop --view filesystems       # Or processes, home.
disktop --home-path /srv/homes   # Tree for the home view (default /home).
disktop --interval 0.5 --history 120 --sort write
disktop --all-devices            # Also show loop, ram and zram devices.
disktop --no-color               # Start in monochrome (also NO_COLOR=1).
disktop --once                   # Print one measured snapshot (all disks) and exit.
disktop --json                   # Machine-readable snapshot (stable, English).
disktop --json --scan-home       # Include a complete home scan.
disktop --demo                   # Clearly labeled synthetic UI preview.
```

The default refresh interval is **1 second** (`-d`, 0.1–60 s); history defaults
to **60 seconds** (`--history`, 10–600). `-s` sorts processes by `io`, `read`,
`write`, `total`, `pid` or `command`.

## Keyboard

| Key | Action |
| --- | --- |
| <kbd>F1</kbd> / `?` | Help |
| <kbd>F2</kbd> | Open Setup |
| <kbd>F3</kbd> / `/` | Search the current table |
| <kbd>F4</kbd> / `v`, `1` `2` `3` | Processes / Filesystems / Home below the disks |
| <kbd>F5</kbd> / `d` | Focus a device; Tab / Shift+Tab cycle the disks |
| <kbd>F6</kbd> | Choose the process sort column; `s` cycles it |
| <kbd>F9</kbd> / Space | Pause or resume the display |
| <kbd>F12</kbd> | Save current settings |
| Enter / → , Backspace / ← | Home view: open a directory / go up |
| `r`, `a` | Home view: rescan now / allocated vs. apparent size |
| ↑ / ↓, Page Up / Page Down, Home / End | Move through the table |
| `q` / <kbd>F10</kbd> / Ctrl+C | Quit; `q` closes overlays, F10 returns from Setup |

### All disks at once

Every whole disk has its own block: a label row (name, type, size, mounts and
model) and four htop-style meters with their current values,
`RD[|||||   12.3 MiB/s]`:

- **RD** read and **WR** write (green / yellow; German LE / SC), scaled to the
  disk's highest rate in the visible history.
- **IO** utilization (cyan).
- **US** fill level (magenta, red from 90 %; German BL): used and total space of
  the filesystems mounted from the disk, its partitions and stacked devices
  such as LUKS, LVM or MD, each block device counted once. `not mounted` means
  nothing is mounted; `swap only` marks disks used only for swap; `-` means not
  measured.

The **graph** of each disk shows utilization and fill level over the visible
history on one 0–100 % scale, so a disk filling up and its I/O load can be read
side by side. F2 Chart hides either series or all graphs. Partitions, loop, ram
and zram devices are hidden (F2 Device shows loop/ram/zram). Device-mapper and
MD devices that sit on a single shown disk, such as LUKS or LVM volumes on one
disk, are part of that disk's block: its mounts and fill level include them and
its rates already contain their I/O. F2 Device › Show stacked devices shows
them as their own blocks. MD RAID or LVM spanning several disks always gets its
own block.

The overview adapts to the terminal: wide terminals arrange the blocks in two to
four columns with the graph beside the meters; narrow ones put it below. When
space runs short, graphs get shorter, then blocks show meters only, then each
disk takes a single row. If even that does not fit, a `+N more` row says how many
disks are hidden and Tab scrolls the focus to them; no disk is hidden silently.

The **focused** disk (F5, Tab, `--device`, Auto = the disk of the home path) is
shown in reverse video; the row below the title lists its IOPS, await, queue
depth and totals. Without focus, that row summarizes all physical disks. The
process strip below the disks shows 5 rows by default (F2 Table, 0–20; 0 hides
it) and takes any rows the disks leave free. Filesystems and Home use the lower
half of the screen when selected.

### Make it yours with F2

Setup has **General**, **Device**, **Chart**, **Table** and **Home** panels:
refresh interval, colors, language, focus (Auto follows the disk holding the
home path), loop/ram/zram and stacked-device visibility, graphs and history, the IO and fill-level
graph series, read/write/fill-level colors, view, sorting, idle processes,
process rows, the home path (Enter edits, Enter applies, Esc cancels), the
rescan interval (1 minute to 24 hours) and allocated vs. apparent sizes.
Changes apply immediately; **F12 saves them for the next start**.

The terminal interface is available in English and German. **Auto** follows
`LC_ALL`, `LC_MESSAGES` or `LANG`; General › Language fixes either. Command-line
help, errors, `--once` and `--json` stay English.

<details>
<summary><strong>Settings file and command-line overrides</strong></summary>

Settings are saved atomically with private permissions to
`$XDG_CONFIG_HOME/disktop/config.json` or `~/.config/disktop/config.json`.
Command-line options and `NO_COLOR` apply to that run only; F12 keeps the saved
values and adds only what you changed in Setup or with keys. Malformed settings
produce a warning, defaults are used and the file is preserved; unknown keys are
kept; settings files from older versions get defaults for newer options. A
saved device that no longer exists falls back to automatic selection for that
run. As root, disktop refuses to save into another user's settings
directory.

</details>

## What the numbers mean

Device rates are deltas of the kernel's **/proc/diskstats** counters, whose
sectors are always 512 bytes. Utilization is the share of time with requests in
flight; await is the average request time; `qd` the average queue depth. A
counter reset or a re-plugged device shows `-` for one interval instead of a
wrong rate. **All** adds physical whole disks only, since partitions, device
mapper (LVM, LUKS) and MD RAID would count the same I/O twice.

The fill level comes from `statvfs` of the filesystems listed in the
Filesystems view, mapped to disks through partitions and holders in sysfs.
Like `df`'s Use%, it is used / (used + available), so blocks reserved for root
count as full and the shown total is used + available. A filesystem spanning
several disks (btrfs, ZFS) is counted on the disk its mount source names. Swap
partitions come from `/proc/swaps` and are not part of the fill level.

`--json` adds a `usage` object to every device (`filesystems`, `unmeasured`,
`total_bytes`, `used_bytes`, `available_bytes`, `swap`; `null` when not
measured, for example by a helper from an older release) and `backing_disks`,
the whole disks below a dm or md device. The document version
stays 1 because fields are only added. `--once` lists every disk of the
overview with rates, utilization, await and fill level.

Process rates come from **/proc/PID/io**: `read_bytes` and `write_bytes` are
bytes a process caused to be read from or dirtied for storage, across all
devices. **They cannot be attributed to a device**, exclude page-cache hits, and
count writes when data is dirtied rather than at writeback. Cancelled writes
(dirty data truncated before writeback) are charged to the truncating process,
which need not be the writer, so they are shown separately. Processes that exit
between two refreshes are not listed; their I/O remains in the device rates.

<details>
<summary><strong>Filesystems and the home scan</strong></summary>

- Filesystems lists block-backed mounts plus btrfs and ZFS. Pseudo and memory
  filesystems (proc, tmpfs, overlay, …) are skipped, as are network and FUSE
  filesystems, because `statvfs` on an unreachable server can hang. Bind mounts
  of the same filesystem root are shown once. `*` marks the filesystem that
  contains the home path.
- The home scan runs in a background thread with idle I/O priority. It stays on
  one filesystem, never follows symlinks (each directory is opened relative to
  its parent with `O_NOFOLLOW`), counts hard-linked files once, and never reads
  file contents. Sizes are allocated blocks (`du`) or apparent sizes (`du -b`).
- Scans are bounded: 20 million entries, 15 minutes, 128 directory levels. The
  retained tree keeps 20,000 directories, preferring shallower and then larger
  ones, and at most 200 subdirectories per directory; the rest is summarized as
  "smaller dirs" and still counted in every total. Directories that could not be
  read are marked partial (`!`).
- Growth columns compare each retained directory with the previous scan and the
  first scan of this session. Names are shown with control characters,
  bidirectional overrides and invalid UTF-8 escaped (`\xNN`, `\u{NNNN}`).

</details>

## Development

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
shellcheck scripts/*.sh
python3 tests/terminal.py target/release/disktop
python3 tests/setup.py target/release/disktop
```

<details>
<summary><strong>Isolated I/O and helper tests</strong></summary>

These run as root inside a disposable container with a read-only repository
mount and no network:

```bash
docker build -t disktop-test tests
docker run --rm --network none -v "$PWD:/work:ro" disktop-test \
  python3 tests/live_io.py --isolated target/release/disktop
docker run --rm --network none --cap-add DAC_READ_SEARCH --cap-add SYS_PTRACE \
  -v "$PWD:/work:ro" disktop-test python3 tests/helper.py --isolated \
  target/release/disktop target/release/disktop-collector
```

`tests/live_io.py` has separate unprivileged writer and reader processes write
and fsync a file on the container's disk-backed layer and read it back after
dropping its cached pages; disktop must attribute the bytes to the right PIDs
and the physical disks must show the writes. `tests/helper.py` checks the
installer, per-thread capabilities, the cleared helper environment, root
processes visible only through the helper, a private home directory scanned
completely only through the helper without file names leaving it, protocol
bounds, and shutdown or fallback with a stopped or killed helper.

[CI](.github/workflows/ci.yml) runs formatting, linting, tests on stable and
Rust 1.88.0, the release build, the terminal and Setup checks, both container
tests, ShellCheck and a dependency audit.

</details>

See [AGENTS.md](AGENTS.md) for repository-specific contributor instructions.

The [project website](https://disktop.wutz.io) is a dependency-free static site in
[`site/`](site/), published by the [Pages workflow](.github/workflows/pages.yml).
Its terminal images and recording use explicit DEMO mode with synthetic data.

## License

Copyright (C) 2026 itmitalles

disktop is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version (SPDX: `GPL-3.0-or-later`). disktop is distributed in the hope that it
will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See [LICENSE](LICENSE) for
the full license text.

The self-hosted website fonts are licensed separately under the SIL Open Font
License; see [`site/assets/fonts/`](site/assets/fonts/).

---

[GPL-3.0-or-later](LICENSE) · Inspired by htop, nvtop and iotop.
