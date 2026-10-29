#!/usr/bin/env python3
"""Real PTY smoke test for result jumps; only synthetic temporary fixtures.

Usage: python3 tests/tui_result_jump.py --binary target/debug/diskbytype --output /tmp/diskbytype-tui-proof
Linux inotify watches assert that jumps do not open or read fixture files/directories.
These counters detect rescans; they do not trace metadata-only syscalls.
No reveal, file-opening, or clipboard key is sent while a popup is open.
"""
import argparse
import atexit
import codecs
import ctypes
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import unicodedata


class Screen:
    """Minimal VT screen decoder for crossterm's cursor/clear/SGR output."""
    def __init__(self, width, height):
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.resize(width, height)

    def resize(self, width, height):
        self.width, self.height = width, height
        self.rows = [[" "] * width for _ in range(height)]
        self.x = self.y = 0

    def feed(self, data):
        text = self.pending + self.decoder.decode(data)
        self.pending = ""
        i = 0
        while i < len(text):
            char = text[i]
            if char == "\x1b":
                match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", text[i:])
                if match:
                    raw, _, command = match.groups()
                    nums = [int(x) if x else 0 for x in raw.lstrip("?=>").split(";")]
                    n = nums[0] or 1
                    if command in "Hf":
                        self.y = n - 1
                        self.x = (nums[1] or 1) - 1 if len(nums) > 1 else 0
                    elif command == "G": self.x = n - 1
                    elif command == "d": self.y = n - 1
                    elif command == "A": self.y -= n
                    elif command == "B": self.y += n
                    elif command == "C": self.x += n
                    elif command == "D": self.x -= n
                    elif command == "J" and nums[0] in (2, 3):
                        self.rows = [[" "] * self.width for _ in range(self.height)]
                    elif command == "K" and 0 <= self.y < self.height:
                        start = 0 if nums[0] in (1, 2) else self.x
                        end = self.width if nums[0] in (0, 2) else self.x + 1
                        for x in range(max(0, start), min(self.width, end)):
                            self.rows[self.y][x] = " "
                    i += len(match.group(0))
                    continue
                if text[i:].startswith("\x1b[") or i + 1 == len(text):
                    self.pending = text[i:]
                    break
                i += 2
                continue
            if char == "\r": self.x = 0
            elif char == "\n": self.y += 1
            elif char == "\b": self.x = max(0, self.x - 1)
            elif ord(char) >= 32:
                width = 0 if unicodedata.combining(char) else (2 if unicodedata.east_asian_width(char) in "WF" else 1)
                if 0 <= self.y < self.height and 0 <= self.x < self.width:
                    self.rows[self.y][self.x] = char
                    if width == 2 and self.x + 1 < self.width:
                        self.rows[self.y][self.x + 1] = ""
                self.x += width
            i += 1

    def text(self):
        return "\n".join("".join(row) for row in self.rows)


class FileWatch:
    def __init__(self, root):
        libc = ctypes.CDLL(None, use_errno=True)
        self.fd = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
        assert self.fd >= 0, ctypes.get_errno()
        paths = [root]
        for folder, dirs, files in os.walk(root):
            paths.extend(Path(folder) / name for name in dirs + files)
        for path in paths:
            # IN_ACCESS | IN_OPEN: a scan necessarily opens its directories.
            assert libc.inotify_add_watch(self.fd, os.fsencode(path), 0x21) >= 0

    def events(self):
        count = 0
        while True:
            try:
                data = os.read(self.fd, 65536)
            except BlockingIOError:
                return count
            offset = 0
            while offset < len(data):
                _, mask, _, size = struct.unpack_from("iIII", data, offset)
                if mask & 0x21:
                    count += 1
                offset += 16 + size


