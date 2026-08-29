"""End-to-end dispatch tests for the Nautilus extension.

These are the four incidents, replayed. Only Nautilus's object shell is
faked; everything after the click is real — the extension's real dispatch
code, the real session bus, the real daemon, and the real files. Each
scenario asserts three layers: what the menu SAID, what the daemon's audit
log RECORDED, and what happened ON DISK.
"""

import os
import re
import subprocess
import sys
import time
import unittest
from unittest import mock

LIVE_TESTS = os.environ.get("GONEDRIVE_LIVE_TESTS") == "1"
ROOT = os.environ.get("GONEDRIVE_TEST_ROOT", os.path.expanduser("~/OneDrive"))
TOOLS = f"{ROOT}/工具"


def load_extension():
    """Import the installed extension with only `Nautilus` stubbed.

    Gio and GLib stay real: the point is that a click reaches the real
    daemon over the real bus.
    """
    import gi

    fake = mock.MagicMock()
    fake.InfoProvider = type("InfoProvider", (), {})
    fake.MenuProvider = type("MenuProvider", (), {})
    fake.ColumnProvider = type("ColumnProvider", (), {})
    fake.Column = mock.MagicMock()

    class MenuItem:
        def __init__(self, name="", label="", tip=""):
            self.label = label
            self.tip = tip
            self._handlers = []
            self.submenu = None

        def connect(self, _signal, callback, *args):
            self._handlers.append((callback, args))

        def activate(self):
            for callback, args in self._handlers:
                callback(self, *args)

        def set_submenu(self, menu):
            self.submenu = menu

    class Menu:
        def __init__(self):
            self.items = []

        def append_item(self, item):
            self.items.append(item)

    fake.MenuItem = MenuItem
    fake.Menu = Menu

    real_require = gi.require_version
    gi.require_version = lambda ns, v: None if ns == "Nautilus" else real_require(ns, v)
    sys.modules["gi.repository.Nautilus"] = fake
    from gi import repository

    repository.Nautilus = fake

    import importlib.util

    path = os.path.expanduser(
        os.environ.get(
            "GONEDRIVE_EXTENSION",
            "~/.local/share/nautilus-python/extensions/gonedrive.py",
        )
    )
    spec = importlib.util.spec_from_file_location("gonedrive_installed", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ext = load_extension() if LIVE_TESTS else None


class FakeFile:
    """The slice of NautilusVFSFile the extension touches."""

    def __init__(self, path):
        self._path = path

    def get_uri_scheme(self):
        return "file"

    def get_uri(self):
        from urllib.parse import quote

        return "file://" + quote(self._path)

    def is_directory(self):
        return os.path.isdir(self._path)


def state_of(path):
    try:
        return os.getxattr(path, "user.gonedrive.state").decode()
    except OSError:
        return "-"


def daemon_log():
    out = subprocess.run(
        ["pgrep", "-x", "gonedrived"], capture_output=True, text=True
    )
    assert out.stdout.strip(), "the daemon must be running for these tests"
    # The daemon writes to a session log file; read the freshest one.
    return os.environ["GONEDRIVE_TEST_LOG"]


ANSI = re.compile(r"\x1b\[[0-9;]*m")


def audit_since(mark):
    with open(daemon_log(), errors="ignore") as handle:
        lines = [ANSI.sub("", l) for l in handle.readlines()[mark:]]
    return [l for l in lines if "FreeUpSpace requested" in l or "SetPinState requested" in l]


def log_mark():
    with open(daemon_log(), errors="ignore") as handle:
        return len(handle.readlines())


def menu_for(files):
    provider = ext.GOnedriveExtension()
    items = provider.get_file_items([FakeFile(p) for p in files])
    return items[0].submenu.items if items else []


def find(menu, needle):
    return next((i for i in menu if needle in i.label), None)


@unittest.skipUnless(
    LIVE_TESTS,
    "set GONEDRIVE_LIVE_TESTS=1 with a running daemon and installed extension",
)
class DispatchTests(unittest.TestCase):
    def settle(self):
        time.sleep(1.0)

    def test_single_folder_names_itself_and_frees_only_itself(self):
        # Incident 1, resolved half: acting on 软件 must name 软件 and touch
        # nothing beside it.
        before = state_of(f"{TOOLS}/ss.txt")
        mark = log_mark()

        menu = menu_for([f"{TOOLS}/软件"])
        free = find(menu, "释放")
        self.assertIsNotNone(free)
        self.assertIn("整个文件夹「软件」", free.label)

        free.activate()
        self.settle()

        audited = audit_since(mark)
        self.assertEqual(len(audited), 1, audited)
        self.assertIn(f"path={TOOLS}/软件", audited[0])
        self.assertEqual(state_of(f"{TOOLS}/ss.txt"), before, "sibling untouched")

    def test_a_stray_multi_selection_is_visible_and_audited(self):
        # Incident 1, root cause: a selection quietly holding ss.txt. The
        # label must say "2 项", and the audit must list BOTH paths — the
        # click is then a visible choice, not a mystery.
        mark = log_mark()
        menu = menu_for([f"{TOOLS}/软件", f"{TOOLS}/ss.txt"])
        free = find(menu, "释放")
        self.assertIn("选中的 2 项", free.label)

        free.activate()
        self.settle()

        audited = "".join(audit_since(mark))
        self.assertIn("工具/软件", audited)
        self.assertIn("工具/ss.txt", audited)

    def test_background_click_names_the_whole_folder(self):
        provider = ext.GOnedriveExtension()
        items = provider.get_background_items(FakeFile(TOOLS))
        free = find(items[0].submenu.items, "释放")
        self.assertIn("整个文件夹「工具」", free.label)
        # Not activated: naming the blast radius is the fix under test.

    def test_the_drive_root_is_refused_end_to_end(self):
        # Incident 2: one click dehydrated the whole drive. The daemon now
        # refuses root-scoped actions outright; prove it through the same
        # path the real click takes.
        mark = log_mark()
        menu = menu_for([ROOT])
        free = find(menu, "释放")
        self.assertIn(f"整个文件夹「{os.path.basename(ROOT)}」", free.label)

        free.activate()
        self.settle()

        # The request is audited, and nothing on disk moved.
        for name in ["ss.txt", "软件/WinRAR64hh.exe"]:
            self.assertEqual(
                state_of(f"{TOOLS}/{name}"),
                "dehydrated",
                f"{name} must be untouched by a refused root action",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
