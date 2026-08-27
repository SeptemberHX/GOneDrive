"""GOnedrive integration for the Nautilus file manager.

Shows, per file, whether its content is here or still only in the cloud, and
offers the two actions that matter: keep it here, or give the space back.

Two decisions keep this fast enough to run on every file Nautilus draws:

* **Status comes from the file's own extended attributes**, not from the
  daemon. Rendering a folder of a thousand files means a thousand answers, and
  a ``getxattr`` is a syscall where asking the daemon would be a round trip.
  It also means the emblems are still right when the daemon is not running.
* **Actions go over D-Bus and do not wait.** Freeing a folder can take a while;
  blocking here would freeze the file manager's UI thread.
"""

import os

import gi

gi.require_version("Nautilus", "4.1")
from gi.repository import Gio, GLib, GObject, Nautilus  # noqa: E402

XATTR_STATE = "user.gonedrive.state"
XATTR_PIN = "user.gonedrive.pin"

BUS_NAME = "org.gonedrive.Daemon"
OBJECT_PATH = "/org/gonedrive/Daemon"
INTERFACE = "org.gonedrive.Daemon1"

# Emblem names, minus the "emblem-" prefix Nautilus adds back.
EMBLEM_CLOUD = "gonedrive-cloud"
EMBLEM_LOCAL = "gonedrive-local"
EMBLEM_PINNED = "gonedrive-pinned"
EMBLEM_SYNCING = "gonedrive-syncing"
EMBLEM_ERROR = "gonedrive-error"


def _xattr(path, name):
    try:
        return os.getxattr(path, name).decode("utf-8")
    except OSError:
        return None


class Status:
    """What to show for one file."""

    def __init__(self, emblem, label):
        self.emblem = emblem
        self.label = label


ONLINE_ONLY = Status(EMBLEM_CLOUD, "仅在线")
AVAILABLE = Status(EMBLEM_LOCAL, "在此设备上可用")
ALWAYS_KEEP = Status(EMBLEM_PINNED, "始终保留在此设备")
SYNCING = Status(EMBLEM_SYNCING, "正在同步")
FAILED = Status(EMBLEM_ERROR, "同步出错")


def status_of(path):
    """Classify a path, or return None if GOnedrive does not manage it."""
    state = _xattr(path, XATTR_STATE)
    if state is None:
        return None
    if state == "dehydrated":
        return ONLINE_ONLY
    if state == "hydrating":
        return SYNCING
    if state != "hydrated":
        return FAILED
    # Downloaded. Whether it stays is the user's choice, and Windows draws
    # those two cases differently, so we do too.
    return ALWAYS_KEEP if _xattr(path, XATTR_PIN) == "pinned" else AVAILABLE


class GOnedriveExtension(
    GObject.GObject,
    Nautilus.InfoProvider,
    Nautilus.MenuProvider,
    Nautilus.ColumnProvider,
):
    def __init__(self):
        super().__init__()
        self._bus = None

    # ---- status ---------------------------------------------------------

    def get_columns(self):
        """A sortable status column, the way Explorer shows one."""
        return [
            Nautilus.Column(
                name="GOnedrive::status",
                attribute="gonedrive_status",
                label="状态",
                description="文件内容是否在本机",
            )
        ]

    def update_file_info(self, file):
        path = self._path_of(file)
        if path is None:
            return
        status = status_of(path)
        if status is None:
            return
        file.add_emblem(status.emblem)
        file.add_string_attribute("gonedrive_status", status.label)

    # ---- menu -----------------------------------------------------------

    def get_file_items(self, *args):
        # Nautilus dropped the window argument from this callback; accept both
        # so the extension works across versions.
        files = args[-1]
        managed = [(f, self._path_of(f)) for f in files]
        managed = [(f, p) for f, p in managed if p is not None]
        if not managed:
            return []

        statuses = [status_of(p) for _, p in managed]
        # A folder carries no status of its own but still has contents worth
        # acting on, so it counts as actionable when it is inside the tree.
        folders = [p for f, p in managed if f.is_directory()]
        if not any(statuses) and not folders:
            return []

        paths = [p for _, p in managed]
        items = []

        if any(s in (ONLINE_ONLY, AVAILABLE) for s in statuses) or folders:
            keep = Nautilus.MenuItem(
                name="GOnedrive::pin",
                label="始终保留在此设备",
                tip="下载并保留，之后新增的内容也会自动下载",
            )
            keep.connect("activate", self._on_pin, paths, "pinned")
            items.append(keep)

        if any(s in (AVAILABLE, ALWAYS_KEEP) for s in statuses) or folders:
            free = Nautilus.MenuItem(
                name="GOnedrive::free",
                label="释放空间",
                tip="移除本机内容，文件保留，打开时自动重新下载",
            )
            free.connect("activate", self._on_free, paths)
            items.append(free)

        if any(s is ONLINE_ONLY for s in statuses):
            fetch = Nautilus.MenuItem(
                name="GOnedrive::fetch",
                label="立即下载",
                tip="现在就把内容取回本机",
            )
            fetch.connect("activate", self._on_fetch, paths)
            items.append(fetch)

        if not items:
            return []

        top = Nautilus.MenuItem(name="GOnedrive::menu", label="GOnedrive")
        submenu = Nautilus.Menu()
        for item in items:
            submenu.append_item(item)
        top.set_submenu(submenu)
        return [top]

    def get_background_items(self, *args):
        folder = args[-1]
        path = self._path_of(folder)
        if path is None:
            return []
        # `get_file_items` takes a list; the background callback hands over a
        # single folder.
        return self.get_file_items([folder]) if folder.is_directory() else []

    # ---- daemon ---------------------------------------------------------

    def _on_pin(self, _menu, paths, pin):
        for path in paths:
            self._call("SetPinState", GLib.Variant("(ss)", (path, pin)))

    def _on_free(self, _menu, paths):
        for path in paths:
            self._call("FreeUpSpace", GLib.Variant("(s)", (path,)))

    def _on_fetch(self, _menu, paths):
        for path in paths:
            self._call("FetchNow", GLib.Variant("(s)", (path,)))

    def _call(self, method, args):
        """Fire a D-Bus method without waiting for it.

        The file manager's UI runs on this thread; freeing a large folder can
        take seconds, and blocking here would freeze it.
        """
        try:
            if self._bus is None:
                self._bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            self._bus.call(
                BUS_NAME,
                OBJECT_PATH,
                INTERFACE,
                method,
                args,
                None,
                Gio.DBusCallFlags.NONE,
                -1,
                None,
                self._on_reply,
                method,
            )
        except Exception as error:  # noqa: BLE001 - never break the file manager
            print(f"gonedrive: {method} failed: {error}")

    def _on_reply(self, source, result, method):
        try:
            source.call_finish(result)
        except Exception as error:  # noqa: BLE001
            print(f"gonedrive: {method} failed: {error}")

    # ---- helpers --------------------------------------------------------

    @staticmethod
    def _path_of(file):
        if file.get_uri_scheme() != "file":
            return None
        path = Gio.File.new_for_uri(file.get_uri()).get_path()
        return path if path and os.path.lexists(path) else None