class Session:
    def __init__(self, binary, root, output, name, extra=()):
        self.output, self.name = output, name
        self.master, self.slave = pty.openpty()
        self.screen = Screen(140, 36)
        self.raw = bytearray()
        self.frames, self.jumps = {}, []
        self.set_size(140, 36)
        self.original_termios = termios.tcgetattr(self.slave)
        env = dict(os.environ, TERM="xterm-256color", HOME=str(root.parent / "isolated-home"),
                   XDG_CONFIG_HOME=str(root.parent / "isolated-config"), PATH="/no-external-commands")
        self.watch = FileWatch(root)
        args = [str(binary), "--icons", "none", *extra, str(root)]
        self.process = subprocess.Popen(args, stdin=self.slave, stdout=self.slave, stderr=self.slave,
                                        env=env, start_new_session=True)
        atexit.register(self.cleanup)
        self.drain("initial", .55)
        assert "diskbytype" in self.screen.text(), self.screen.text()
        self.initial_accesses = self.watch.events()
        assert self.initial_accesses > 0, "watch did not observe initial scan"

    def cleanup(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
        for fd in (self.master, self.slave, self.watch.fd):
            try:
                os.close(fd)
            except OSError:
                pass

    def set_size(self, width, height):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))

    def drain(self, label, wait=.18):
        until = time.monotonic() + wait
        while time.monotonic() < until:
            if select.select([self.master], [], [], .03)[0]:
                data = os.read(self.master, 65536)
                self.raw.extend(data)
                self.screen.feed(data)
        self.frames[label] = self.screen.text()
        if label != "quit":
            assert self.process.poll() is None, self.screen.text()
        return self.screen.text()

    def key(self, data, label, wait=.18):
        os.write(self.master, data)
        return self.drain(label, wait)

    def resize(self, width, height, label):
        self.screen.resize(width, height)
        self.set_size(width, height)
        # The child owns no shell; resize delivery is explicit to its process group.
        os.killpg(self.process.pid, signal.SIGWINCH)
        return self.drain(label)

    def popup(self, title, label):
        assert "jump in TUI" not in self.screen.text(), self.screen.text()
        self.key(b"\t\r", label)
        assert title in self.screen.text(), self.screen.text()
        assert "jump in TUI" in self.screen.text(), self.screen.text()
        assert "reveal" in self.screen.text() and "copy" in self.screen.text(), self.screen.text()

    def selected(self, needle):
        # A breakdown selection in the right pane must not make an unselected
        # entry with the same row appear selected. Popup assertions likewise
        # exclude the background panes outside the centered popup rectangle.
        if "jump in TUI" in self.screen.text():
            left = (self.screen.width - round(self.screen.width * .86)) // 2
            right = self.screen.width - left
        else:
            left, right = 0, round(self.screen.width * .58)
        rows = [line[left:right] for line in self.screen.text().splitlines()]
        assert any("▌" in line and needle in line for line in rows), self.screen.text()

    def jump(self, label):
        self.watch.events()
        text = self.key(b"s", label)
        accesses = self.watch.events()
        assert accesses == 0, f"jump opened/read fixture paths: {accesses}"
        self.jumps.append({"label": label, "fixture_open_or_access_events": accesses})
        assert "jump in TUI" not in text, text
        return text

    def finish(self):
        self.key(b"q", "quit")
        self.process.wait(timeout=5)
        assert self.process.returncode == 0, self.screen.text()
        assert termios.tcgetattr(self.slave) == self.original_termios, "terminal attributes not restored"
        assert b"\x1b[?1049l" in self.raw, "alternate screen not left"
        (self.output / (self.name + ".ansi")).write_bytes(self.raw)
        (self.output / (self.name + "-screens.json")).write_text(json.dumps(self.frames, indent=2))
        atexit.unregister(self.cleanup)
        self.cleanup()
        return {"scenario": self.name, "jumps": self.jumps, "terminal_restored": True,
                "initial_scan_open_or_access_events": self.initial_accesses}


