# 作用域与破坏性动作护栏

> 右键/一键动作的作用范围，越根路径，破坏性操作的确认。核心：**破坏性动作的作用域必须与用户所指一致，且永不越出同步根。**

---

## SS-001 · 释放/固定的作用域波及兄弟项乃至整盘

- **首次发现**：2026-08-27
- **现象**：在"工具/软件"右键释放，结果把 `ss.txt`、甚至整个 OneDrive 都释放；右键释放的实际范围像是整个同步根。
- **根因**：root 作用域的 `FreeUpSpace`/`SetPinState` 未被拦截；上传路径误把文件标 `pinned` 导致固定/释放判定错乱。
- **修复**：拒绝 root 作用域的一键动作并引导到面板文件夹页（`crates/gonedrived/src/service.rs` 的 `relative()` 护栏 + `AccessDenied`）；schema v3 迁移清洗被误设的 `pinned`。会话端到端复验：释放 `桌面备忘` 只释放其内文件，兄弟 `工具` 不受影响。
- **回归测试**：`data/nautilus/tests/test_dispatch.py`（单文件夹菜单只释放自身、整盘根被端到端拒绝）。
- **提交**：`Refuse drive-wide one-click actions; sweep orphaned staging files`。

## SS-002 · 越根路径与整盘 dehydrate 未被护栏拦截（测试缺口）

- **首次发现**：2026-08-28（探索发现，测试缺口）
- **现象**：`service.rs::relative()` 拒绝越出同步根的路径、拒绝整盘 dehydrate，但无测试守护，回归风险高。
- **根因**：安全护栏存在但零测试。
- **修复**：将 `relative()` 的纯逻辑抽为 `confine_to_root`（`crates/gonedrived/src/service.rs`）并补单测——越根路径（含共享前缀的兄弟目录 `OneDrive-evil`、无关绝对路径、父目录）判 `Outside`；同步根自身（含结尾斜杠）判 `WholeRoot`。
- **回归测试**：`crates/gonedrived/src/service.rs::a_path_outside_the_root_is_refused`、`::the_sync_root_itself_is_refused_so_one_click_cannot_sweep_the_drive`、`::a_path_inside_the_root_is_confined_to_a_relative_one`。

<!-- 关联 data-loss 的 DL-001 批量删除阀也是作用域安全的一部分，主条目见 data-loss.md。 -->

## SS-003 · 批量删除确认被无关的扫描消耗 → 用户确认后删除仍被拒

