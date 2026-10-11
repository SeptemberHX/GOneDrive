"""GOnedrive integration for the Nautilus file manager.

Shows, per file, whether its content is here or still only in the cloud, and
offers the two actions that matter: keep it here, or give the space back.

Two decisions keep this fast enough to run on every file Nautilus draws:

* **Status normally comes from the file's own extended attributes**, not from
  the daemon. Rendering a folder of a thousand files means a thousand answers,
  and a ``getxattr`` is a syscall where asking the daemon would be a round trip.
  An application-owned inode created by atomic save deliberately has no marks;
  inside the configured root it falls back to locally available.
* **Actions go over D-Bus and do not wait.** Freeing a folder can take a while;
  blocking here would freeze the file manager's UI thread.
"""

import os
import tomllib

import gi

gi.require_version("Nautilus", "4.1")
from gi.repository import Gio, GLib, GObject, Nautilus  # noqa: E402

XATTR_STATE = "user.gonedrive.state"
XATTR_PIN = "user.gonedrive.pin"
XATTR_SYNCING = "user.gonedrive.syncing"

BUS_NAME = "org.gonedrive.Daemon"
OBJECT_PATH = "/org/gonedrive/Daemon"
INTERFACE = "org.gonedrive.Daemon1"


def _prefers_chinese(env=os.environ):
    """Chinese for a Chinese session, English otherwise.

    The same rule as gonedrive_ipc::i18n::prefers_chinese, so the menu and the
    panel never disagree: no translation under the C locale, otherwise the
    first LANGUAGE entry, then LC_ALL, LC_MESSAGES and LANG.
    """
    locale = env.get("LC_ALL") or env.get("LC_MESSAGES") or env.get("LANG")
    if not locale or locale in ("C", "POSIX") or locale.startswith("C."):
        return False
    language = next((e for e in (env.get("LANGUAGE") or "").split(":") if e), None)
    return (language or locale).startswith("zh")


CHINESE = _prefers_chinese()


def _(en, zh):
    """The interface text in this session's language."""
    return zh if CHINESE else en


# Emblem names, minus the "emblem-" prefix Nautilus adds back.
EMBLEM_CLOUD = "gonedrive-cloud"
EMBLEM_LOCAL = "gonedrive-local"
EMBLEM_PINNED = "gonedrive-pinned"
EMBLEM_SYNCING = "gonedrive-syncing"
EMBLEM_ERROR = "gonedrive-error"


def configured_roots():
    """Return the local sync roots without contacting the daemon.

    File managers preserve user xattrs when copying a file.  The attributes
    therefore identify a placeholder only *inside* the configured sync tree;
    treating them as globally authoritative would put cloud badges and actions
    on an ordinary copy in Downloads.  Reading the small config file once when
    Nautilus loads the provider keeps status rendering offline and syscall-only.
    """
    config_home = os.environ.get("XDG_CONFIG_HOME")
    if not config_home:
        config_home = os.path.join(os.path.expanduser("~"), ".config")
    config_path = os.path.join(config_home, "gonedrive", "config.toml")
    default_root = os.path.join(os.path.expanduser("~"), "OneDrive")
    try:
        with open(config_path, "rb") as handle:
            root = tomllib.load(handle).get("sync_root", default_root)
    except (OSError, tomllib.TOMLDecodeError):
        root = default_root
    if not isinstance(root, str) or not root.strip():
        root = default_root
    root = os.path.expanduser(root)
    # The daemon refuses a relative sync_root outright.  Resolving one here
    # against whatever directory Nautilus happened to start in would put cloud
    # badges and context-menu actions on some unrelated tree; agree with the
    # daemon and use the default instead.
    if not os.path.isabs(root):
        root = default_root
    return (os.path.normpath(root),)


