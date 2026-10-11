# 设计说明

> This document is currently available in Chinese only. For an English overview see the [README](../README.md); for building, see [docs/BUILDING.md](BUILDING.md).

GOnedrive 的实现取舍、按需下载的内核机制、已知边界和诊断方法。使用说明见 [README](../README.zh-CN.md)。

## 按需模式的默认行为

新配置默认启用按需文件：首次登录先持久化 Graph 的 `token=latest`
变化基线，再列出根目录的直属项并为直属子目录预取一层占位项；第一次打开一个
云端文件夹时同样只向下预取一层。不会为了显示根目录先读取六位数的整盘历史元数据。
配置文件中明确设置
`on_demand = false` 可改用传统全量元数据同步。

Linux 的桌面索引器不了解云占位目录，递归枚举会被误认为逐层浏览。默认配置因此在
watcher 和首次 Graph 请求之前创建 `.trackerignore`；可用
`gonedrived indexer --include` 显式恢复桌面索引，这个选择会写回配置并跨重启保留。

按需下载走 Linux 6.14 引入的 **fanotify 预读事件**（`FAN_CLASS_PRE_CONTENT` / `FAN_PRE_ACCESS`），这是 Linux 上唯一对标 Windows CFAPI 占位文件语义的机制：同步目录是真实的 ext4 目录，占位文件是真实的稀疏文件，任意程序 `open()` + `read()` 时由内核拦截并触发回填。

## 设计要点

五个 crate，进程边界是刻意的：UI 崩溃不能中断传输，同步逻辑不能跑在 root 下。

```
crates/gonedrive-core/   同步引擎、Graph 客户端、SQLite 状态库（无 UI、无特权）
crates/gonedrive-ipc/    D-Bus 契约（前端不链接同步引擎）
crates/gonedrived/       守护进程：轮询、inotify、传输队列、D-Bus 服务
crates/gonedrive-gtk/    GTK4 前端：纯 D-Bus 客户端
crates/gonedrive-hsm/    按需下载 helper：fanotify 预读事件（唯一以 root 运行的部分）
```

几个值得单独说明的取舍：

**路径是推导出来的，不是存下来的。** 数据库里每一项只记 parent 和 name，完整路径由递归 CTE 得到。重命名一个有五万个后代的文件夹因此是一条 UPDATE，而不是五万条。

**名字按大小写折叠建索引。** OneDrive 认为 `Report.docx` 和 `report.docx` 是同一项，ext4 不这么认为。没有大小写不敏感的唯一约束，这两者会无声地互相覆盖到天荒地老 —— 所以冲突在插入时就由数据库拦下。

**下载先落临时文件再原子重命名。** 临时文件刻意放在同目录（同一文件系统），重命名才是原子的。崩溃或断线绝不会在用户期待文档的位置留下一个被截断的文件。短读也会被拒绝：连接提前干净结束，看起来和成功同步一个损坏文件一模一样。

**上传一律带 `conflictBehavior: fail`。** 冲突由我们判定，绝不让服务端静默覆盖用户没见过的版本。两端都改时保留双份：本地版本改名为 `文件名 (主机名 的冲突副本 时间戳).ext` 并上传，远端版本落回原名。

**远端删除不会消灭未保存的本地修改。** 如果一项在远端被删、但本地在上次同步后被改过，本地内容会保留并作为新文件重新上传。用户可能并不打算把那个删除应用到自己更新的内容上。

**deltaLink 原样存储。** 服务端明确说它是不透明值；从中抠出 token 自己拼 URL 是 410 的常见来源。收到 410 `resyncRequired` 时走完整重枚举 + 代际扫除 —— 因为重枚举只报告"存在什么"，从不报告"什么被删了"。

