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
