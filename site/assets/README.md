# Public site assets

`disktop-demo.png`, `disktop-filesystems.png`, `disktop-home.png` and
`disktop-demo.mp4` show the actual disktop terminal UI in explicit `--demo` mode
at 120 × 36 after the 60-second history has filled. Disks, rates, fill levels,
processes and directories are synthetic. The recording demonstrates the all-disks
overview, the Filesystems and Home views (with a directory drill-down) and F2
Setup. These are deliberate public product assets, not live captures or desktop
screenshots.

The terminal images, recording and derived social card were refreshed on
2026-10-10 for [issue #4](https://github.com/itmitalles-de/disktop/issues/4).
RD/WR meters now share one scale across all displayed disks, as implemented
in review PR #3; slower disks therefore have proportionally shorter bars.

They were made by running the release binary in a pseudo-terminal with an empty
config directory and `LANG=en_US.UTF-8`, rendering the pyte screen buffer with
DejaVu Sans Mono (9 × 19 px cells) on a muted ANSI palette, reducing the PNGs to
256 colors, and encoding 10 frames per second with ffmpeg (H.264, CRF 28).

`mark.svg`, `readme-banner.svg` and `badge-*.svg` are original vector artwork. `social-card.png`
combines the product branding with the DEMO terminal image. Project artwork is
covered by the repository's license, GPL-3.0-or-later.

The fonts in `fonts/` are self-hosted Chivo and IBM Plex Mono. Their source URLs
and SIL Open Font License notices are included in that directory.
