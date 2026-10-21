#!/usr/bin/env python3
"""Enumerate exact TUI top-500 tie subsets against CLI JSON in a real Linux PTY.

Uses only temporary fixtures and snapshot jumps, never reveal/open/copy actions.
IN_OPEN/IN_ACCESS checks detect fixture reads/opens, not metadata-only syscalls.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

from tui_result_jump import Session


ROW = re.compile(r"║\s*(▌?)\s*(\d+)\s+.*?\s(f\d{4}\.pdf)\s*║")


def collect_popup(session, count):
    found = {}
    selected = 1
    while True:
        deadline = time.monotonic() + 3
        while True:
            rows = [match.groups() for match in ROW.finditer(session.screen.text())]
            for marker, rank, name in rows:
                rank = int(rank)
                assert found.setdefault(rank, name) == name
            if any(marker and int(rank) == selected for marker, rank, _ in rows):
                break
            assert time.monotonic() < deadline, session.screen.text()
            session.drain(f"wait-selected-{selected}", .04)
        if selected == count:
            break
        selected = min(selected + 15, count)
        # Do not send another key until the intended selected rank renders.
        session.key(b"\x1b[6~", f"page-to-{selected}", .04)
    assert sorted(found) == list(range(1, count + 1)), sorted(found)
    return [found[i] for i in range(1, count + 1)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary, output = args.binary.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    results = []
    with tempfile.TemporaryDirectory(prefix="dutype-tui-ties-") as temp:
        for count, size in [(499, 1), (500, 1), (501, 1), (501, 0)]:
            root = Path(temp) / f"files-{count}-{size}"
            root.mkdir()
            for i in range(count):
                (root / f"f{i:04}.pdf").write_bytes(b"x" * size)
            for mode in ("du", "dedup"):
                env = dict(os.environ, HOME=str(root.parent / "isolated-home"),
                           XDG_CONFIG_HOME=str(root.parent / "isolated-config"))
                report = json.loads(subprocess.check_output([
                    str(binary), "--json", "--category", "PDF", "--top", "500",
                    "--size-mode", mode, str(root)], env=env, text=True))
                expected = [f"f{i:04}.pdf" for i in reversed(range(count))][:500]
                cli = [Path(item["path"]).relative_to(root).as_posix() for item in report["items"]]
                assert cli == expected
                assert report["matched_items"] == count
                assert report["items_truncated"] == (count > 500)
                # Zero-only categories are intentionally hidden; extension mode includes them.
                for view in (("category", "extension") if size else ("extension",)):
                    name = f"{count}-{size}-{mode}-{view}"
                    session = Session(binary, root, output, name, ("--size-mode", mode))
                    if view == "extension":
                        session.key(b"m", "extension-mode")
                    session.popup("*.pdf files" if view == "extension" else "PDF files", "popup")
                    actual = collect_popup(session, len(expected))
                    assert actual == cli, (name, actual, cli)
                    session.selected(expected[-1])
                    session.jump("last-result-jump")
                    session.selected(expected[-1])
                    result = session.finish()
                    result.update(count=count, size=size, size_mode=mode, view=view,
                                  ordered_names=actual, binary_sha256=binary_hash)
                    results.append(result)
                    (output / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
    assert len(results) == 14
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == binary_hash
    print(json.dumps({"scenarios": len(results), "compared_rows": sum(len(r["ordered_names"]) for r in results),
                      "jumps": sum(len(r["jumps"]) for r in results), "binary_sha256": binary_hash}, indent=2))


if __name__ == "__main__":
    main()