- **首次发现**：2026-09-15（代码审计）
- **现象**：批量删除阀触发后用户点击确认，但删除仍然被拒绝、再次弹出安全停止。
- **根因**：`crates/gonedrive-core/src/sync/local.rs` 在 `if !deletes.is_empty()` 之前无条件 `mass_delete_approval.swap(false)`。确认之后若先来一次没有删除的 watcher 局部扫描（任何无关的写入），一次性确认就被它悄悄吃掉。
- **修复**：只有真正携带删除的扫描才消耗确认标志。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_mass_delete_approval_survives_an_unrelated_scan`（已验证：无修复时 RED，有修复 GREEN）。

## SS-004 · root helper 不知道同步根 → 任何本地用户可让 root 进程 mark 自己拥有的任意文件

- **首次发现**：2026-09-15（代码审计）
- **现象**：helper 的套接字为 0666，授权边界仅为"目标 inode 的 owner uid == 对端 uid"，不限制在同步根之下。任意本地用户因此获得本需 CAP_SYS_ADMIN 的内核 pre-content 钩子，可挂在自己拥有的任何文件上（`~/.ssh/authorized_keys` 等），阻塞 root 进程对它们的读取最长 300 秒；FUSE 上的文件还能让 root worker 无限期阻塞。
- **根因**：`crates/gonedrive-hsm/src/marks.rs::open_authorised_kind` 用 `AT_FDCWD` 打开绝对路径，没有任何根目录概念。
- **修复**：新增协议 `RegisterRoots`（`Features.sync_roots`）。守护进程连接后先声明同步根；helper 校验根为对端拥有的真实目录且不在 FUSE/网络文件系统上，此后所有 mark 与暂存文件都相对根描述符以 `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV` 解析，未声明根的会话什么都不能 mark。同时补上 `O_NOCTTY`（对端拥有的 pts 曾可成为 helper 的控制终端，关闭 master 即 SIGHUP 杀死 helper）并将 unit 改为 `Restart=always`。
- **回归测试**：`crates/gonedrive-hsm/src/tests.rs::a_file_outside_every_registered_root_is_refused`、`::a_root_prefix_is_matched_by_whole_component`、`::a_root_must_be_a_directory_the_peer_owns_reached_without_symlinks`、`::helper_unit_restarts_after_any_exit`。
- **兼容性**：新 helper 会拒绝旧守护进程的所有 mark（它不会声明根）；两者随包一起升级，deb 的 postinst 同时重启两个服务。

## SS-005 · 登录完成时用陈旧的配置快照覆盖 config.toml

- **首次发现**：2026-09-15（代码审计）
- **现象**：浏览器登录窗口期间切换了同步模式，登录一完成，磁盘上的模式被还原成登录开始前的值；下次重启回到旧模式。
- **根因**：`service.rs::start_login` 在 spawn 前取得 `app.config()` 快照并持有最长 300 秒，`login.rs::complete` 结束时把这个快照 `save()` 回磁盘。
- **修复**：`complete` 不再写配置；调用方在登录完成后用**当时的**实时配置保存。

## SS-006 · 向守护进程写回复没有超时 → 少数挂起的对端可永久耗尽 root worker 池

- **首次发现**：2026-09-15（第三轮审计）
- **现象**：守护进程被挂起、暂停或异常退出而未关闭连接时，特权 helper 的处理能力逐渐归零，占位符读取全部超时，只能重启 helper 恢复。
- **根因**：`crates/gonedrive-hsm/src/server.rs::send_to_stream` 的 `write_all` 未设 `SO_SNDTIMEO`，而它发生在 `recv_timeout(HYDRATE_TIMEOUT)` **之前**，那个 300 秒超时完全保护不到它。worker 池只有 64 个固定线程。
- **修复**：accept 后给 socket 设 30 秒写超时（回复只有一行，健康的守护进程永远不会触发）。写失败时主动关闭连接——超时可能已经写出半行 JSON，对端继续读会与之后所有回复失去同步；关闭后由会话守卫把待处理事件全部拒绝（fail-closed），守护进程重连。
- **回归测试**：`crates/gonedrive-hsm/src/tests.rs::a_reply_to_a_peer_that_stopped_reading_gives_up_and_ends_the_connection`。

## SS-007 · 拒绝文件系统清单遗漏 autofs 与 overlayfs

- **首次发现**：2026-09-15（第三轮审计）
- **根因**：`marks.rs::REFUSED_FILESYSTEMS` 覆盖了 fuse、nfs、smb/cifs、9p、afs、ceph 等，但缺 autofs（`0x0187`）与 overlayfs（`0x794c7630`）。autofs 挂载点被访问时可能触发按需挂载而长时间阻塞，overlay 的下层也可能由用户控制且缓慢——两者都能把一个 root worker 按住任意久，正是该清单要堵的那类"一个本地用户拖垮所有人"的路径。与 SS-006 叠加时后果更重。
- **修复**：补入两项。

## SS-008 · 账户生命周期：移除账户后引擎继续运行，令牌被重新写回钥匙环

- **首次发现**：2026-09-15（第三轮审计）
- **现象**：有传输在进行时退出登录，日志涌出"unknown account"与外键错误、状态变 `error`；更糟的是刚删掉的钥匙环条目会被重新创建，账户像是没退干净。重复登录同一账户则出现两个引擎扫描同一个根，两个 `Authenticator` 轮流轮换同一个 refresh token 而互相作废。
- **根因**：`detach_account` 只把引擎从 map 里摘掉。协调器与回填路径持有 `Arc<Engine>` 克隆并跨 await 使用，被摘掉的引擎照常跑完当前的下载、扫描与 delta；其 `Authenticator` 在下一次 Graph 调用时刷新令牌，而刷新成功就会 `save`，重建刚被删除的条目。`attach_account` 只拒绝**不同** id，同 id 直接 `insert` 覆盖，旧引擎并不停止。CLI 的 `logout` 完全绕过守护进程，同样制造分裂脑。
- **修复**：新增统一的停止原语。`Engine::stop()` 置位并连带停止 `GraphClient`；所有操作都经过 `Engine::account()`，在那里一处检查即可让在途工作在下一个检查点以 `Cancelled` 退出；网络侧在 `send()` 与分片上传循环各检查一次，令牌刷新因此不再发生（分片上传走预授权会话 URL，本就不刷新）。`detach_account` 先停再摘，并清掉该根下的在途回填登记。`attach_account` 对同 id 改为"先退休旧引擎再装新的"。`report_core_error` 对 `Cancelled` 不再置错误状态——那是我们自己要求的停止。CLI `logout` 在守护进程运行时改走 D-Bus。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_stopped_engine_neither_writes_nor_reaches_the_service`（断言得到的是 `Cancelled` 而非 mock 服务器的 404，证明请求根本没有发出）。

