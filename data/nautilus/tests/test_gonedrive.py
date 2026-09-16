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

    def test_an_atomic_save_inside_the_sync_root_falls_back_to_available(self):
        # GTK's replace-contents API creates a new inode without user xattrs.
        # The provider is allowed to infer only after `_path_of` has proved the
        # file lives in the configured sync root.
        self.assertIs(
            ext.status_of(self.path, managed_without_xattr=True), ext.AVAILABLE
        )

    def test_the_managed_fallback_never_claims_a_symlink(self):
        link = os.path.join(self.dir.name, "link")
        os.symlink(self.path, link)
        self.assertIsNone(ext.status_of(link, managed_without_xattr=True))

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


class SyncRootTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = os.path.join(self.dir.name, "OneDrive")
        os.mkdir(self.root)

    def tearDown(self):
        self.dir.cleanup()

    def test_root_and_descendants_are_managed(self):
        self.assertTrue(ext.is_in_sync_root(self.root, (self.root,)))
        self.assertTrue(
            ext.is_in_sync_root(os.path.join(self.root, "Docs", "a.txt"), (self.root,))
        )

    def test_a_similar_path_prefix_is_not_inside_the_root(self):
        copied = os.path.join(self.dir.name, "OneDrive-old", "a.txt")
        self.assertFalse(ext.is_in_sync_root(copied, (self.root,)))

    def test_an_external_copy_is_not_claimed_even_if_it_kept_xattrs(self):
        copied = os.path.join(self.dir.name, "Downloads", "copy.bin")
        os.makedirs(os.path.dirname(copied))
        with open(copied, "wb") as handle:
            handle.write(b"complete bytes")
        os.setxattr(copied, "user.gonedrive.state", b"dehydrated")

        self.assertIs(ext.status_of(copied), ext.ONLINE_ONLY)
        self.assertFalse(ext.is_in_sync_root(copied, (self.root,)))


class MenuScopeTests(unittest.TestCase):
    def test_selection_and_background_actions_never_share_an_id(self):
        for action in ("menu", "pin", "free", "fetch"):
            self.assertNotEqual(
                ext.menu_item_name("selection", action),
                ext.menu_item_name("background", action),
            )

    def test_background_menu_is_bound_only_to_the_current_folder(self):
        provider = ext.GOnedriveExtension()
        folder = mock.MagicMock()
        folder.is_directory.return_value = True
        provider._path_of = mock.Mock(return_value="/sync/current")
        provider._build_menu = mock.Mock(return_value=["background-menu"])
        provider.get_file_items = mock.Mock(
            side_effect=AssertionError("background must not reuse selection actions")
        )

        self.assertEqual(provider.get_background_items(folder), ["background-menu"])
        provider._build_menu.assert_called_once_with(
            [(folder, "/sync/current")], "background"
        )

    def test_file_menu_is_bound_only_to_selected_paths(self):
        provider = ext.GOnedriveExtension()
        selected = mock.MagicMock()
        provider._path_of = mock.Mock(return_value="/sync/selected.docx")
        provider._build_menu = mock.Mock(return_value=["selection-menu"])

        self.assertEqual(provider.get_file_items([selected]), ["selection-menu"])
        provider._build_menu.assert_called_once_with(
            [(selected, "/sync/selected.docx")], "selection"
        )

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


class ConfiguredRootsTests(unittest.TestCase):
    """The roots decide where cloud badges and menu items may appear."""

    def _roots_with(self, body):
        with tempfile.TemporaryDirectory() as home:
            config_dir = os.path.join(home, "gonedrive")
            os.makedirs(config_dir)
            with open(os.path.join(config_dir, "config.toml"), "w") as handle:
                handle.write(body)
            with mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": home}):
                return ext.configured_roots()

    def test_a_relative_sync_root_is_refused_not_resolved_against_the_cwd(self):
        # The daemon refuses a relative sync_root. Resolving one here against
        # whatever directory Nautilus started in would badge an unrelated tree.
        (root,) = self._roots_with('sync_root = "relative/OneDrive"\n')
        self.assertEqual(root, os.path.join(os.path.expanduser("~"), "OneDrive"))

    def test_an_absolute_sync_root_is_used_as_written(self):
        (root,) = self._roots_with('sync_root = "/srv/cloud/OneDrive"\n')
        self.assertEqual(root, "/srv/cloud/OneDrive")

    def test_a_missing_or_broken_config_falls_back_to_the_default(self):
        with tempfile.TemporaryDirectory() as home:
            with mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": home}):
                (root,) = ext.configured_roots()
        self.assertEqual(root, os.path.join(os.path.expanduser("~"), "OneDrive"))