**目录按 CFAPI 的“未完全填充目录 + FULL 单目录填充”语义工作。** 这里的 FULL 是
“一次返回当前目录的全部直属项”，不是首次遍历整盘；打开目录时会额外预取直属子目录的
一层占位项，但更深层仍为 Partial，等它自己被打开时才填充。新账户先取得并落盘
`delta?token=latest`，然后用 `/children` 填根目录并预取一层，绝不递归到叶子。children
分页不是全盘快照，所以游标必须先落盘；children 与 delta 的本地提交也严格排序为“目录
快照在前、后续变化在后”，慢响应不能把新版本回滚。
单项无法表示时保存原始 payload 延期重放，相关路径冻结，但全局 delta 游标继续推进。
只有旧数据恢复或 410 游标失效才进行完整重枚举。GOnedrive 对齐的是 Microsoft 公开的
CFAPI/Graph 行为契约，不声称复刻 OneDrive 客户端未公开的内部实现。

Linux 没有 CFAPI 的 `FETCH_PLACEHOLDERS` 回调。GOnedrive 用 HSM 的 process-aware
fanotify permission event 映射这个边界：每个**已填充父目录**只有一个直属子目录 mark，
不因它包含十万个 Partial 子目录而注册十万次；前台程序打开某个 Partial 子目录时，该次
open 等待其直属项全部落盘后再继续。完成填充的子目录随后成为新的单一边界。daemon 自身
和后台索引器的访问直接放行且不触网，普通 inotify 只负责本地写入/删除/重命名，绝不参与
目录填充判定。

**待下载的项不会被误判为本地删除。** 一个已记录但还没写到磁盘的项，看起来和用户删掉的文件一模一样。把它删到远端会摧毁用户从未碰过的数据，所以只有确认曾经落过盘（有 inode 记录）的项才会触发远端删除。

**OneDrive 拒绝的文件名会被跳过并上报，绝不自动改名。** 背着用户重命名他们的文件，比拒绝同步更糟。

## 按需下载的实现

同步目录是**真实的 ext4 目录**，占位文件是**真实的稀疏文件**：`ls` 显示真实大小、`du` 显示 0、文件管理器里看得见。任意程序 `open()` + `read()` 时由内核拦截，自动下载后放行 —— 读到的是真实内容，调用方不知道发生过什么。

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

## 面板与 Nautilus 的读取策略

- 双击打开未下载的文件时，会弹出小进度窗口显示各文件进度、可取消；由守护进程按需拉起，全部完成后自动消失。
- **部分回填**：规则只有一句 —— **看，不改变任何东西；用，才下载**。文件管理器（按进程名识别）的任何读取都按范围喂给它、永不翻转文件状态，与文件大小无关；已在盘上的范围直接本地供给，重复浏览零流量；浏览留下的缓存随时可被"释放空间"整体冲掉。真实使用者（编辑器、`cat`、复制）读小文件或读出嗅探窗之外时才全量下载并翻转状态。老 helper 经 Features 握手自动降级为全量，绝不会错把范围当整文件。
- 后台索引器（tracker/baloo/updatedb 等）的文件读取被直接拒绝，既不下载也不弹窗；GNOME `localsearch` 还会递归打开目录，因此同步根默认带 `.trackerignore`，从源头避免它触发目录填充和无效 Graph 请求。拒绝名单同时存在于守护进程侧（改名单无需 root）与 helper 侧（兜底）。
- helper 升级/重启时守护进程自动重连，并重建占位文件 mark 与已浏览目录的 population boundary。

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

### 稀疏占位符与整体复制工具

占位符是稀疏文件（空洞），这让它几乎不占磁盘，但也意味着 `cp`/`tar`/`rsync`/`dd`
这类**稀疏感知**工具会用 `SEEK_HOLE` 跳过空洞、直接复制零 —— 它们 open 之后往往
一次 `read` 都不发，`FAN_PRE_ACCESS` 拦不到。为此额外监听 **open 事件**：这些整体
复制工具一打开占位文件就先全量回填，等它 SEEK 时文件已完整，复制到的是真内容。
媒体播放器等流式读取的程序不在此列，仍按读取渐进回填。（Windows 用文件系统过滤器
让占位符对 SEEK 表现为非稀疏；Linux 上 FUSE 兜底后端是等价方案，在路线图上。）

## 自动释放与带宽

