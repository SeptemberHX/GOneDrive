<div align="center">

**English** | [简体中文](README.zh-CN.md)

<img src="data/icons/gonedrive-app.svg" width="96" alt="GOnedrive">

# GOnedrive

**A OneDrive client for Linux with two-way sync and real files-on-demand — it works like the Windows client.**

Built with Rust, GTK 4 and libadwaita.

[![CI](https://github.com/SeptemberHX/GOneDrive/actions/workflows/ci.yml/badge.svg)](https://github.com/SeptemberHX/GOneDrive/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SeptemberHX/GOneDrive)](https://github.com/SeptemberHX/GOneDrive/releases)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](LICENSE)

</div>

> [!NOTE]
> The user interface (panel, tray menu and Nautilus menu) is currently in **Simplified Chinese only**. The command line, logs and documentation are in English. Translations are welcome.

---

## What it does

- **Your whole drive is on disk, using almost no space.** Files appear in `~/OneDrive` as placeholders with their real size and dates, and download automatically the first time they are opened.
- **Every program just works.** There is no mount point and no special "open with": LibreOffice, `cat`, `cp` or an image viewer opening a placeholder transparently gets the real content.
- **Two-way sync.** Local creates, edits, deletes and renames go to the cloud, and the other way round. When both sides change the same file, both versions are kept; nothing is silently overwritten.
- **File manager integration.** Every file in Nautilus shows a status emblem, and the context menu offers "Always keep on this device" and "Free up space".
- **Tray panel.** Quota, sync progress and recent activity, plus choosing which folders stay on this device.
- **Browsing never downloads.** Thumbnails, previews and desktop indexers do not trigger downloads; only actually opening or copying a file does.

### How it compares

| | Sync folder is a normal folder | Files-on-demand | GUI |
|---|:-:|:-:|:-:|
| [abraunegg/onedrive](https://github.com/abraunegg/onedrive) | ✅ | ❌ | ❌ |
| [onedriver](https://github.com/jstaf/onedriver) / `rclone mount` | ❌ (FUSE mount) | ✅ | ❌ |
| **GOnedrive** | ✅ | ✅ | ✅ |

Files-on-demand is built on the fanotify pre-content events added in Linux 6.14, the closest Linux equivalent of Windows placeholder files (the Cloud Files API). See the [design notes](docs/DESIGN.md) (Chinese) for how it works.

---

## Requirements

| | |
|---|---|
| Distribution | **Ubuntu 26.04 or newer** (the `.deb` is built on 26.04) |
| Kernel | **6.14 or newer** (files-on-demand needs fanotify pre-content events) |
| Desktop | GNOME (for Nautilus emblems and menus); the tray icon needs the [AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/), which Ubuntu ships by default |
| Account | Personal OneDrive, or a work/school Microsoft 365 account |

Other distributions with kernel ≥ 6.14, GTK ≥ 4.18 and libadwaita ≥ 1.7 can [build from source](docs/BUILDING.md).

---

## Install

1. Download the latest `gonedrive_<version>_amd64.deb` from [Releases](https://github.com/SeptemberHX/GOneDrive/releases/latest).
2. Install it:

   ```bash
   sudo apt install ./gonedrive_*_amd64.deb
   ```

   This enables two services: the `gonedrive-hsm` system service for files-on-demand, and the `gonedrived` user service that does the syncing.

3. Log out and back in once, so Nautilus loads the extension and the tray starts with your session.

To upgrade, download the newer `.deb` and repeat step 2.

## Getting started

1. Open **GOnedrive** from the app grid and click **登录** (Sign in).
2. Your browser opens Microsoft's sign-in page. Sign in with your own Microsoft account and grant access. Credentials are stored in the system keyring.
3. Your drive's top-level folders appear in `~/OneDrive`. Syncing continues in the background, even with the window closed.

### The panel

Click the tray icon (or open GOnedrive from the app grid) to open the panel. Closing it only hides it; syncing carries on.

| Page | What it shows |
|---|---|
| **Overview** | Account and quota, sync status, current transfers, "Sync now" and "Open folder" |
| **Folders** | Your drive's folders, each with a switch: on keeps it on this device, off frees its space (files stay openable) |
| **Activity** | Recent downloads, uploads, deletes, conflicts and errors |
| **Settings** (header menu) | Pause syncing, on-demand mode, sync folder, sign out |

Opening a file that is not downloaded yet shows a small progress window, which closes by itself when the download finishes.

### File status

| Emblem | Meaning |
|---|---|
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-cloud.svg" width="20"> | Online-only; downloads when opened |
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-local.svg" width="20"> | Available on this device; can be freed when space runs low |
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-pinned.svg" width="20"> | Always kept on this device |
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-syncing.svg" width="20"> | Syncing |
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-error.svg" width="20"> | Sync error; see the Activity page in the panel |

Right-click a file or folder in Nautilus for the GOnedrive submenu:

- **Always keep on this device**: download now and keep it available offline, including files added to the folder later.
- **Free up space**: remove the local copy. The file turns back into a placeholder and can still be opened at any time.
- **Download now**: fetch it now instead of on first open.

### Command line

Everything the panel does is also available from the terminal:

```bash
gonedrived status                    # account, quota, pending work
gonedrived sync                      # sync once now
gonedrived pin Documents/Contracts   # always keep on this device
gonedrived free-space Pictures/2023  # free up space; files stay as placeholders
gonedrived fetch Documents/report.docx  # download this file now
gonedrived storage Documents         # what actually uses disk space
gonedrived mode                      # on-demand (default) or full (download everything)
gonedrived logout                    # sign out; local files are kept
```

See `gonedrived --help` for the full list. The configuration file is `~/.config/gonedrive/config.toml`; [config.example.toml](data/config.example.toml) documents every option (sync folder, bandwidth limits, automatic freeing when the disk runs low, and more).

---

## FAQ

<details>
<summary><b>The tray icon does not appear</b></summary>

GNOME has no tray of its own; enable the [AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/) (on Ubuntu it is the `gnome-shell-extension-appindicator` package). Syncing works without the tray; open GOnedrive from the app grid to see the panel.
</details>

<details>
<summary><b>Opening or copying a file fails with "Operation not permitted"</b></summary>

The on-demand download was refused, usually because the helper service is not running or OneDrive cannot be reached. Check the service:

```bash
systemctl status gonedrive-hsm
journalctl -u gonedrive-hsm -n 50
```

If you ever installed from source with `install.sh`, the old files in `/usr/local/bin` and `/etc/systemd/system` take precedence over the package. Remove them first (see [Building and development](docs/BUILDING.md#installing-a-build)).
</details>

<details>
<summary><b>Desktop search does not find files in OneDrive</b></summary>

By default GOnedrive asks GNOME's file indexer to skip the sync folder, because the indexer's recursive crawl would otherwise load the entire drive folder by folder. To turn search back on:

```bash
gonedrived indexer --include
```
</details>

<details>
<summary><b>A "conflict copy" file appeared</b></summary>

When the same file is changed locally and in the cloud at the same time, GOnedrive overwrites neither. The cloud version keeps the original name, and the local version is renamed to `name (<hostname> 的冲突副本 <timestamp>).ext` and uploaded. Keep the one you want and delete the other.
</details>

<details>
<summary><b>Sign-in says a client_id is missing</b></summary>

Your build has no application id baked in. Set `client_id` in `~/.config/gonedrive/config.toml`, or register an application as described in [Building and development](docs/BUILDING.md#registering-an-entra-id-application-once-about-3-minutes). A client id only identifies which program is asking for access; you still sign in with your own account.
</details>

<details>
<summary><b>Why is there a service running as root?</b></summary>

Intercepting reads of placeholder files needs the kernel's fanotify pre-content events, which are only available to processes with `CAP_SYS_ADMIN`. GOnedrive keeps that part in a small separate service, `gonedrive-hsm`: it has no network access, never sees your credentials, only acts on files owned by the user who asks, and runs with a restricted systemd capability set. Syncing, downloading and the UI all run as your normal user.
</details>

<details>
<summary><b>Uninstalling</b></summary>

```bash
sudo apt remove gonedrive
```

Files in `~/OneDrive` are kept, but **placeholders can no longer be read after uninstalling** (your data in the cloud is unaffected). To keep using some files offline afterwards, mark them "Always keep on this device" first, or run `gonedrived mode full` to download everything.
</details>

---

## Documentation

- [Building and development](docs/BUILDING.md): building from source, registering an Entra app, tests, releases
- [Design notes](docs/DESIGN.md) (Chinese): the sync engine and files-on-demand internals, known trade-offs, diagnostic logs

## License

[GPL-3.0-or-later](LICENSE)
