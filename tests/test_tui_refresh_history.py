#!/usr/bin/env python3
"""Deterministic PTY synchronization checks; no process or real terminal needed."""
import itertools
import unittest
from unittest import mock

from tui_refresh_history import Session


VIEW = "dutype · scanned in 0.00s\nselected.pdf\nrescan folder"
MARKED = VIEW + "\n│ ! │"


class FakeScreen:
    def __init__(self, text):
        self.value = text

    def text(self):
        return self.value


class ScriptedSession(Session):
    def __init__(self, frames):
        self.name = "scripted"
        self.master = -1
        self.screen = FakeScreen(VIEW)
        self.frames = {}
        self.process = mock.Mock()
        self.process.poll.return_value = None
        self.updates = iter(frames)
        self.pumps = 0

    def pump(self):
        self.pumps += 1
        self.screen.value = next(self.updates, self.screen.value)


class RefreshSynchronizationTests(unittest.TestCase):
    def run_refresh(self, frames, error=False):
        session = ScriptedSession(frames)
        # Advance the deadline deterministically; stale-state tests never sleep.
        clock = itertools.count()
        with mock.patch("tui_refresh_history.os.write") as write, mock.patch(
            "tui_refresh_history.time.monotonic", side_effect=lambda: next(clock)
        ):
            if error:
                with self.assertRaises(AssertionError):
                    session.refresh()
            else:
                session.refresh()
        self.assertEqual([call.args[1] for call in write.call_args_list], [b"!", b"R"])
        return session

    def test_completed_refresh_without_any_progress_frame(self):
        session = self.run_refresh([MARKED, VIEW.replace("selected.pdf", "fresh.pdf")])
        self.assertEqual(session.pumps, 2)
        self.assertIn("fresh.pdf", session.frames["completed_refresh"])

    def test_unchanged_successful_view_is_fresh_after_marker_acknowledgement(self):
        session = self.run_refresh([MARKED, VIEW])
        self.assertEqual(session.frames["completed_refresh"], VIEW)
        self.assertEqual(session.frames["acknowledged_before_refresh"], MARKED)

    def test_stale_marked_view_cannot_pass_refresh(self):
        session = self.run_refresh([MARKED], error=True)
        self.assertNotIn("completed_refresh", session.frames)

    def test_progress_frame_is_not_completion(self):
        session = self.run_refresh([MARKED, "Scanning fixture", VIEW])
        self.assertEqual(session.pumps, 3)
        self.assertEqual(session.frames["completed_refresh"], VIEW)

    def test_partial_progress_clear_with_stale_footer_is_not_completion(self):
        session = self.run_refresh([MARKED, "\nrescan folder"], error=True)
        self.assertNotIn("completed_refresh", session.frames)


if __name__ == "__main__":
    unittest.main()
