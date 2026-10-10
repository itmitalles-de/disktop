# disktop

**Linux disk activity and storage use. Right in your terminal.**
Live I/O graphs, per-process reads and writes, filesystems and a `/home` size
view, with F2 Setup — inspired by htop and nvtop, and the disk counterpart of
[nettop](https://github.com/itmitalles-de/nettop).

**disktop** puts a block device's throughput, IOPS, utilization and latency,
the processes causing storage I/O, every mounted filesystem, and where the space
in `/home` goes into one compact view. Green for read, yellow for write, stepped
history graphs, and your terminal's own ANSI palette. The layout works in a split
terminal, including 36 × 16.

**Start without sudo.** The interface runs as your regular user and shows your
own processes and the directories you can read. An optional one-time helper
setup adds all users' processes and home directories; the interface itself never
gains privileges.

| At a glance | Under your control |
| --- | --- |
| **I/O over time** | Read/write history, IOPS, utilization, await, queue depth |
| **Processes** | READ/s, WRITE/s, cancelled writes and totals per PID; sort and search |
| **Filesystems** | Use, free space, inodes and backing-device rates; `*` marks home |
| **Home sizes** | Background scan, drill-down, share of parent, growth between scans |
| **F2 Setup** | Device, refresh rate, graph, colors, view, home path, rescan interval; save with F12 |

## Install

You need **Linux** and **Rust 1.88+**. The source is MIT licensed.

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

Repeat the helper setup after updating disktop. `--no-helper` bypasses an
installed helper. An administrator revokes access by removing
`/usr/local/libexec/disktop-collector` and its receipt
`/usr/local/libexec/.disktop-collector.sha256`.

</details>

## Use

```bash
disktop                          # Device of /home, process view.
disktop --device sda             # Select a block device (also /dev/sda).
disktop --device all             # Sum of physical whole disks.
disktop --view filesystems       # Or processes, home.
disktop --home-path /srv/homes   # Tree for the home view (default /home).
disktop --interval 0.5 --history 120 --sort write
disktop --all-devices            # Also show loop, ram and zram devices.
disktop --no-color               # Start in monochrome (also NO_COLOR=1).
disktop --once                   # Print one measured snapshot and exit.
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
| <kbd>F4</kbd> / `v`, `1` `2` `3` | Processes / Filesystems / Home view |
| <kbd>F5</kbd> / `d` | Choose a device; Tab / Shift+Tab cycle disks |
| <kbd>F6</kbd> | Choose the process sort column; `s` cycles it |
| <kbd>F9</kbd> / Space | Pause or resume the display |
| <kbd>F12</kbd> | Save current settings |
| Enter / → , Backspace / ← | Home view: open a directory / go up |
| `r`, `a` | Home view: rescan now / allocated vs. apparent size |
| ↑ / ↓, Page Up / Page Down, Home / End | Move through the table |
| `q` / <kbd>F10</kbd> / Ctrl+C | Quit; `q` closes overlays, F10 returns from Setup |

### Make it yours with F2

Setup has **General**, **Device**, **Chart**, **Table** and **Home** panels:
refresh interval, colors, language, device (Auto follows the disk holding the
home path), loop/ram/zram visibility, graph and history, read/write colors,
view, sorting, idle processes, the home path (Enter edits, Enter applies, Esc
cancels), the rescan interval (1 minute to 24 hours) and allocated vs. apparent
sizes. Changes apply immediately; **F12 saves them for the next start**.

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
kept. A saved device that no longer exists falls back to automatic selection for
that run. As root, disktop refuses to save into another user's settings
directory.

</details>

## What the numbers mean

Device rates are deltas of the kernel's **/proc/diskstats** counters, whose
sectors are always 512 bytes. Utilization is the share of time with requests in
flight; await is the average request time; `qd` the average queue depth. A
counter reset or a re-plugged device shows `-` for one interval instead of a
wrong rate. **All** adds physical whole disks only, since partitions, device
mapper (LVM, LUKS) and MD RAID would count the same I/O twice.

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

---

[MIT licensed](LICENSE) · Inspired by htop, nvtop and iotop.
