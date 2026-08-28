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

<!-- 后续 data-loss 类修复（DL-002 full_resync 清扫、DL-003 materialize 清零、DL-004 mtime 保留原地编辑、DL-005 If-Match 丢失更新、DL-006 xattr/DB 失同步、DL-007 冲突副本碰撞）依次追加于此。 -->
