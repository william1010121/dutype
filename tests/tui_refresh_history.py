#!/usr/bin/env python3
"""Shallow real-PTY refresh regression using only isolated temporary files.

Usage: python3 tests/tui_refresh_history.py --binary target/debug/dutype --output /tmp/refresh-proof
No reveal/open/copy commands are sent. PATH, HOME, config, and cache are isolated.
Cancellation is sent only after observing the progress screen, never after a fixed sleep.
"""
import argparse
import atexit
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time

from tui_result_jump import Screen


class Session:
    def __init__(self, binary, root, output, name):
        self.name, self.output = name, output
        self.master = self.slave = self.process = None
        self.cleanup_registered = False
        self.screen, self.raw, self.frames = Screen(140, 36), bytearray(), {}
        try:
            self.master, self.slave = pty.openpty()
            self.original_termios = termios.tcgetattr(self.slave)
            fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 140, 0, 0))
            env = dict(os.environ, TERM="xterm-256color", HOME=str(root.parent / "home"),
                       XDG_CONFIG_HOME=str(root.parent / "config"), XDG_CACHE_HOME=str(root.parent / "cache"),
                       DUTYPE_CACHE_DIR=str(root.parent / "cache" / "dutype"), RAYON_NUM_THREADS="2",
                       PATH="/no-external-commands")
            self.process = subprocess.Popen([str(binary), "--icons", "none", str(root)],
                                            stdin=self.slave, stdout=self.slave, stderr=self.slave,
                                            env=env, start_new_session=True)
            atexit.register(self.cleanup)
            self.cleanup_registered = True
            self.wait(lambda: "rescan folder" in self.screen.text())
        except BaseException:
            self.cleanup()
            raise

    def pump(self):
        if select.select([self.master], [], [], .01)[0]:
            data = os.read(self.master, 65536)
            self.raw.extend(data)
            self.screen.feed(data)

    def drain_available(self):
        while select.select([self.master], [], [], .01)[0]:
            self.pump()

    def wait(self, predicate, timeout=10):
        deadline = time.monotonic() + timeout
        while not predicate() and time.monotonic() < deadline:
            self.pump()
        assert predicate(), (self.name, self.process.poll(), self.screen.text())

    def key(self, key, predicate):
        os.write(self.master, key)
        self.wait(predicate)

    def selected(self, name):
        return any("▌" in row[:81] and name in row[:81] for row in self.screen.text().splitlines())

    def frame(self, label):
        self.frames[label] = self.screen.text()

    def enter_inner(self):
        self.key(b"\r", lambda: "inner" in self.screen.text() and "other.txt" not in self.screen.text())
        self.key(b"\r", lambda: "a_big.pdf" in self.screen.text())
        self.key(b"G", lambda: self.selected("z_selected.pdf"))

    def refresh(self, key=b"R"):
        self.key(key, lambda: "Scanning " in self.screen.text())
        self.frame("observed_progress")
        self.wait(lambda: "Scanning " not in self.screen.text() and "rescan folder" in self.screen.text())

    def cleanup(self):
        try:
            if self.process is not None and self.process.poll() is None:
                try:
                    self.process.terminate()
                except OSError:
                    # Still our own child: do not leave it running if SIGTERM fails.
                    self.process.kill()
                try:
                    self.process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=2)
        finally:
            for name in ("master", "slave"):
                fd = getattr(self, name)
                setattr(self, name, None)
                if fd is not None:
                    try:
                        os.close(fd)
                    except OSError:
                        pass
            if self.cleanup_registered:
                atexit.unregister(self.cleanup)
                self.cleanup_registered = False

    def finish(self):
        try:
            if self.process.poll() is None:
                os.write(self.master, b"q")
                self.wait(lambda: self.process.poll() is not None)
            self.drain_available()
            assert self.process.returncode == 0, self.screen.text()
            assert termios.tcgetattr(self.slave) == self.original_termios, "terminal not restored"
            (self.output / f"{self.name}.ansi").write_bytes(self.raw)
            (self.output / f"{self.name}.json").write_text(json.dumps(self.frames, indent=2))
        finally:
            self.cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    binary, output = args.binary.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    results = []
    with tempfile.TemporaryDirectory(prefix="dutype-shallow-refresh-") as tmp:
        root = Path(tmp) / "scan-root"
        inner = root / "branch" / "inner"
        inner.mkdir(parents=True)
        (inner / "a_big.pdf").write_bytes(b"a" * 65536)
        (inner / "z_selected.pdf").write_bytes(b"z" * 8192)
        (root / "other.txt").write_bytes(b"o" * 4096)
        s = Session(binary, root, output, "full-success")
        s.enter_inner()
        s.frame("before_refresh")
        s.refresh()
        assert s.selected("z_selected.pdf"), s.screen.text()
        s.frame("nonfirst_selection_preserved")
        (inner / "z_selected.pdf").write_bytes(b"z" * 131072)
        s.refresh()
        assert s.selected("z_selected.pdf"), s.screen.text()
        s.frame("reordered_selection_preserved")
        s.key(b"h", lambda: s.selected("inner"))
        s.key(b"h", lambda: s.selected("branch"))
        s.frame("back_history_preserved")
        s.key(b"G", lambda: s.selected("other.txt"))
        s.refresh(b"r")  # Lowercase r at root uses the full-refresh path.
        assert s.selected("other.txt"), s.screen.text()
        s.frame("root_selection_preserved")
        s.finish()
        results.append({"case": "full-success", "selection_reorder_back_and_root": "passed"})

        (inner / "z_selected.pdf").write_bytes(b"z" * 8192)
        s = Session(binary, root, output, "partial-success")
        s.enter_inner()
        (inner / "z_selected.pdf").write_bytes(b"z" * 131072)
        s.key(b"r", lambda: "rescanned " in s.screen.text())
        assert s.selected("z_selected.pdf"), s.screen.text()
        s.frame("partial_selection_preserved")
        s.finish()
        results.append({"case": "partial-success", "selected_identity_preserved": True})

        (inner / "z_selected.pdf").write_bytes(b"z" * 8192)
        s = Session(binary, root, output, "missing-selection-and-folder")
        s.enter_inner()
        (inner / "z_selected.pdf").unlink()
        s.refresh()
        assert s.selected("a_big.pdf"), s.screen.text()
        s.frame("missing_selection_falls_back")
        inner.rename(inner.with_name("renamed"))
        s.refresh()
        assert s.selected("renamed"), s.screen.text()
        assert "a_big.pdf" not in s.screen.text(), s.screen.text()
        s.frame("missing_folder_falls_back_to_parent")
        s.key(b"h", lambda: s.selected("branch"))
        s.finish()
        results.append({"case": "missing-selection-and-folder", "fallback_and_back": "passed"})

        # Flat bounded fixture, at most two directory levels. No payload is needed.
        many = root / "bulk"
        many.mkdir()
        for i in range(8192):
            (many / f"f{i:05}.txt").touch()
        for refresh, cancel, name in [(b"r", b"\x1b", "partial-escape"), (b"r", b"q", "partial-q"),
                                      (b"R", b"\x1b", "full-escape"), (b"R", b"q", "full-q")]:
            s = Session(binary, root, output, name)
            s.key(b"G", lambda: s.selected("bulk"))
            s.key(b"\r", lambda: "f00000.txt" in s.screen.text())
            s.key(b"j", lambda: s.selected("f00001.txt"))
            s.frame("old_snapshot")
            (many / "new.txt").write_bytes(b"n" * 4096)
            s.key(refresh, lambda: "Scanning " in s.screen.text())
            s.frame("observed_scan_before_cancel")
            os.write(s.master, cancel)
            s.wait(lambda: "rescan cancelled" in s.screen.text() or s.process.poll() is not None)
            if refresh == b"r":
                assert s.process.poll() is None, s.screen.text()
                assert "rescan cancelled" in s.screen.text(), s.screen.text()
                assert s.selected("f00001.txt"), s.screen.text()
                assert "new.txt" not in s.screen.text(), s.screen.text()
            else:
                # Read all queued output after exit before judging the last frame.
                s.drain_available()
                assert s.process.returncode == 0, s.screen.text()
                # No completed snapshot rendered between observed scan and exit.
                assert "Scanning " in s.screen.text(), s.screen.text()
            s.frame("after_cancel")
            s.finish()
            (many / "new.txt").unlink()
            results.append({"case": name, "observed_scan_then_expected_cancel_behavior": "passed"})
    (output / "summary.json").write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
