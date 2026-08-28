# 同步正确性

> 非"数据丢失"但会造成错误同步行为的缺陷：重命名退化、resync 清扫、大小写碰撞、丢失更新。

---

## SC-001 · 跨目录重命名退化为"删除 + 重传"

- **首次发现**：2026-08-27
- **现象**：把文件移动到另一目录，本应是一次服务端 PATCH move，却变成删除旧项 + 重新上传，丢失服务端版本历史、浪费带宽。
- **根因**：扫描早期分别处理"这里新增"和"那里消失"两半。
- **修复**：`scan_local` 在做任何破坏性动作前，用 inode 配对 vanished/appeared 两半，配成一次 `MoveRemote`（`has_moved` 校验旧路径确已不存在，防 inode 复用）。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_cross_directory_rename_is_one_move_not_delete_plus_upload`。

## SC-002 · full_resync 清扫删除本地已改动文件（无 modified 护栏）

- **首次发现**：2026-08-28（探索发现）
- **现象**：410 resync 触发全量清扫时，对枚举"漏掉"的项直接 `remove_local`，不做 `local_is_modified` 检查，与 delta 路径（保留并重传）不一致——本地有未上传改动的文件被删。
- **根因**：`crates/gonedrive-core/src/sync/engine.rs::full_resync` 清扫段对 `items_missed_by_resync` 的每一项直接 `remove_local`，无 `local_is_modified` 检查——与 delta tombstone 路径的 `KeepLocalAndReupload` 不一致。
- **修复**：清扫前若为文件且 `local_is_modified(stale, stat)` 为真，则 `delete_subtree` 脱钩 + 入队 `Upload`（保留并重传），与 delta 路径完全一致；否则才 `remove_local`。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_resync_sweep_keeps_a_locally_edited_file_it_would_have_deleted`（已验证：无修复时 RED，有修复 GREEN）。

## SC-003 · 大小写折叠 / Unicode 碰撞未测

- **首次发现**：2026-08-28（探索发现）
- **现象**：OneDrive 大小写不敏感、Linux 敏感；仅大小写不同的两个远端项落同一目录时的行为无测试，属潜在覆盖/损坏风险。`names.rs::fold_case` 零测试。
- **修复**：Phase 3——`fold_case` 单测 + 集成场景。
- **回归测试**：待补（Phase 3）。

## SC-004 · 上传无 If-Match → 拉取与推送间他端改动被静默覆盖

- **首次发现**：2026-08-28（探索发现）
- **现象**：已知项上传用 `conflictBehavior=replace` 且不带 eTag，拉取 delta 与上传之间他端所做的改动被无声覆盖（丢失更新）。
- **根因**：`graph.rs` 上传不带 `If-Match`。
- **修复**：Phase 1.5——带 `If-Match: <etag>`，服务端 412 转冲突副本。（待做，跨"数据丢失"与"正确性"，主条目将并入 data-loss.md）
- **回归测试**：待补（Phase 1.5）。
