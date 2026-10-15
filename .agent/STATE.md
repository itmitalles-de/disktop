# Verified state

## Refactoring #36 (local, 2026-10-15)

- Worktree `../dwtop-refactor-20261015`, branch `refactor/full-pass-20261015`,
  based on `7030a0d`; no publication or installed-binary changes.
- Terminal state/input/layout/renderers, scanner policy/traversal/service,
  configuration persistence, CLI/source/output/terminal control and collector
  adapters now have
  explicit module boundaries; public import paths are preserved.
  The inventory and dependency boundaries are in `ARCHITECTURE.md`.
- Verified so far: all 124 Rust tests, formatting and Clippy; terminal/Setup
  checks before/after UI extraction; 864 deterministic terminal text/style
  fingerprints unchanged. A real unprivileged scan of the same synthetic
  directory tree is identical, excluding only start time and duration.
- Isolated I/O/helper validation and the remaining issue acceptance checks are
  pending. Evidence is local in
  `/home/tim/.codex/reports/refactoring-2026-10-15/dwtop/`.

## Review fixes #22–#34 (2026-10-11)

- Scan-root aliases require trusted root-owned ancestors; user-controlled and
  procfs aliases cannot widen policy. Running helpers refresh hidepid/GID
  policy every sample, clear command caches on changes and fail closed.
- Home scanner counters translate across helper/local source switches,
  including asynchronous scans and paused UI acceptance. Collector clock
  resets clear history; missing measurements split Steps/Braille curves.
  Apparent-size growth columns explicitly retain allocated-block semantics.
- Inaccessible mount metadata remains visible as unavailable. Capacity
  deduplication prefers complete measurements. Resolver work remains bounded
  across path changes and refreshes cached paths; backing graphs use bounded
  BFS and per-sample slave caches.
- Process rows share a 16 MiB serialized budget; complete helper replies fit
  32 MiB by disclosing omitted rows/metadata while retaining the Home tree.
  Additive protocol-v1 omitted counts preserve older helper compatibility.
  Requests accept worst-case escaping of every valid 4096-byte root.
- Verified: 124 Rust tests on stable and Rust 1.88, fmt, Clippy, release,
  terminal/Setup, isolated real I/O and capability-helper tests, ShellCheck
  and dependency audit. Dedicated Ubuntu 24.04/KVM kernel tests passed:
  seven policy/GID transitions on one helper PID, hard-NFS outage response
  in about 1.31 s and scan recovery in about 0.36 s. VM resources retired.
- Evidence and tested release hashes: `../disktop-review/review-fixes-20261011/`.
  Installer receipts remain authoritative for host executable provenance;
  older installation statements below describe their historical milestones.

## Space layout (2026-10-11)

- #20: short filesystem tables reserve only their header and actual rows,
  releasing unused height to fill history. Space/Home blocks budget three
  capacity rows plus history instead of the I/O meter layout. At 60x24, two
  disks and four mounts now show both graphs and the complete table.
- I/O layout, graph preferences, long-table scrolling and empty filter
  messages retain their behavior. The EN/DE matrix covers eight terminal
  sizes and 1, 2, 4, 8 and 16 disks. Verified: 104 Rust tests, fmt, Clippy,
  release build, terminal restoration and Setup/configuration integration.
- Actual before/after 60x24 captures and validation logs are local only in
  `../disktop-review/space-layout/`. Collection and helper code are unchanged.

## dwtop rename (2026-10-11)

- Public repository is `itmitalles-de/dwtop`, GPL-3.0-or-later; the local main
  checkout is `../dwtop`. PR #19 merged as `5eb7a40` (implementation `7282532`).
  UI and helper executables are `dwtop` and `dwtop-collector`; Space/history,
  Home growth and the separate I/O menu retain their behavior.
- Settings and scan policies prefer dwtop, then spctop, then disktop. Only
  safely absent files permit fallback; higher-priority malformed/unsafe files
  remain authoritative failures. Saving preserves old settings and unknown
  fields. Explicit spctop I/O preferences survive. Policies reject invalid
  UTF-8/paths, more than 64 roots and oversized files completely; a selected
  policy disappearing during loading cannot enable defaults.
- Verified: 102 Rust tests on stable and Rust 1.88, fmt, Clippy, release build,
  terminal/configuration migration, isolated real I/O and capability-helper
  tests, ShellCheck, dependency audit and site references. PR CI/MSRV/audit,
  all CodeQL languages and Pages build passed. Public Pages deployment passed;
  HTML, JS, banner, social card, screenshots and video hashes match the repo.
