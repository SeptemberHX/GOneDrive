<div align="center">

[English](README.md) | **简体中文**

<img src="data/icons/gonedrive-app.svg" width="96" alt="GOnedrive">

# GOnedrive

**Linux 上的 OneDrive 客户端：双向同步 + 文件按需下载，用起来像 Windows 版。**

*A OneDrive client for Linux with two-way sync and true files-on-demand, built with Rust, GTK 4 and libadwaita.*

[![CI](https://github.com/SeptemberHX/GOneDrive/actions/workflows/ci.yml/badge.svg)](https://github.com/SeptemberHX/GOneDrive/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SeptemberHX/GOneDrive)](https://github.com/SeptemberHX/GOneDrive/releases)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](LICENSE)

</div>

---

## 它能做什么

- **整个网盘都在本地，但几乎不占空间。** 文件以“占位文件”出现在 `~/OneDrive` 里，大小、时间都是真实的；第一次打开时才自动下载。
- **任何程序都能直接用。** 不是挂载点，也不需要专用的打开方式：LibreOffice、`cat`、`cp`、图片查看器打开占位文件时都会透明地拿到真实内容。
- **双向同步。** 本地新建、修改、删除、重命名都会同步到云端，反之亦然。两边同时修改时保留两份，不会悄悄覆盖。
- **文件管理器集成。** Nautilus 里每个文件都有状态角标，右键即可“始终保留在此设备”或“释放空间”。
- **托盘面板。** 查看配额、同步进度和最近活动，选择要保留在本机的文件夹。
- **浏览不下载。** 文件管理器生成预览、桌面索引器扫描目录都不会触发下载，只有真正打开或复制文件时才会。

### 和现有方案的区别

| | 同步目录是普通文件夹 | 按需下载 | 图形界面 |
|---|:-:|:-:|:-:|
| [abraunegg/onedrive](https://github.com/abraunegg/onedrive) | ✅ | ❌ | ❌ |
| [onedriver](https://github.com/jstaf/onedriver) / `rclone mount` | ❌（FUSE 挂载点） | ✅ | ❌ |
| **GOnedrive** | ✅ | ✅ | ✅ |

按需下载使用 Linux 6.14 引入的 fanotify 预读事件，这是 Linux 上最接近 Windows 占位文件（CFAPI）的机制。原理见 [设计说明](docs/DESIGN.md)。

---

## 系统要求

| | 要求 |
|---|---|
| 发行版 | **Ubuntu 26.04 或更新**（deb 包在 26.04 上构建） |
| 内核 | **≥ 6.14**（按需下载依赖 fanotify 预读事件） |
| 桌面 | GNOME（Nautilus 角标与右键菜单）；托盘图标需要 [AppIndicator 扩展](https://extensions.gnome.org/extension/615/appindicator-support/)，Ubuntu 默认已装 |
| 账户 | 个人版 OneDrive，或工作/学校 Microsoft 365 账户 |

其他发行版只要满足内核 ≥ 6.14、GTK ≥ 4.18、libadwaita ≥ 1.7，可以 [从源码构建](docs/BUILDING.zh-CN.md)。

---

## 安装

1. 从 [Releases](https://github.com/SeptemberHX/GOneDrive/releases/latest) 下载最新的 `gonedrive_<版本>_amd64.deb`。
2. 安装：

   ```bash
   sudo apt install ./gonedrive_*_amd64.deb
   ```

   安装过程会自动启用两个服务：用于按需下载的系统服务 `gonedrive-hsm`，和负责同步的用户服务 `gonedrived`。

3. 注销并重新登录一次（让 Nautilus 加载扩展、托盘程序随会话启动）。

升级时下载新版 deb 重复第 2 步即可。

## 开始使用

1. 从应用列表打开 **GOnedrive**，点击 **登录**。
2. 浏览器会打开微软的登录页，用你自己的 Microsoft 账户登录并同意授权。凭据保存在系统密钥环中。
3. 登录完成后，`~/OneDrive` 会出现网盘根目录的内容。之后的同步都在后台进行，关闭窗口也不会停止。

### 面板

单击托盘图标（或从应用列表打开 GOnedrive）即可打开面板。关闭面板只是隐藏，同步会继续进行。

| 页面 | 内容 |
|---|---|
| **概览** | 账户与配额、同步状态、当前传输，以及“立即同步”“打开文件夹” |
| **文件夹** | 网盘里的文件夹，每个都有开关：打开即保留在此设备，关闭即释放空间（文件仍可打开） |
| **活动** | 最近的下载、上传、删除、冲突和错误 |
| **设置**（标题栏菜单） | 暂停同步、按需下载模式、同步文件夹、退出登录 |

打开尚未下载的文件时，会弹出一个小的进度窗口，下载完成后自动关闭。

### 文件状态

| 角标 | 含义 |
|---|---|
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-cloud.svg" width="20"> | 仅在云端，打开时自动下载 |
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-local.svg" width="20"> | 已下载到本机，空间紧张时可以释放 |
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-pinned.svg" width="20"> | 始终保留在此设备 |
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-syncing.svg" width="20"> | 正在同步 |
| <img src="data/icons/hicolor/scalable/emblems/emblem-gonedrive-error.svg" width="20"> | 同步出错，详情见面板的“活动”页 |

在 Nautilus 中右键文件或文件夹，GOnedrive 子菜单提供：

- **始终保留在此设备**：立即下载，并保持离线可用（包括文件夹里以后新增的文件）。
- **释放空间**：删除本地副本，文件变回占位文件，仍然可以随时打开。
- **立即下载**：现在就下载，而不是等第一次打开。

### 命令行

图形界面能做的事，命令行也都能做：

```bash
gonedrived status                  # 账户、配额、待处理队列
gonedrived sync                    # 立即同步一次
gonedrived pin  文档/合同           # 始终保留在此设备
gonedrived free-space 图片/2023     # 释放空间，文件留在原地变成占位
gonedrived fetch 文档/报告.docx      # 现在就下载这个文件
gonedrived storage 文档             # 查看每个文件实际占用多少磁盘
gonedrived mode                    # 查看模式：on-demand（默认）或 full（全部下载）
gonedrived logout                  # 退出登录，本地文件保留
```

完整列表见 `gonedrived --help`。配置文件位于 `~/.config/gonedrive/config.toml`，可选项及说明见 [config.example.toml](data/config.example.toml)（同步目录、带宽限制、空间不足时自动释放等）。

---

## 常见问题

<details>
<summary><b>托盘图标不显示</b></summary>

GNOME 本身不支持托盘图标，需要启用 [AppIndicator 扩展](https://extensions.gnome.org/extension/615/appindicator-support/)（Ubuntu 自带，包名 `gnome-shell-extension-appindicator`）。没有托盘时同步照常进行，从应用列表打开 GOnedrive 即可看到面板。
</details>

<details>
<summary><b>打开或复制文件时提示“不允许的操作”</b></summary>

这表示按需下载被拒绝了，通常是因为特权服务没有运行，或网络无法访问 OneDrive。检查服务状态：

```bash
systemctl status gonedrive-hsm
journalctl -u gonedrive-hsm -n 50
```

如果以前用 `install.sh` 从源码装过，`/usr/local/bin` 和 `/etc/systemd/system` 下的旧文件会覆盖 deb 包的版本，请先删掉它们（见 [构建与开发](docs/BUILDING.zh-CN.md#安装方式)）。
</details>

<details>
<summary><b>系统搜索搜不到 OneDrive 里的文件</b></summary>

默认情况下，GOnedrive 会让 GNOME 的文件索引器跳过同步目录，否则索引器的递归扫描会把整个网盘逐层加载一遍。如果你需要搜索，可以重新开启：

```bash
gonedrived indexer --include
```
</details>

<details>
<summary><b>出现了“冲突副本”文件</b></summary>

同一个文件在本地和云端同时被修改时，GOnedrive 不会覆盖任何一边：云端版本保留原名，本地版本改名为 `文件名 (主机名 的冲突副本 时间戳).扩展名` 并上传。确认要保留哪份后删掉另一份即可。
</details>

<details>
<summary><b>登录时提示缺少 client_id</b></summary>

你用的是没有内置应用 ID 的构建。在 `~/.config/gonedrive/config.toml` 中设置 `client_id`，或按 [构建与开发](docs/BUILDING.zh-CN.md#注册-entra-id-应用一次性约-3-分钟) 注册一个应用。client_id 只标识“哪个程序在请求访问”，你仍然用自己的账户登录。
</details>

<details>
<summary><b>为什么需要一个 root 运行的服务？</b></summary>

拦截“读取占位文件”这件事需要内核的 fanotify 预读事件，而它只对具有 `CAP_SYS_ADMIN` 的进程开放。因此 GOnedrive 把这一小部分拆成独立的 `gonedrive-hsm` 服务：它不联网、不接触登录凭据、只处理调用者本人拥有的文件，并由 systemd 限制了能力集。同步、下载和界面都以普通用户身份运行。
</details>

<details>
<summary><b>如何卸载</b></summary>

```bash
sudo apt remove gonedrive
```

`~/OneDrive` 里的文件会保留，但**占位文件在卸载后无法再读出内容**（云端的数据不受影响）。如果想在卸载后继续离线使用某些文件，卸载前先对它们执行“始终保留在此设备”，或运行 `gonedrived mode full` 下载全部内容。
</details>

---

## 文档

- [构建与开发](docs/BUILDING.zh-CN.md)：从源码编译、注册 Entra 应用、测试、发布流程
- [设计说明](docs/DESIGN.md)：同步引擎和按需下载的实现、已知的设计取舍、诊断日志

## 许可

[GPL-3.0-or-later](LICENSE)
