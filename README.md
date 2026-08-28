# GOnedrive

Linux 上的 OneDrive 客户端，目标是对标 Windows 官方客户端的两项核心能力：**文件夹双向同步**与**文件按需下载**。GTK4 + libadwaita 界面，Rust 实现。

当前状态：**M1 + M2 核心已完成** —— OAuth 登录、双向同步、冲突保留、GTK 界面，以及**按需下载**（占位文件、自动回填、释放空间）均已在真实账户上跑通。

---

## 它和现有方案的区别

| | 同步目录是真实目录 | 按需下载 | 图形界面 |
|---|---|---|---|
| `abraunegg/onedrive` | ✅ | ❌ | ❌ |
| `onedriver` / `rclone mount` | ❌（FUSE 挂载点） | ✅ | ❌ |
| **GOnedrive** | ✅ | ✅ | ✅ |

按需下载走 Linux 6.14 引入的 **fanotify 预读事件**（`FAN_CLASS_PRE_CONTENT` / `FAN_PRE_ACCESS`），这是 Linux 上唯一对标 Windows CFAPI 占位文件语义的机制：同步目录是真实的 ext4 目录，占位文件是真实的稀疏文件，任意程序 `open()` + `read()` 时由内核拦截并触发回填。详见 `M2 路线`。

---

## 构建

```bash
# 依赖（Ubuntu / Debian）
sudo apt install libgtk-4-dev libadwaita-1-dev pkg-config build-essential

# Rust 工具链
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

cargo build --release
```

有了 client_id（见下一节）之后，用这条命令构建可分发的二进制：

```bash
GONEDRIVE_CLIENT_ID=<你的 id> cargo build --release
```

SQLite 与 TLS 无需系统开发包：SQLite 由 `rusqlite` 静态编译，TLS 走 rustls。

产物：

| 二进制 | 作用 |
|---|---|
| `target/release/gonedrived` | 后台同步守护进程（普通用户权限） |
| `target/release/gonedrive` | GTK4 前端（纯 D-Bus 客户端，不持有凭据） |

---

## 注册 Entra ID 应用（一次性，约 3 分钟）

**先说清楚：`client_id` 不是你的账户。** 登录时你仍然是在微软官方页面用自己的 Microsoft 账户输密码。`client_id` 只是"哪个程序在请求访问"的标识 —— 它不是机密，微软的 OAuth 也不存在免注册路径（ROPC 对个人账户和开了 MFA 的账户都是禁用的）。

**这是构建者做一次的事，不是每个用户都要做的事。** 和 abraunegg/onedrive、rclone、onedriver 的做法一致：注册一个应用，把 id 烤进二进制，之后所有用户开箱即用；需要独立 API 配额的人再自行覆盖。

### 优先级

| 来源 | 用途 |
|---|---|
| 环境变量 `GONEDRIVE_CLIENT_ID` | 覆盖一切，开发和临时测试用 |
| 配置文件里的 `client_id` | 用户级覆盖（比如共享注册被限流了，想用自己的配额） |
| 构建期烤入 | **推荐的分发方式**，见下 |

三者都没有时，守护进程会拒绝启动并打印完整指引，而不是稍后在微软那边报一个什么都没说明的 `AADSTS700016`。

### 注册步骤

