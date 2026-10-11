# Public site assets

`disktop-demo.png` and `disktop-filesystems.png` show the default dwtop Space
view. `disktop-home.png` shows directory sizes and growth; `dwtop-io.png` shows
the secondary I/O view. `disktop-demo.mp4` demonstrates Space, Home (including
navigation into a directory), the view menu, I/O, return to Space, and F2 Setup.
The existing asset filenames retain the former project name for compatibility.

All terminal media show the real release binary in explicit `--demo` mode at
120 × 36 after the 60-second history has filled. Disks, rates, fill levels,
processes and directories are synthetic. These are deliberate public product
assets, not live captures or desktop screenshots. Media was refreshed for the
dwtop rename in [issue #18](https://github.com/itmitalles-de/dwtop/issues/18).

They were made by running the release binary in a pseudo-terminal with an empty
config directory and `LANG=en_US.UTF-8`, rendering the pyte screen buffer with
DejaVu Sans Mono (9 × 19 px cells) on a muted ANSI palette, reducing the PNGs to
256 colors, and encoding 10 frames per second with ffmpeg (H.264, CRF 28).

`mark.svg`, `readme-banner.svg` and `badge-*.svg` are original vector artwork.
`social-card.png` is a browser render of `social-card.html` at 1200 × 630 pixels,
combining the product branding with the DEMO terminal image. Project artwork is
covered by the repository's license, GPL-3.0-or-later.

The fonts in `fonts/` are self-hosted Chivo and IBM Plex Mono. Their source URLs
and SIL Open Font License notices are included in that directory.