def config_stamp():
    """Identity of the config file, for noticing an edit without re-reading it.

    The roots used to be read once when Nautilus loaded the provider, so
    changing sync_root meant restarting the file manager — and nothing said so.
    One stat per query is far cheaper than the xattr reads already happening on
    the same path.
    """
    config_home = os.environ.get("XDG_CONFIG_HOME")
    if not config_home:
        config_home = os.path.join(os.path.expanduser("~"), ".config")
    config_path = os.path.join(config_home, "gonedrive", "config.toml")
    try:
        stat = os.stat(config_path)
    except OSError:
        return None
    return (stat.st_mtime_ns, stat.st_size, stat.st_ino)


def is_in_sync_root(path, roots):
    """Use component boundaries, so ``OneDrive-old`` is not in ``OneDrive``."""
    absolute = os.path.abspath(path)
    for root in roots:
        try:
            if os.path.commonpath((absolute, root)) == root:
                return True
        except ValueError:
            # Different drives on platforms where paths can name a drive.
            continue
    return False


def describe_scope(managed):
    """Human words for exactly what a menu action will touch."""
    if len(managed) == 1:
        file, path = managed[0]
        name = os.path.basename(path)
        if file.is_directory():
            return _(f'the folder "{name}"', f"整个文件夹「{name}」")
        return _(f'"{name}"', f"「{name}」")
    return _(f"the {len(managed)} selected items", f"选中的 {len(managed)} 项")


def menu_item_name(namespace, action):
    """Return a Nautilus action id unique to its selection scope.

    Nautilus installs both the selected-item and folder-background menus in
    the same action map. Reusing an id lets the later background action replace
    the selected-file callback, which can turn "free this file" into "free the
    whole folder".
    """
    return f"GOnedrive::{namespace}::{action}"


def _xattr(path, name):
    try:
        return os.getxattr(path, name).decode("utf-8")
    except (OSError, UnicodeDecodeError):
        # Missing, unreadable, or not ours to interpret: no status is better
        # than an exception thrown into Nautilus for every file it draws.
        return None


class Status:
    """What to show for one file."""

    def __init__(self, emblem, label):
        self.emblem = emblem
        self.label = label


ONLINE_ONLY = Status(EMBLEM_CLOUD, _("Online-only", "仅在线"))
AVAILABLE = Status(EMBLEM_LOCAL, _("Available on this device", "在此设备上可用"))
ALWAYS_KEEP = Status(EMBLEM_PINNED, _("Always keep on this device", "始终保留在此设备"))
SYNCING = Status(EMBLEM_SYNCING, _("Syncing", "正在同步"))
FAILED = Status(EMBLEM_ERROR, _("Sync error", "同步出错"))


# Folders carry the same xattrs but read as a different pair of states: a
# directory is a container, not a file, so "online-only" means nothing inside
# is here and "available" means some content is. Windows badges folders too,
# and a tree with unbadged folders looks half-managed.
FOLDER_ONLINE = Status(EMBLEM_CLOUD, _("Online-only", "仅在线"))
FOLDER_LOCAL = Status(EMBLEM_LOCAL, _("Has local content", "包含本地内容"))
FOLDER_PINNED = Status(EMBLEM_PINNED, _("Always keep on this device", "始终保留在此设备"))


