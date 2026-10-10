#!/usr/bin/env python3
"""Check that local references in the site and README.md resolve to files.

Usage: python3 .github/check-site.py [repository-root]

Covers href/src/poster/srcset/data-src in site/*.html, url(...) in site/*.css
and the local image and link paths in README.md (Markdown and HTML syntax).
External (scheme:), protocol-relative, fragment-only and data: references are
ignored; query strings and fragments are stripped before the lookup.
"""

import re
import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

SINGLE = {"href", "src", "poster", "data-src"}


class Refs(HTMLParser):
    def __init__(self):
        super().__init__()
        self.refs = []

    def handle_starttag(self, tag, attrs):
        for name, value in attrs:
            if not value:
                continue
            if name in SINGLE:
                self.refs.append(value)
            elif name == "srcset":
                self.refs += [part.split()[0] for part in value.split(",") if part.strip()]


def local(ref):
    ref = ref.strip()
    if not ref or ref.startswith(("#", "//")):
        return None
    parts = urlsplit(ref)
    if parts.scheme or not parts.path:
        return None
    return unquote(parts.path)


def main():
    root = Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
    site = root / "site"
    problems = []
    checked = 0

    def check(source, ref, base):
        nonlocal checked
        path = local(ref)
        if path is None:
            return
        checked += 1
        target = (site / path.lstrip("/")) if path.startswith("/") and base == site else base / path
        target = target.resolve()
        if not target.exists() or (base == site and site not in target.parents and target != site):
            problems.append(f"{source}: {ref!r} does not resolve to an existing file")

    for page in sorted(site.glob("*.html")):
        parser = Refs()
        parser.feed(page.read_text(encoding="utf-8"))
        for ref in parser.refs:
            check(page.relative_to(root), ref, site)
    for sheet in sorted(site.glob("*.css")):
        for ref in re.findall(r"url\(\s*['\"]?([^'\")]+?)['\"]?\s*\)", sheet.read_text(encoding="utf-8")):
            check(sheet.relative_to(root), ref, site)
    readme = (root / "README.md").read_text(encoding="utf-8")
    refs = re.findall(r"!?\[[^\]]*\]\(\s*<?([^)\s>]+)", readme)
    refs += re.findall(r"""\b(?:src|href|srcset)=["']([^"']+)["']""", readme)
    for ref in refs:
        check("README.md", ref, root)
    if problems:
        print("\n".join(problems))
        return 1
    print(f"OK: {checked} local references resolve")
    return 0


if __name__ == "__main__":
    sys.exit(main())