def fixture(root):
    (root / "docs/deeper").mkdir(parents=True)
    (root / "docs/deeper/a-large.pdf").write_bytes(b"a" * 131072)
    (root / "docs/deeper/z-target.pdf").write_bytes(b"b" * 65536)
    (root / "docs/zero.pdf").touch()
    (root / "project/node_modules/pkg").mkdir(parents=True)
    (root / "project/node_modules/pkg/index.js").write_bytes(b"c" * 4096)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary, output = args.binary.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    results = []
    with tempfile.TemporaryDirectory(prefix="diskbytype-tui-jump-") as tmp:
        root = Path(tmp) / "fixture"
        fixture(root)
        for mode in ("du", "dedup"):
            s = Session(binary, root, output, "files-" + mode, ("--size-mode", mode))
            s.popup("PDF files", "category-popup")
            s.key(b"j", "select-second-file")
            s.selected("z-target.pdf")
            assert "jump in TUI" in s.resize(40, 10, "small-popup")
            s.resize(1, 1, "tiny-popup")
            s.resize(140, 36, "restored-popup")
            s.selected("z-target.pdf")
            assert "docs › deeper" in s.jump("category-jump")
            s.selected("z-target.pdf")
            s.key(b"\x7f", "back-to-docs")
            s.selected("deeper/")
            s.key(b"\x7f", "back-to-root")
            s.selected("docs/")
            s.key(b"m", "extension-mode")
            s.popup("*.pdf files", "extension-popup")
            s.key(b"j", "extension-select-second")
            assert "docs › deeper" in s.jump("extension-jump")
            s.selected("z-target.pdf")
            s.popup("*.pdf files", "repeated-popup")
            s.key(b"j", "repeated-select-second")
            s.jump("repeated-same-parent-jump")
            s.selected("z-target.pdf")
            # Also jump to a zero-byte result in the current folder after returning up.
            s.key(b"\x7f", "zero-parent")
            s.popup("*.pdf files", "zero-popup")
            s.key(b"jj", "zero-select")
            s.jump("zero-direct-file-jump")
            s.selected("zero.pdf")
            s.key(b"g\rj", "reenter-deeper")
            # Cancel leaves the selected file/location intact; r cannot operate behind a popup.
            s.popup("*.pdf files", "cancel-popup")
            s.key(b"r", "refresh-key-blocked-by-popup")
            assert "jump in TUI" in s.screen.text()
            s.key(b"\x1b", "cancel")
            s.key(b"\t", "focus-entries")
            s.selected("z-target.pdf")
            # A real refresh replaces results. The old file must not be reachable through a stale row.
            (root / "docs/deeper/z-target.pdf").rename(root / "docs/deeper/new-target.pdf")
            s.key(b"r", "refresh", .55)
            assert "z-target.pdf" not in s.screen.text()
            s.popup("*.pdf files", "fresh-popup")
            assert "new-target.pdf" in s.screen.text() and "z-target.pdf" not in s.screen.text()
            s.key(b"j", "fresh-second")
            s.jump("fresh-result-jump")
            s.selected("new-target.pdf")
            results.append(s.finish())
            (root / "docs/deeper/new-target.pdf").rename(root / "docs/deeper/z-target.pdf")

        # Nested starting root and directory category, including normal Back history.
        s = Session(binary, root / "project", output, "directory")
        s.popup("node_modules directories", "directory-popup")
        assert "node_modules" in s.jump("directory-jump")
        s.selected("pkg/")
        s.key(b"\x7f", "directory-back")
        s.selected("node_modules/")
        results.append(s.finish())

        # Actual invalid-UTF-8 raw names and their identical escaped display twins.
        collision = Path(tmp) / "collision"
        collision.mkdir()
        raw = os.fsencode(collision) + b"/d\xffir"
        literal = collision / r"d\xffir"
        os.mkdir(raw)
        literal.mkdir()
        with open(raw + b"/same.pdf", "wb") as handle: handle.write(b"r" * 65536)
        (literal / "same.pdf").write_bytes(b"l" * 32768)
        (literal / "literal-marker.txt").write_bytes(b"marker")
        s = Session(binary, collision, output, "raw-name-collision")
        s.popup("PDF files", "raw-popup")
        s.jump("raw-result-jump")
        assert "literal-marker.txt" not in s.screen.text()
        s.selected("same.pdf")
        s.key(b"\x7f", "collision-back")
        s.popup("PDF files", "literal-popup")
        s.key(b"j", "literal-second")
        s.jump("literal-result-jump")
        assert "literal-marker.txt" in s.screen.text()
        s.selected("same.pdf")
        results.append(s.finish())

        empty = Path(tmp) / "empty"
        empty.mkdir()
        s = Session(binary, empty, output, "empty")
        s.key(b"s", "jump-without-result")
        assert "empty directory" in s.screen.text()
        results.append(s.finish())

    (output / "summary.json").write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
