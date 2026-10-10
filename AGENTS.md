# spctop

Follow `/home/tim/AGENTS.md` when working on the owner's machine.

This is a Linux terminal space monitor in Rust, the storage counterpart of nwtop,
visually based on htop/nvtop. Used/free space, fill history and filesystem
capacity are the main view; directory sizes/growth remain easy to reach.
Disk throughput, utilization and process I/O belong in the separate I/O view
selected through the view menu. Preserve the compact styling and function-key
help. Keep the layout usable at 36x16, 40x24, 80x24, 120x36 and 200x60 with
1 to 16 disks, in English and German.

Device rates come from /proc/diskstats deltas (512-byte sectors); process rates
from /proc/PID/io and are not attributable to devices. Unavailable data,
hidden processes and partial scans must be visible; never substitute demo values
in live mode. The home scanner stays on one filesystem, never follows symlinks,
never reads file contents and returns only directory names and sizes.

Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
`cargo test`, plus `tests/terminal.py` and `tests/setup.py` for UI changes. Run
`tests/live_io.py` and `tests/helper.py` in the isolated container when
collection, scanning or helper code changes. Keep screenshots, recordings and
local review data out of Git.
