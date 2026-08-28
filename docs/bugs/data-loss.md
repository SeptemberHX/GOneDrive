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

## DL-003 · materialize 空洞重建把用户的稀疏文件清零

- **首次发现**：2026-08-28
- **现象**：`materialize_all` 对 xattr=Hydrated 且 `blocks*512*4 < len`（分配 <25% 视为空洞）的文件直接 `Placeholder::create` 截断清零。真实稀疏文件（VM 镜像、预分配数据库、稀疏日志）满足此条件，会被误清零，且从未上传。
- **根因**：`crates/gonedrive-core/src/sync/engine.rs::materialize_all` 的空洞重建分支仅凭稀疏度判据，无任何上传安全检查（不看 ItemState、不比哈希）。
- **修复**：重建前增加 `safe_to_rebuild` 门禁——仅当"记录的内容哈希 xattr 存在**且**与当前文件哈希相等"（即确为我们写入的、已在服务端的字节）才重建；否则（哈希不符=用户编辑过，或无记录哈希）保持原样，绝不清零。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::materialize_never_zeroes_a_sparse_file_the_user_edited`（已验证：无门禁 RED，有门禁 GREEN）。

## DL-004b · 下载覆盖前未查内容 → 保留 mtime 的原地编辑被远端静默覆盖

- **首次发现**：2026-08-28
- **现象**：远端变更 + 本地保留 size/mtime 的原地编辑同时发生时，`decide_remote` 判远端为新方（`plan.rs:181` `Download{overwrite:true}`），`run_download` 直接覆盖本地，丢失未上传的编辑，且不生成冲突副本。
- **根因**：`run_download` 覆盖前不查本地内容。
- **修复**：`run_download` 覆盖前调用 `local_content_unsynced`（同 DL-004 哈希门禁）；若本地有未上传编辑，先 `make_conflict_copy` 保留并重传，远端再落到原名。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_download_preserves_a_hidden_local_edit_as_a_conflict_copy`（已验证 RED→GREEN）。

## DL-007 · 冲突副本命名一秒内碰撞，第二份覆盖第一份

- **首次发现**：2026-08-28
- **现象**：冲突副本名精确到秒（`names::conflict_copy_name`），同一文件在一秒内两次冲突会生成同名副本，第二次 `rename` 覆盖第一份——保留下来的第一份本地内容反被丢失。
- **根因**：`crates/gonedrive-core/src/sync/engine.rs::make_conflict_copy` 直接 `rename`，无目标存在性检查。
- **修复**：新增 `names::free_path`——目标已存在则在扩展名前插入 `(2)`/`(3)`… 直到不冲突；`make_conflict_copy` 改用它。
- **回归测试**：`crates/gonedrive-core/src/names.rs::free_path_dodges_existing_conflict_copies`。

## DL-005 · 上传无 If-Match → 拉取与推送之间他端改动被静默覆盖（丢失更新）

- **首次发现**：2026-08-28
- **现象**：已知项上传用 `conflictBehavior=replace` 且不带 eTag；我们拉取 delta 之后、上传之前，他端（另一设备/网页）对同一文件的改动会被我们的上传无声覆盖。
- **根因**：`crates/gonedrive-core/src/graph.rs` 的 `upload_simple`/`create_upload_session` 不发 `If-Match`。
- **修复**：
  1. `upload_simple`/`create_upload_session` 增 `if_match` 参数，已知项上传带 `If-Match: <etag>`；`UploadTarget` 增 `etag` 字段，`run_upload` 从记录取 etag 传入。
  2. 服务端 412 → `run_upload` 捕获：`make_conflict_copy` 保留本地、`delete_subtree` 忘掉旧记录（下次 delta 拉回新远端并下载）、把副本作为新文件重传。绝不覆盖他端改动。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::an_upload_that_would_overwrite_a_newer_remote_becomes_a_conflict`（用 `header_exists("If-Match")` 匹配，证明 header 确实发出）。

---

## 小结：size+mtime 启发式的三处破坏面已全部加哈希门禁

`Engine::local_content_unsynced`（内容哈希 vs `user.gonedrive.quickxor` xattr，稀疏文件短路）现同时守护：**dehydrate 打洞**（DL-004）、**下载覆盖**（DL-004b）、**materialize 空洞重建**（DL-003）。加上 **DL-001 批量删除/挂载护栏**、**SC-002 resync 清扫护栏**、**DL-005 If-Match**、**DL-007 冲突副本防碰撞**，Phase 1 数据丢失面已系统性堵住。
