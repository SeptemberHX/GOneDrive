"""Tests for the Nautilus extension's decision logic.

The parts that talk to Nautilus and D-Bus need a running file manager, but the
part that decides *what to show* is pure and is where the bugs would be: a
wrong emblem is a lie about whether the user's file is on their disk.
"""

import importlib.util
import os
import sys
import tempfile
import unittest
from unittest import mock


def load_extension():
    """Import the extension with the Nautilus bindings stubbed out."""
    for name in ("gi", "gi.repository"):
        sys.modules.setdefault(name, mock.MagicMock())
    sys.modules["gi"].require_version = lambda *_: None

    repository = mock.MagicMock()
    # `GObject.GObject` and the provider interfaces are used as base classes,
    # so they have to be real types rather than mocks.
    repository.GObject.GObject = type("GObject", (), {})
    for provider in ("InfoProvider", "MenuProvider", "ColumnProvider"):
        setattr(repository.Nautilus, provider, type(provider, (), {}))
    sys.modules["gi.repository"] = repository
    for name in ("Gio", "GLib", "GObject", "Nautilus"):
        sys.modules[f"gi.repository.{name}"] = getattr(repository, name)

    path = os.path.join(os.path.dirname(__file__), "..", "gonedrive.py")
    spec = importlib.util.spec_from_file_location("gonedrive_ext", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ext = load_extension()


class StatusTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.dir.name, "file.bin")
        with open(self.path, "wb") as handle:
            handle.write(b"x")

    def tearDown(self):
        self.dir.cleanup()

    def mark(self, state=None, pin=None):
        if state is not None:
            os.setxattr(self.path, "user.gonedrive.state", state.encode())
        if pin is not None:
            os.setxattr(self.path, "user.gonedrive.pin", pin.encode())

    def test_an_unmanaged_file_gets_no_emblem(self):
        # Every file in the user's home passes through here. Claiming one we do
        # not manage would put a OneDrive badge on an unrelated file.
        self.assertIsNone(ext.status_of(self.path))

    def test_a_placeholder_reads_as_online_only(self):
        self.mark(state="dehydrated")
        self.assertIs(ext.status_of(self.path), ext.ONLINE_ONLY)

    def test_downloaded_but_evictable_differs_from_always_keep(self):
        # Windows draws these two differently, because the difference is
        # whether the file will still be there after the disk fills up.
        self.mark(state="hydrated")
        self.assertIs(ext.status_of(self.path), ext.AVAILABLE)

        self.mark(pin="pinned")
        self.assertIs(ext.status_of(self.path), ext.ALWAYS_KEEP)

    def test_an_interrupted_hydration_shows_as_syncing(self):
        self.mark(state="hydrating")
        self.assertIs(ext.status_of(self.path), ext.SYNCING)

    def test_an_unrecognised_state_shows_as_an_error(self):
        # Better a visible error than a badge implying the file is fine.
        self.mark(state="something-new")
        self.assertIs(ext.status_of(self.path), ext.FAILED)

    def test_every_status_names_a_distinct_emblem(self):
        statuses = [
            ext.ONLINE_ONLY,
            ext.AVAILABLE,
            ext.ALWAYS_KEEP,
            ext.SYNCING,
            ext.FAILED,
        ]
        emblems = [s.emblem for s in statuses]
        self.assertEqual(len(emblems), len(set(emblems)))
        for status in statuses:
            self.assertTrue(status.label)

    def test_a_missing_file_is_not_claimed(self):
        os.remove(self.path)
        self.assertIsNone(ext.status_of(self.path))


class EmblemAssetTests(unittest.TestCase):
    def test_every_emblem_the_code_names_is_shipped(self):
        # A named-but-missing emblem renders as nothing, which looks exactly
        # like "this file is not managed".
        icons = os.path.join(
            os.path.dirname(__file__), "..", "..", "icons",
            "hicolor", "scalable", "emblems",
        )
        for status in (
            ext.ONLINE_ONLY,
            ext.AVAILABLE,
            ext.ALWAYS_KEEP,
            ext.SYNCING,
            ext.FAILED,
        ):
            svg = os.path.join(icons, f"emblem-{status.emblem}.svg")
            self.assertTrue(os.path.exists(svg), f"missing {svg}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