- Website remains `disktop.wutz.io`, with dwtop branding and commands. EN/DE,
  desktop/mobile, themes, tabs, images and complete playback passed. Review:
  `../disktop-review/rename-dwtop/review.html` (opened in visible Brave).
- UI and helper are installed with hashes matching the validated release;
  helper root:tim 0750, DAC_READ_SEARCH and SYS_PTRACE; UI has no capabilities.
  Live Space/I/O --once and JSON verify both physical NVMe, marked /home,
  1,089 readable processes, no hidden processes and no nonresponding mounts.
  All three scan-policy files are absent on this host, so /home is the default.
- Receipt/hash-verified legacy spctop UI/helper and receipts were retired;
  settings and policies remain untouched. The merged rename worktree and
  local/remote branch were removed. #18 is complete; GitHub Issues remain the
  canonical source of open work. Verification and retirement evidence is in
  `../disktop-review/rename-dwtop/`.

## Full review fixes (2026-10-10)

- #11: current and legacy scan-root policies exceeding 64 KiB fail closed in
  their entirety; exact-limit valid policies remain accepted.
- #12: unreadable/depth/hard-link-incomplete scans cannot annotate growth or
  replace complete baselines; retention folding with exact totals still can.
- #13/#16: bounded display names are separate from raw filename identities.
  Navigation/growth survive display collisions and changed node indices;
  rescans truncate deleted/folded navigation to the deepest surviving prefix.
  Identity is an additive protocol-v1 field with legacy name fallback.
- #14: capacity mapping follows partitions and holders at every stacked
  level, including partitions of md/dm, with bounded breadth-first traversal.
- #15: mount probes share a 500 ms budget per sample, preserve late results
  and never duplicate an outstanding path. Launches rotate; at most eight
  calls within their deadline and 64 outstanding overall.
- Verified: 98 Rust tests, formatting, Clippy, release build, terminal/Setup
  tests and isolated live I/O/helper tests. Real root-owned policy tests cover
  oversized current/legacy files and refusal to fall back from an invalid new
  policy. The maximum retained tree plus filename identities fits the unchanged
  32 MiB response bound. Permission-loss/recovery tests ran unprivileged.
- Evidence: `../disktop-review/full-code-review-20261010/`. Installation
  hashes below identify historical builds, not the current installation.

## spctop space-first transition (2026-10-10)

- Repository renamed to public `itmitalles-de/spctop`, GPL-3.0-or-later.
  Issue #9 tracks the rename and the used-space-first interface.
- Space is the default view: used/free capacity, fill percentage and history,
  plus filesystems. Home keeps directory sizes and growth; F4 opens the view
  menu with a separate I/O view for disk throughput/utilization and processes.
- Executables are `spctop` and `/usr/local/libexec/spctop-collector`.
  Legacy disktop preferences remain readable until a new spctop configuration
  is saved. Legacy process-first preferences migrate to the Space view.
- The helper policy uses `/etc/spctop/scan-roots`, falling back to the legacy
  `/etc/disktop/scan-roots` when absent; invalid policies fail closed. Only
  when both are absent does the default /home root apply.
- Website hostname remains `disktop.wutz.io`; branding, examples and public
  DEMO media reflect spctop. Evidence: `../disktop-review/rename-spctop/`.
- Verified: 86 Rust tests on stable and Rust 1.88, Clippy, formatting,
  terminal restoration and Setup/navigation/migration PTY tests; isolated live
  I/O and helper tests including legacy policy precedence and unsafe ancestors;
  ShellCheck, 32 site references, EN/DE desktop/mobile browser checks and video
  playback. Public DEMO media comes from the actual new release, not mockups.
- UI and helper installed with matching release hashes; legacy executables
  removed only after verifying their installation receipts and known hashes.
  Helper root:tim 0750, DAC_READ_SEARCH and SYS_PTRACE; UI no capabilities.
  Live default --once, explicit --view io --once and JSON verified both NVMe,
  /home, 1,073 readable processes and no nonresponding mounts. Both policy
  files are absent on this host, so /home is the default allowed scan root.
  UI SHA-256: `05dd96706f2f581a67fd4b3d09f9d55b8b4a4786b129226e5e5f1c4bdb13fa9e`;
  helper: `a6247db58edd8267c24976ad7b0bd74582165aaf76663178f7a45bf841e9f646`.