## SS-009 · 磁盘不足时上传与删除被一并饿死

- **首次发现**：2026-09-15（第三轮审计）
- **现象**：可用空间低于下限时，不仅下载停了，上传和删除也停了——而后两者根本不需要磁盘空间。
- **根因**：`queue.rs::drain_queue` 的查询把下载排在批次前面。空间闸门把整批下载滤掉之后，`ops.is_empty()` 直接 `return`，本轮再也不取下一批。注释声称"上传和删除照常"，与实现不符。
- **修复**：整批被扣下时改为 `continue` 取下一批（被推迟的行已经不再到期，循环必然收敛）。同时把"扣下"与"失败"分开：新增 `Store::defer_op` 只推后 `next_attempt_at` 而不增加 attempts——操作根本没跑过，计入重试预算会把退避迅速推到 256 秒上限，也会污染真实失败的计数。
- **回归测试**：`crates/gonedrive-core/src/db/tests.rs::deferring_an_operation_does_not_spend_its_retry_budget`（空间闸门本身依赖真实 statvfs，未做端到端测试）。

## SS-010 · 特权 helper 的 worker 池不按 uid 分配，且日志可被文件名伪造

- **首次发现**：2026-09-15（第三轮审计）
- **现象**：两个独立问题。其一，64 个 worker 在所有已连接 uid 之间共享；一个被阻塞的读者最长占用 300 秒，因此同一台机器上的某个用户同时打开足够多的占位文件即可吃光整个池子，其他用户的正常回填全部排在后面。其二，路径来自对端的文件系统，可以包含换行与 ANSI 转义序列，而消息里一律用 `Display` 原样输出——精心构造的文件名可以在 journal 里伪造日志行，或在有人用终端查看时注入转义序列。
- **修复**：worker 配额按 uid 计，上限为池子的四分之三——单用户几乎不受影响，同时始终为其他人留出下限；超额的事件直接拒绝（与队列已满时的行为一致，读者收到 I/O 错误而不是无限等待）。记账抽成独立的 `WorkerSlots`，不依赖需要 `CAP_SYS_ADMIN` 的 fanotify 组，因此可测。日志方面新增 `shown()`，用调试形式输出路径（带引号并转义控制字符），helper crate 内 36 处 `.display()` 全部改用它。
- **回归测试**：`crates/gonedrive-hsm/src/tests.rs::one_uid_cannot_take_the_whole_worker_pool`、`::a_crafted_name_cannot_forge_a_log_line`。

## SS-011 · 登录的两条分裂脑：并发 StartLogin 与绕过守护进程的 CLI 登录