前提：你需要一个 Entra 租户。**个人 outlook.com 账户默认没有** —— 有工作/学校的 Microsoft 365 账户就直接用它登录（注册出来的应用照样能登录个人 OneDrive）；没有的话在 [azure.microsoft.com/free](https://azure.microsoft.com/free) 注册免费账户，会自动创建一个 Default Directory。

1. 打开 [Entra ID 门户 → 应用注册 → 新注册](https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade)
2. **名称**随便填，比如 `GOnedrive`
3. **支持的账户类型**选 **“任何组织目录中的帐户和个人 Microsoft 帐户”**
   （少了"个人"这半句，outlook.com 账户登录会被拒）
4. **重定向 URI**：左侧下拉选 **“公共客户端/本机(移动和桌面)”**，右侧填 **`http://localhost`**
5. 创建后在概述页复制 **“应用程序(客户端) ID”**

> **`http://localhost` 必须一字不差，不要加路径、不要加端口。**
> 微软匹配 `localhost` 重定向 URI 时会忽略端口（这正是本客户端能用随机端口的原因），
> 但**路径是精确匹配的**。填成 `http://localhost/callback` 而客户端请求 `http://localhost:1234`，
> 会得到 `AADSTS50011: reply URL does not match`。
> 这也正是微软官方对"使用系统浏览器的桌面应用"的推荐值。
>
> 文档建议优先用 `127.0.0.1` 而非 `localhost`，但门户 UI 拒绝添加 `http` + `127.0.0.1` 的组合
> （需手改应用清单 JSON），且端口豁免规则是针对 `localhost` 写的。一次点击胜过一次改清单。

### 用起来

```bash
# 分发方式：烤进二进制，用户不必再管
GONEDRIVE_CLIENT_ID=<你的 id> cargo build --release

# 或临时试一下
export GONEDRIVE_CLIENT_ID=<你的 id>
```

之后 `gonedrived login` 打开浏览器，用**你自己的 Microsoft 账户**正常登录并同意授权即可。refresh token 存进系统密钥环，不落数据库文件。

---

## 使用

```bash
gonedrived login     # 打开浏览器登录，凭据存入系统密钥环
gonedrived run       # 持续同步
gonedrive            # 打开图形界面
```

其他命令：

```bash
gonedrived status    # 查看账户、配额、待处理队列
gonedrived sync      # 单次同步后退出
gonedrived logout    # 移除凭据（本地文件保留）
```

安装（含开机自启与按需下载 helper）：

```bash
sudo ./install.sh     # 二进制 + 特权 helper 系统服务（唯一需要 root 的一半）
./install.sh --user   # 守护进程用户服务 + Nautilus 集成 + 图标
```

---

## 设计要点

四个 crate，进程边界是刻意的：UI 崩溃不能中断传输，同步逻辑不能跑在 root 下。

```
crates/gonedrive-core/   同步引擎、Graph 客户端、SQLite 状态库（无 UI、无特权）
crates/gonedrive-ipc/    D-Bus 契约（前端不链接同步引擎）
crates/gonedrived/       守护进程：轮询、inotify、传输队列、D-Bus 服务
crates/gonedrive-gtk/    GTK4 前端：纯 D-Bus 客户端
```

几个值得单独说明的取舍：

**路径是推导出来的，不是存下来的。** 数据库里每一项只记 parent 和 name，完整路径由递归 CTE 得到。重命名一个有五万个后代的文件夹因此是一条 UPDATE，而不是五万条。

**名字按大小写折叠建索引。** OneDrive 认为 `Report.docx` 和 `report.docx` 是同一项，ext4 不这么认为。没有大小写不敏感的唯一约束，这两者会无声地互相覆盖到天荒地老 —— 所以冲突在插入时就由数据库拦下。

**下载先落临时文件再原子重命名。** 临时文件刻意放在同目录（同一文件系统），重命名才是原子的。崩溃或断线绝不会在用户期待文档的位置留下一个被截断的文件。短读也会被拒绝：连接提前干净结束，看起来和成功同步一个损坏文件一模一样。

**上传一律带 `conflictBehavior: fail`。** 冲突由我们判定，绝不让服务端静默覆盖用户没见过的版本。两端都改时保留双份：本地版本改名为 `文件名 (主机名 的冲突副本 时间戳).ext` 并上传，远端版本落回原名。

**远端删除不会消灭未保存的本地修改。** 如果一项在远端被删、但本地在上次同步后被改过，本地内容会保留并作为新文件重新上传。用户可能并不打算把那个删除应用到自己更新的内容上。

**deltaLink 原样存储。** 服务端明确说它是不透明值；从中抠出 token 自己拼 URL 是 410 的常见来源。收到 410 `resyncRequired` 时走完整重枚举 + 代际扫除 —— 因为重枚举只报告"存在什么"，从不报告"什么被删了"。

**待下载的项不会被误判为本地删除。** 一个已记录但还没写到磁盘的项，看起来和用户删掉的文件一模一样。把它删到远端会摧毁用户从未碰过的数据，所以只有确认曾经落过盘（有 inode 记录）的项才会触发远端删除。

**OneDrive 拒绝的文件名会被跳过并上报，绝不自动改名。** 背着用户重命名他们的文件，比拒绝同步更糟。

---

## 测试

```bash
cargo test --workspace
```

86 项测试，其中 13 项是端到端集成测试：用 `wiremock` 起一个假 Graph，驱动真实的引擎、真实的数据库、真实的文件系统，覆盖首次全量同步、增量、远端改动、远端删除、本地新增、本地删除、目录创建、双端冲突、410 重同步、429 限流退避、非法文件名，以及"待下载不能被误判为本地删除"这条守卫。

单元测试覆盖冲突判定矩阵的每一格、delta 拓扑排序、路径推导、大小写冲突、分片边界（4 MiB 阈值、320 KiB 倍数），以及 OAuth loopback 回调的 state 校验和错误透传。

---

## 按需下载

同步目录是**真实的 ext4 目录**，占位文件是**真实的稀疏文件**：`ls` 显示真实大小、`du` 显示 0、文件管理器里看得见。任意程序 `open()` + `read()` 时由内核拦截，自动下载后放行 —— 读到的是真实内容，调用方不知道发生过什么。

```bash
gonedrived free-space 文档        # 释放空间，文件留在原地变成占位
gonedrived on-demand 图片         # 为从未下载过的文件建占位
gonedrived storage 文档           # 看每个文件实际占多少
gonedrived fetch 文档/报告.docx    # 手动回填
```

需要 `gonedrive-hsm` 以 root 运行（systemd 系统服务，或 `sudo gonedrive-hsm`）。

### 实测结果

在一个 1 TB / 已用 348.7 GiB 的账户上，`工具/` 文件夹 9 个文件、32.5 MiB：

| | 结果 |
|---|---|
| 释放空间后 | 32.5 MiB 声称，36 KiB 实占 |
| `cat` 一个 206 B 文件 | 自动回填，SHA1 与服务端记录一致 |
| `md5sum` 一个 13.5 MiB 文件 | 5.1 秒完成，SHA1 一致 |
| 回填后二次读 | 0.027 秒（mark 已移除，零开销） |
| 时间戳 | 回填前后都是原始的 2017-09-12 |

### 几个必须做对的地方

**用事件自带的 fd 写，不能用自己打开的 fd。** 这是最容易踩的坑：往稀疏文件的空洞里写会触发页面换入，而**页面换入本身就是一次读**，于是又触发 `FAN_PRE_ACCESS`，无限循环 —— 文件每秒被回填一次，而那个想读它的进程永远阻塞。内核只豁免 fanotify group 持有者的访问，所以写操作必须由特权 helper 通过内核递过来的那个 fd 完成。守护进程负责下载到同目录的临时文件，helper 负责填。

**inode mark 而不是 mount mark。** mount mark 会让整个文件系统的每一次读都过一遍 root 进程 —— 性能不可接受，而且一个 bug 的爆炸半径大得多。回填完成后立刻移除 mark，之后的读零开销。

**事件循环绝不做 IO。** 事件只做分发，回填在 worker 线程里做。

**每个事件必须应答。** 没被应答的 pre-content 事件会让读进程**永久卡死**。失败路径回 `FAN_DENY`，调用方拿到一个正常的 IO 错误。

**xattr 是权威，不是数据库。** 崩在回填中途会让数据库记着 `hydrating` 而磁盘上文件还是空的；信数据库就会漏掉这个文件的拦截，它之后会静默地读出全零。所以以文件自己的 xattr 为准 —— 这也让数据库丢失后能从磁盘恢复。

**helper 不碰网络也不碰 token。** 它只认识路径和字节范围，连数据库都不需要 —— 路径到 item id 的映射存在文件自己的 xattr 里。socket 用 `SO_PEERCRED` 校验对端 uid，且只接受该 uid 拥有的文件。

## 面板与托盘

主窗口即托盘面板，对齐 Windows 客户端的弹出面板，三页：

| 页 | 内容 |
|---|---|
| **概览** | 账户与头像、配额条、同步状态、当前传输进度、立即同步/打开文件夹 |
| **文件夹** | 对齐 Windows 的 "选择文件夹"：逐级浏览远端目录，开关式选择；关闭即释放本机空间，文件仍可随时打开 |
| **活动** | 最近同步历史（下载/上传/删除/冲突/失败 + 相对时间），含按需下载 |
| **设置** | 暂停开关、同步文件夹、退出登录 |

- 关闭窗口 = 隐藏到后台继续同步；托盘图标（StatusNotifierItem）单击唤出面板，右键菜单含暂停/立即同步/打开文件夹/退出。
- 托盘依赖桌面的 StatusNotifierWatcher（GNOME 需 AppIndicator 扩展）。watcher 不在时每 30 秒自动重试注册，不影响其他功能。
- 双击打开未下载的文件时，会弹出小进度窗口显示各文件进度、可取消；由守护进程按需拉起，全部完成后自动消失。
- **部分回填**：文件管理器嗅探文件类型只读头部 16 KB —— 就只从云端取这 16 KB 填入窗口，文件保持占位、监视保留、零 UI 打扰；只有真正读过窗口之外的进程才触发全量下载。"进文件夹"因此不等于"下载文件夹"。老 helper 经 Features 握手自动降级为全量，绝不会错把范围当整文件。
- 后台索引器（tracker/baloo/updatedb 等）的读取被直接拒绝，既不下载也不弹窗；拒绝名单同时存在于守护进程侧（改名单无需 root）与 helper 侧（兜底）。
- helper 升级/重启时守护进程自动重连并重新注册全部占位文件。

调试辅助：`GONEDRIVE_SNAPSHOT=/path.png`（离屏渲染当前窗口后退出）、`GONEDRIVE_PAGE=activity|settings`（预选页）、`GONEDRIVE_NO_TRAY=1`。

## 文件管理器集成

Nautilus 里每个文件带状态角标，右键有 GOnedrive 子菜单 —— 对齐 Windows 资源管理器。

```bash
./install-desktop-integration.sh   # 装到 ~/.local/share，不需要 root
nautilus -q                        # 重启文件管理器
```

| 角标 | 含义 | 对应 Windows |
|---|---|---|
| 蓝色云 | 仅在线，内容不在本机 | Online-only |
| 绿色勾（描边） | 已下载，空间紧张时可被回收 | Available on this device |
| 绿色实心勾 | 始终保留在此设备 | Always keep on this device |
| 蓝色循环箭头 | 正在同步 | Syncing |
| 红色感叹号 | 同步出错 | Error |

右键菜单：**始终保留在此设备** / **释放空间** / **立即下载**，按文件当前状态显示对应项。

两个让它足够快的决定：

**状态从文件自己的 xattr 读，不问守护进程。** 渲染一个上千文件的目录就是上千次判断，`getxattr` 是一次系统调用，而问守护进程是一次往返。副作用是守护进程没运行时角标依然正确。

**动作走 D-Bus 且不等返回。** 释放一个大文件夹要花几秒，在这里阻塞会冻住文件管理器的界面线程。

## 一次审计修掉的五类缺陷

在真实账户联调中审计出并修复（各有回归测试）：

1. **上传空洞**：扫描器把回填中/占位文件判为"本地修改"，会把全零内容上传覆盖云端 → 扫描跳过 + 上传前查 xattr。
2. **分裂脑**：CLI 各命令自建引擎，与守护进程抢库、互相轮换 OAuth refresh token（会随机把用户登出）→ 修改型命令一律优先经 D-Bus 走守护进程。
3. **自触发死锁**：守护进程写自己监视的文件 → 内核挂起写并反问守护进程 → D-Bus 全堵、文件管理器卡死 → 双防御：释放前先摘 watch；helper 按 `SO_PEERCRED` pid 直接放行守护进程自身访问。
4. **崩溃残留**：`hydrating` 状态永不恢复 → 启动时以文件 xattr 为准修复。
5. **同步风暴**：本地扫描自己的 `open()` 被 inotify 当成变更，唤醒下一轮扫描，状态永远在"正在同步/已是最新"间横跳 → 只有真正的写事件才唤醒。

## 自动释放与带宽

- **存储感知**（对齐 Windows Storage Sense）：`auto_free_below_bytes` 非零时，空闲空间低于阈值即按 **最久未访问优先** 把已下载且非"始终保留"的文件转回占位，直至恢复到阈值之上；有未上传改动的文件绝不触碰。没有特权 helper 时该功能整体不启用（自动动作绝不能制造读不回来的占位文件）。默认关闭 —— 归还空间是用户选择的策略。
- **带宽限制**：`download_limit_bytes_per_sec` / `upload_limit_bytes_per_sec`，0 为不限。

## v1.0 范围界定

- **单账户**（引擎层已按多账户结构组织，UI 与 CLI 面向单账户）
- **内核 ≥ 6.14 + CAP_SYS_ADMIN**（fanotify 预读钩子）；FUSE 兜底与 libcloudproviders 侧栏在路线图上
- 托盘依赖桌面的 StatusNotifierWatcher；它缺席时 30 秒周期自愈重试

## 已知的开发环境怪癖

`GONEDRIVE_SNAPSHOT` 离屏渲染对"文件夹"页偶发返回空节点（其余页正常）；页面本身在真实屏幕上工作正常，数据层有 D-Bus 级验证。

## 许可

GPL-3.0-or-later