- The following entries record the verified pre-rename baseline and historical
  evidence paths; installation/test results for this transition are recorded above.

- Public repository `itmitalles-de/disktop`, branch `main`, Linux/Rust 1.88+,
  GPL-3.0-or-later. Overview and website PRs #1/#2 are merged. Review round 1
  merged as [PR #3](https://github.com/itmitalles-de/disktop/pull/3) on
  2026-10-10 (`7a5fc33`, including `cff30b9` and test fix `1f63fe3`). The
  review worktree and local/remote `fix/review-round-1` branch were removed.
  Follow-up [PR #8](https://github.com/itmitalles-de/disktop/pull/8) merged as
  `0d6e02e` (implementation `86da3ad`), closing issues #4–#7. Local `main`
  is synchronized; local/remote `fix/followup-issues` branches were removed.
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
  from `0d6e02e`; helper is root:tim 0750 with the expected capabilities.
  `/etc/disktop/scan-roots` is absent, so the default applies. Live `--once`
  and `--json` show both NVMe disks, correctly mark `/home`, report all 1,207
  sampled processes readable and no nonresponding mounts. Brief live/demo
  PTY sessions quit cleanly and restore the terminal; demo meters show the
  common RD/WR scale.
- `site/` is the public dependency-free website with local licensed fonts,
  explicit DEMO media, EN/DE and persisted system-aware light/dark controls.
  Pages uses GitHub Actions, `disktop.wutz.io`, and enforced HTTPS; the merge
  Pages deployment passed. Review evidence stays outside Git in
  `../disktop-review/`; round-1 before/after terminal text captures are in
  `../disktop-review/round-1/`.

## Follow-up completion (2026-10-10)

- #4: refreshed all three DEMO PNGs, the 17.4 s MP4 and social card for the
  common RD/WR scale, preserving viewport, palette, style and synthetic data.
- #5: ZFS pool membership maps to all known backing disks. Pool allocation is
  counted once across datasets/disks, separate from dataset statvfs. Optional
  `zpool` discovery is bounded and unprivileged; failures remain visible.
  `accounting_keys` is additive; `block_devices` retains actual device names.
- #6: scanner retention shares the node budget among root-child subtrees;
  evicted detail remains folded into exact totals. Adversarial tests cover
  equal-depth floods, traversal orders, deep active ancestors and tiny budgets.
- #7: real isolated KVM tests passed for hard NFS outage/recovery, hidepid 1/2,
  direct/helper collection and primary/supplementary gid exemptions. They found
  and fixed missing direct-collection warnings: stat EACCES is counted, and
  hidden enumeration reports an unknown total. The TUI warns in both cases.
- Verified: fmt, Clippy with warnings denied, 81 Rust tests on stable and
  Rust 1.88.0, release build, terminal/setup, isolated live I/O/helper tests,
  ShellCheck, Cargo Audit and site references. Real ZFS mirror with two
  disks/multiple datasets/quota passed as user and root; failed-query behavior
  also passed. Reproduction: `tests/kernel.md`, `tests/zfs.md`.
- Review: `../disktop-review/followup-issues/index.html`; real-kernel evidence
  is in `../disktop-review/followup-kernel-20261010/`. PR #8 CI, MSRV, audit,
  CodeQL and Pages build passed. Merge `0d6e02e` CI (stable/MSRV, audit,
  terminal/setup, isolated I/O/helper, ShellCheck), CodeQL and Pages deployment
  passed; published media hashes match the repository assets.

## Open work and limits

- GitHub Issues are the canonical source of remaining work; #11–#16 are
  addressed by the full review fixes above. Earlier #4–#7 closed with PR #8.
- ZFS file-backed/unresolved members have no inferred disk mapping. Scanner
  limits can still summarize root children themselves, and already evicted
  descendant names cannot be recovered within that scan. Real kernel tests
  cover NFSv4.2/hidepid 1/2, not every NFS variant or hidepid 4.
- Hardware coverage still excludes real md RAID, zram, hot-plug, HDDs and a
  real swap partition. Network/generic FUSE mounts are filtered; fuseblk is
  eligible and protected by bounded probing. Same-filesystem bind mounts in
  a home tree can be scanned twice (like `du -x`); processes exiting between
  samples are absent. Growth is per session and retained directory, with
  incomplete scans excluded as baselines. Only 64-bit Linux is supported.