def status_of(path, is_dir=False, managed_without_xattr=False):
    """Classify a path already known to be inside a GOnedrive sync root."""
    state = _xattr(path, XATTR_STATE)
    if state is None:
        # Atomic-save APIs replace the inode and discard its user xattrs. The
        # daemon intentionally leaves that application-owned inode alone so an
        # editor does not receive a false external-modification notification.
        # Callers may use this fallback only after `_path_of` proved the file is
        # inside a configured sync root.
        if (
            managed_without_xattr
            and not is_dir
            and not os.path.islink(path)
            and os.path.isfile(path)
        ):
            return AVAILABLE
        return None
    # A transfer in flight wins over the resting state, so an upload or download
    # reads as "syncing" rather than the check or cloud it will settle into.
    if _xattr(path, XATTR_SYNCING) is not None:
        return SYNCING
    if is_dir:
        if _xattr(path, XATTR_PIN) == "pinned":
            return FOLDER_PINNED
        return FOLDER_LOCAL if state == "hydrated" else FOLDER_ONLINE
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
        self._roots = configured_roots()
        self._roots_stamp = config_stamp()

    def roots(self):
        """The configured sync roots, re-read when the config file changes."""
        stamp = config_stamp()
        if stamp != self._roots_stamp:
            self._roots = configured_roots()
            self._roots_stamp = stamp
        return self._roots

    # ---- status ---------------------------------------------------------

    def get_columns(self):
        """A sortable status column, the way Explorer shows one."""
        return [
            Nautilus.Column(
                name="GOnedrive::status",
                attribute="gonedrive_status",
                label=_("Status", "状态"),
                description=_("Whether the file's content is on this device", "文件内容是否在本机"),
            )
        ]

    def update_file_info(self, file):
        path = self._path_of(file)
        if path is None:
            return
        status = status_of(path, file.is_directory(), managed_without_xattr=True)
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

        return self._build_menu(managed, "selection")

    def _build_menu(self, managed, namespace):
        """Build one menu bound to exactly the paths in ``managed``."""

        statuses = [
            status_of(p, f.is_directory(), managed_without_xattr=True)
            for f, p in managed
        ]
        # A folder carries no status of its own but still has contents worth
        # acting on, so it counts as actionable when it is inside the tree.
        folders = [p for f, p in managed if f.is_directory()]
        if not any(statuses) and not folders:
            return []

        paths = [p for _, p in managed]
        # The label carries the exact scope. The report that forced this: a
        # selection quietly holding one extra file made "释放空间" free a
        # sibling the user never meant to touch — a label that says what it
        # will act on turns that from a mystery into a visible mistake.
        scope = describe_scope(managed)
        items = []

        if any(s in (ONLINE_ONLY, AVAILABLE) for s in statuses) or folders:
            keep = Nautilus.MenuItem(
                name=menu_item_name(namespace, "pin"),
                label=_(f"Always keep {scope} on this device", f"始终保留{scope}"),
                tip=_(
                    "Download and keep it; anything added later downloads too",
                    "下载并保留，之后新增的内容也会自动下载",
                ),
            )
            keep.connect("activate", self._on_pin, paths, "pinned")
            items.append(keep)

        if any(s in (AVAILABLE, ALWAYS_KEEP) for s in statuses) or folders:
            free = Nautilus.MenuItem(
                name=menu_item_name(namespace, "free"),
                label=_(f"Free up space used by {scope}", f"释放{scope}的空间"),
                tip=_(
                    "Remove the local copy; the file stays and downloads again when opened",
                    "移除本机内容，文件保留，打开时自动重新下载",
                ),
            )
            free.connect("activate", self._on_free, paths)
            items.append(free)

        if any(s is ONLINE_ONLY for s in statuses):
            fetch = Nautilus.MenuItem(
                name=menu_item_name(namespace, "fetch"),
                label=_(f"Download {scope} now", f"立即下载{scope}"),
                tip=_("Bring the content to this device now", "现在就把内容取回本机"),
            )
            fetch.connect("activate", self._on_fetch, paths)
            items.append(fetch)

        if not items:
            return []

        top = Nautilus.MenuItem(
            name=menu_item_name(namespace, "menu"), label="GOnedrive"
        )
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
        if not folder.is_directory():
            return []
        # Keep this construction independent from `get_file_items`: both
        # providers coexist in Nautilus, but their callbacks must never share an
        # action id or capture each other's paths.
        return self._build_menu([(folder, path)], "background")

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

    def _path_of(self, file):
        if file.get_uri_scheme() != "file":
            return None
        path = Gio.File.new_for_uri(file.get_uri()).get_path()
        if not path or not os.path.lexists(path):
            return None
        return path if is_in_sync_root(path, self.roots()) else None
