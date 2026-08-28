# 数据丢失 / 损坏

> 最高优先级类别。任何会让用户永久失去或损坏数据的路径。核心不变量：**宁可不动，也绝不误删/误覆盖/误清零。**

---

## DL-001 · 同步根被卸载但目录仍在 → 扫描误判全部文件被删 → 整账户远端删除

- **首次发现**：2026-08-28
- **现象**：同步根是网络家目录 / 外置盘 / bind mount 时，若卷被卸载，挂载点目录仍以空目录形式存在。守护进程的本地扫描看到"空树"，把每个顶层项都判为"本地已删除"，逐个 `DeleteRemote`，最终把整个 OneDrive 账户在服务端删空。全程不可逆。
- **根因**：
  - `crates/gonedrive-core/src/sync/engine.rs` `scan_local` 唯一的根级护栏是 `!account.root_local_path.exists()`——空挂载点 `exists()` 为真，检查通过，扫描照常进行。
  - `vanished` 收集所有磁盘缺失的记录，`run_delete_remote`（同文件）无条件逐个删除。
  - 全代码库无 `st_dev`/挂载校验，也无批量删除阈值（grep 确认）。
- **修复**：
  1. **设备号护栏**：`accounts` 新增 `root_dev` 列（schema v4，`db.rs`），首次扫描记录同步根的 `st_dev`；此后每次扫描比对，不一致即 `SyncEvent::SafetyStop` 并**拒绝整个扫描**，绝不删除。见 `scan_local` 顶部的设备号检查块与 `Store::set_root_dev`。
  2. **批量删除安全阀**：一次扫描若判定删除数 `> config.mass_delete_min`（默认 10）**且** 占全账户比例 `> config.mass_delete_fraction`（默认 0.5），则**扣留全部删除**、发 `SafetyStop`、计入 `summary.refused_deletions`，交用户确认。见 `scan_local` 删除入队前的安全阀块。
  3. 新增 `DaemonEvent::SafetyStop` → D-Bus `safety_stop` 信号（`app.rs`/`service.rs`/`main.rs`），让 UI/CLI 能提示。
- **回归测试**（`crates/gonedrive-core/tests/sync_integration.rs`）：
  - `a_scan_that_would_wipe_the_account_holds_the_deletions` —— 12 个文件一次性全部消失，断言 `deleted_remote == 0`、`refused_deletions >= 12`、每条记录仍在。
  - `a_sync_root_on_a_different_device_refuses_to_scan` —— 篡改 `root_dev` 模拟换盘，断言 `deleted_remote == 0`、记录仍在。
- **备注**：批量删除阀目前是"扣留 + 发信号"，尚未接完整的"用户确认后执行"UI 流程（Phase 3 补）。安全优先：宁可暂不传播一次真实的大删除，也不误删。阈值可在 `config.toml` 调。

---

## DL-004 · 保留 size+mtime 的原地编辑被 dehydrate 打洞清零（含 xattr/DB 失同步放大）

- **首次发现**：2026-08-28
- **现象**：文件被原地编辑但保持了相同长度、且 mtime 被还原（保留 mtime 的编辑器、`rsync --times`、等长重写）时，`local_is_modified` 的 size+mtime 判据看不出改动，dehydrate/释放空间会把这些**从未上传的字节**打成空洞，永久丢失。xattr/DB 失同步（DB 标 Dehydrated 但磁盘实为满内容）会经 `state==Dehydrated` 跳过护栏的捷径进一步放大。
- **根因**：`crates/gonedrive-core/src/sync/engine.rs::dehydrate` 的护栏仅用 `plan::local_is_modified`（`plan.rs` 的 size+mtime 启发式），且对 `state==Dehydrated` 直接跳过。
- **修复（哈希级）**：
  1. 新增 `crates/gonedrive-core/src/hash.rs`：Microsoft quickXorHash 流式实现，已对 25 个真实服务端哈希逐字节验证（25/25）。
  2. 写内容时记录哈希 xattr `user.gonedrive.quickxor`：`Placeholder::mark_hydrated`（覆盖全部 hydration 完成点）与上传成功后（`run_upload`）都按**实际磁盘字节**计算并写入。
  3. dehydrate 护栏改为 `Engine::local_content_unsynced`：满分配文件必须"当前内容哈希 == 记录哈希"才放行；稀疏占位符（含 range 缓存）直接放行；无记录哈希才回退 size+mtime。**不再信任 DB 状态**，故同时堵住 xattr/DB 失同步（原 DL-006/ES-002 的 dehydrate 面）。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::freeing_space_refuses_an_in_place_edit_that_kept_size_and_mtime`（已验证：无哈希门禁时 RED，有则 GREEN）；`hash.rs` 三个单测。
- **待续**：下载覆盖路径（`plan.rs:181` `Download{overwrite:true}`）与 `materialize_all` 空洞重建（DL-003）复用同一 `local_content_unsynced` 门禁，随后提交。

<!-- 后续：DL-003 materialize 清零、DL-005 If-Match 丢失更新、DL-007 冲突副本碰撞。 -->