- **存储感知**（对齐 Windows Storage Sense）：`auto_free_below_bytes` 非零时，空闲空间低于阈值即按 **最久未访问优先** 把已下载且非"始终保留"的文件转回占位，直至恢复到阈值之上；有未上传改动的文件绝不触碰。没有特权 helper 时该功能整体不启用（自动动作绝不能制造读不回来的占位文件）。默认关闭 —— 归还空间是用户选择的策略。
- **带宽限制**：`download_limit_bytes_per_sec` / `upload_limit_bytes_per_sec`，0 为不限。

## v1.0 范围界定

- **单账户**（引擎层已按多账户结构组织，UI 与 CLI 面向单账户）
- **内核 ≥ 6.14 + CAP_SYS_ADMIN**（fanotify 预读钩子）；FUSE 兜底与 libcloudproviders 侧栏在路线图上
- 托盘依赖桌面的 StatusNotifierWatcher；它缺席时 30 秒周期自愈重试

### 已知的设计取舍

下面几条是审查中核实过、但**有意不改**的设计边界。写在这里是为了它们不再被当成新发现。

- **远端删除非空目录**：脱钩不删的策略依赖 Graph 为每个子项都下发 tombstone。若这些
  tombstone 跨轮次分裂到达，整棵目录会被本地扫描当作新增重传回云端。代价是可控的
  （重传而非丢失），改成"目录 tombstone 即递归删除"则把一次误判放大成一次批量删除。
- **`items.id` 是全局主键**：OneDrive 的 item id 只在账户内唯一。同一个驱动器被两个
  账户添加时两边会互相覆写。v1.0 是单账户，多账户要做的时候主键得换成
  `(account_id, id)`。
- **`local_mtime` 只精确到秒**：同一秒内两次同样大小的修改，靠 `stat` 分不出来。
  内容哈希能兜住这一类，但只在已经下载过的文件上有。
- **helper 的会话按 uid 独占**：`SO_PEERCRED` 只验证 uid，所以同一用户的任何进程都能
  抢先占住那唯一的会话席位并且什么都不做，让真正的守护进程连不上。这不构成越权
  （占位的进程本来就能读该用户的全部文件），是同一用户内部的可用性问题。
- **helper 的强制模式不受特性协商门控**：某个 uid 只要发过一次 `RegisterRoots`，此后
  它的所有路径都走强制根限制。行为是对的，只是没有开关。

## 按需开启诊断日志

正式包默认只记录警告和错误，且两个 systemd 服务各自限制为每分钟最多 30 条，避免
范围回填或重复故障持续写满 journal。需要排查时可通过 `RUST_LOG` 临时打开详细日志；
以下命令查看当前服务日志：

```bash
journalctl --user -u gonedrived -f      # 用户服务方式运行时
journalctl -u gonedrive-hsm -f          # 特权 helper（含读者进程名）
```

| 日志行 | 含义 |
|---|---|
| `FreeUpSpace requested path=…` / `… done files=N freed=N` | 谁的哪次释放、动了多少 —— 右键误伤直接对账 |
| `SetPinState requested … / done queued_files=…` | 每次选择/取消选择及其代价 |
| `on-demand read path=… reader=nautilus length=16384` | debug：**哪个进程**在触发按需读取、要了哪个范围 |
| `staged range … bytes=16384` / `hydrated range` | debug：部分回填，只取嗅探窗口，文件仍是占位 |
| `staging content size=…` / `hydrated` | 全量回填 |
| `refusing a background reader` | 索引器/缩略图器被拒，未产生流量 |
| `refusing to upload a placeholder` | 上传守卫拦下了空洞文件 |
| `sync pass finished downloaded=… uploaded=…` | 有实际结果的同步账目；空轮询只在 debug 出现 |
| `repaired a stale hydrating record` | 崩溃残留被自动纠正 |

四起真实事故（误伤兄弟文件、整盘释放、进文件夹触发下载、暂存文件残留）都已固化为
`data/nautilus/tests/test_dispatch.py` —— 用真守护进程、真总线、真文件回放右键链路。该组测试需显式设置 `GONEDRIVE_LIVE_TESTS=1`，并可用 `GONEDRIVE_TEST_ROOT`、`GONEDRIVE_EXTENSION` 和 `GONEDRIVE_TEST_LOG` 指定测试环境；普通测试发现会安全跳过它。