- **首次发现**：2026-09-15（第三轮审计）
- **现象**：其一，连续点两次"登录"，第二次完成时界面收到 `LoginFailed`——而那次登录其实是成功的。其二，守护进程在跑的时候执行 `gonedrived login`，两个进程各自持有同一账户的凭据并轮流轮换 refresh token，微软每次刷新都会轮换，于是两边互相作废直到有一方被登出。
- **根因**：`service.rs::start_login` 没有任何并发保护，第二次登录完成后 `attach_account` 以"账户已激活"失败，而这个错误被当作登录失败上报。`commands/account.rs::login` 则完全在进程内完成整个流程，与 `commands.rs` 顶部关于分裂脑的说明相矛盾（`logout` 已于 SS-008 改走 D-Bus，`login` 当时没有一起改）。
- **修复**：新增 `App::login_in_flight` 标志，`start_login` 发现已有登录在等浏览器时直接拒绝；标志由 `LoginGuard` 在流程结束的任意路径上清除（包括 `Login::start` 自身失败的提前返回）。CLI `login` 在守护进程运行时改为调用 D-Bus `StartLogin` 并等待 `LoginCompleted`/`LoginFailed` 信号——订阅先于请求，避免快速完成的登录落在监听之前。
- **回归测试**：无。两条都需要一个真实的 `App`（其构造会连接 Secret Service）和一条会话总线；与 HP-004 同因，见该条目的说明。

## SS-012 · 自查发现的本轮回归（第二批：运维与可用性）

- **首次发现**：2026-09-15（对本轮全部改动做对抗性自查）
- 六条，均为本次会话自己引入的：
  1. **升级顺序会把整棵占位树锁成 000**。deb 的 postinst 先重启特权 helper 再重启用户守护进程。新 helper 要求对端声明同步根，而旧守护进程不会声明，于是它的每一个 mark 都被拒绝，失败路径把磁盘上**所有**占位文件 chmod 000 直到守护进程也升级完。反方向是安全的（新守护进程 + 旧 helper 只是不受限运行），因此把用户守护进程的重启移到 helper 之前即可消除这个窗口。
  2. **重新认证失败会让账户彻底没有引擎**。`attach_account` 先 detach 旧引擎再去恢复凭据，凭据慢或失败时旧引擎已经停掉。改为先建好替换引擎，成功之后再退休旧的。
  3. **钥匙环被锁会反复弹解锁框**。上一轮把凭据加载超时从 `KeyringLocked` 改成瞬时错误，以免慢网络被报成"钥匙环锁定"；但瞬时错误会在每个轮询周期重试，于是"弹框没人理"的情形每隔几分钟就再弹一次——正是协调器注释明令禁止的。改为分开计时：钥匙环单独 10 秒预算，超时报 `KeyringLocked`（只有显式唤醒才重试）；其后的网络部分仍算瞬时。
  4. **退休账户不回卷在途回填**。`forget_hydrations_for` 只丢账本，不删暂存文件也不把 `Hydrating` 行改回 `Dehydrated`，而本地扫描会跳过该状态。改为在停止引擎**之前**完成回卷（停掉的引擎拒绝一切操作）。
  5. **`gonedrived sync` 仍会空转**。协调器在"剩余操作全在退避"时就完成显式同步，但 CLI 的等待循环仍要求 `pending == 0`；磁盘满时它会在打印结果后继续自旋。改为把守护进程报告的待办数记下，空闲且不超过该数时正常退出。
  6. **`gonedrived login` 可能永远挂住**。等待登录信号没有超时，而信号流绑在 CLI 自己的总线连接上，守护进程退出并不会结束它。加了 360 秒上界（守护进程自身的登录超时是 300 秒）。
- **回归测试**：无。六条都需要真实的 `App`（其构造连接 Secret Service）、会话总线或 dpkg 环境；这正是报告中"把回填编排与 `App` 解耦"那一项要解决的前置问题。
