#!/usr/bin/env python3
"""Create synthetic ordinary/hard-link scan fixtures in a NEW directory.

Usage: python3 bench/hardlinks.py /tmp/diskbytype-bench-new
Each tree has 160,000 directory entries in 80 sibling directories. The
hard-link tree contains 20,000 unique 4-KiB files with eight cross-directory
links each. Never run against user data; an existing destination is rejected.
"""

import os
import sys
from pathlib import Path

root = Path(sys.argv[1])
root.mkdir()
for kind in ["plain", "links"]:
    tree = root / kind
    tree.mkdir()
    for i in range(80):
        (tree / f"d{i:03}").mkdir()
    for i in range(160_000):
        directory = (i % 20_000 + i // 20_000 * 9) % 80 if kind == "links" else i % 80
        path = tree / f"d{directory:03}" / f"f{i:06}.rs"
        if kind == "links" and i >= 20_000:
            source = i % 20_000
            os.link(tree / f"d{source % 80:03}" / f"f{source:06}.rs", path)
        else:
            path.write_bytes(b"a" * 4096)
(root / "config.toml").write_text("")
