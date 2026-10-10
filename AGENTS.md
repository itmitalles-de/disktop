# disktop

Follow `/home/tim/AGENTS.md` when working on the owner's machine.

This is a Linux terminal monitor in Rust, the disk counterpart of nettop and
visually based on nvtop: compact device information, a green read / yellow write
history graph, a process, filesystem or home-size table, and function-key help.
Keep the layout usable at 36x16, 40x24, 80x24 and 120x36, in English and German.

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
