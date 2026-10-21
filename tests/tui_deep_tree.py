#!/usr/bin/env python3
"""Opt-in Linux deep-tree PTY coverage; temporary fixtures, no external actions.

python3 tests/tui_deep_tree.py --binary target/debug/dutype --output /tmp/dutype-deep-proof
"""
import argparse
from contextlib import contextmanager
import ctypes
import fcntl
import json
import os
from pathlib import Path
import pty
import resource
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

sys.dont_write_bytecode = True
from tui_result_jump import Screen


@contextmanager
def fixture(depth):
    """Track only owned paths, and remove deepest-first without Python recursion."""
    base = Path(tempfile.mkdtemp(prefix="dutype-deep-tui-"))
    dirs, file = [base], None
    try:
        root = base / "fixture"
        root.mkdir()
        dirs.append(root)
        leaf = root
        for _ in range(depth):
            leaf = leaf / "d"
            leaf.mkdir()
            dirs.append(leaf)
        file = leaf / "deep.pdf"
        file.write_bytes(b"p" * (8 << 20))
        yield root, dirs[1:] + [file]
    finally:
        if file is not None:
            file.unlink(missing_ok=True)
        for path in reversed(dirs):
            path.rmdir()


class Session:
    """Own every handle before fallible setup; cleanup is safe to call twice."""
    def __init__(self, binary, root, watched, output, name):
        self.master = self.slave = self.watch = None
        self.process = None
        self.output, self.name = output, name
        self.raw, self.frames = bytearray(), {}
        self.screen = Screen(140, 36)
        try:
            self.master, self.slave = pty.openpty()
            self.original_termios = termios.tcgetattr(self.slave)
            self.set_size(140, 36)
            libc = ctypes.CDLL(None, use_errno=True)
            self.watch = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
            if self.watch < 0:
                self.watch = None
                raise OSError(ctypes.get_errno(), "inotify_init1")
            for path in watched:
                if libc.inotify_add_watch(self.watch, os.fsencode(path), 0x21) < 0:
                    raise OSError(ctypes.get_errno(), f"inotify_add_watch: {path}")
            env = dict(os.environ, TERM="xterm-256color", RAYON_NUM_THREADS="2",
                       HOME=str(root.parent / "isolated-home"),
                       XDG_CONFIG_HOME=str(root.parent / "isolated-config"), PATH="/no-external-commands")
            self.process = subprocess.Popen(
                [str(binary), "--icons", "none", "--clones", str(root)],
                stdin=self.slave, stdout=self.slave, stderr=self.slave,
                env=env, start_new_session=True)
            self.wait_for("initial", lambda text: "scanned in" in text and "Entries" in text)
            self.initial_events = self.events()
            assert self.initial_events > 0, "initial scan was not observed"
        except BaseException:
            self.cleanup()
            raise

    def set_size(self, width, height):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))

    def wait_for(self, label, predicate, timeout=10, require_output=False):
        deadline = time.monotonic() + timeout
        initial_bytes = len(self.raw)
        while True:
            text = self.screen.text()
            if predicate(text) and (not require_output or len(self.raw) > initial_bytes):
                self.frames[label] = text
                return text
            assert self.process.poll() is None, f"unexpected exit {self.process.returncode}:\n{text}"
            assert time.monotonic() < deadline, f"timed out waiting for {label}:\n{text}"
            if select.select([self.master], [], [], .05)[0]:
                data = os.read(self.master, 65536)
                assert data, "unexpected PTY EOF"
                self.raw.extend(data)
                self.screen.feed(data)

    def key(self, keys, label, predicate):
        os.write(self.master, keys)
        return self.wait_for(label, predicate, require_output=True)

    def resize(self, width, height, label, predicate):
        self.screen.resize(width, height)
        self.set_size(width, height)
        os.kill(self.process.pid, signal.SIGWINCH)
        return self.wait_for(label, predicate, require_output=True)

    def events(self):
        count = 0
        while True:
            try:
                data = os.read(self.watch, 65536)
            except BlockingIOError:
                return count
            offset = 0
            while offset < len(data):
                _, mask, _, size = struct.unpack_from("iIII", data, offset)
                assert not mask & 0x4000, "inotify queue overflowed"
                count += bool(mask & 0x21)
                offset += 16 + size

    def selected(self, name):
        return any("▌" in row[:80] and name in row[:80] for row in self.screen.text().splitlines())

    def finish(self):
        os.write(self.master, b"q")
        deadline = time.monotonic() + 5
        while self.process.poll() is None and time.monotonic() < deadline:
            if select.select([self.master], [], [], .05)[0]:
                data = os.read(self.master, 65536)
                self.raw.extend(data)
                self.screen.feed(data)
        assert self.process.poll() == 0, "clean quit did not complete"
        # Drain final restoration bytes, which may arrive with process exit.
        while select.select([self.master], [], [], 0)[0]:
            self.raw.extend(os.read(self.master, 65536))
        assert termios.tcgetattr(self.slave) == self.original_termios, "terminal attributes not restored"
        assert b"\x1b[?1049l" in self.raw, "alternate screen not left"

    def cleanup(self):
        try:
            if self.process is not None and self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=2)
        finally:
            for attr in ("master", "slave", "watch"):
                fd = getattr(self, attr)
                setattr(self, attr, None)
                if fd is not None:
                    os.close(fd)
            (self.output / (self.name + ".ansi")).write_bytes(self.raw)
            (self.output / (self.name + "-screens.json")).write_text(json.dumps(self.frames, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--depth", type=int, choices=(350, 1000), default=1000)
    args = parser.parse_args()
    if not sys.platform.startswith("linux"):
        parser.error("this optional PTY/inotify check requires Linux")
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    binary, output = args.binary.resolve(strict=True), args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    results = []
    with fixture(args.depth) as (root, watched):
        for mode in ("category", "extension"):
            session = Session(binary, root, watched, output, f"depth-{args.depth}-{mode}")
            try:
                if mode == "extension":
                    session.key(b"m", "extension", lambda text: ".pdf" in text)
                title = "*.pdf files" if mode == "extension" else "PDF files"
                popup = lambda text: title in text and "jump in TUI" in text
                session.key(b"\t\r", "popup", popup)
                session.resize(1, 1, "tiny", lambda _: True)
                session.resize(140, 36, "restored", popup)
                session.events()
                session.key(b"s", "jump", lambda text: "jump in TUI" not in text and session.selected("deep.pdf"))
                events = session.events()
                assert events == 0, f"jump opened/read fixture paths: {events}"
                session.key(b"c", "dedup", lambda text: "sizes: clones counted once" in text or "sizes: shared" in text)
                assert session.selected("deep.pdf")
                session.key(b"c", "du", lambda text: "sizes: allocated, like du" in text)
                assert session.selected("deep.pdf")
                session.key(b"\x7f", "back", lambda _: session.selected("d/"))
                session.finish()
                results.append(dict(depth=args.depth, mode=mode, jump_open_or_access_events=events,
                                    initial_scan_events=session.initial_events, terminal_restored=True))
            finally:
                session.cleanup()
    (output / "results.json").write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
