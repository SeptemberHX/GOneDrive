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
- **修复**：Phase 3——补 `fold_case` 单测（大小写折叠一致、幂等,含 Turkish İ）+ 集成回归：本地存在仅大小写不同的两个文件时,双方本地内容均不丢失。当前行为已本地安全,测试作为防回归护栏。
- **回归测试**：`crates/gonedrive-core/src/names.rs::fold_case_matches_names_that_differ_only_in_case`、`::fold_case_is_idempotent`;`crates/gonedrive-core/tests/sync_integration.rs::a_local_file_differing_only_in_case_loses_no_data`。

## SC-005 · 上传断点续传路径无集成测试（"未证明即隐患"）

- **首次发现**：2026-08-28（探索发现，测试缺口）
- **现象**：多分片断点续传（`transfer.rs` `resume_or_create` + `nextExpectedRanges`）此前只有 DB 持久化单测，没有端到端集成测试；断点/重试失败会导致截断或重复上传。
- **修复**：补两条 wiremock 集成测试——多分片上传完整走通、以及**从已提交偏移续传而非重启**（`createUploadSession` 断言 `expect(0)` 证明未重开会话）。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_large_upload_completes_in_multiple_chunks`、`::an_interrupted_upload_resumes_from_its_committed_offset`。

## SC-006 · cTag 抢先于内容 → 中断的下载后云端改动永远拉不下来（静默不一致）

- **首次发现**：2026-08-28（真机复现：网页改了 `待购清单.xlsx`，本地始终是旧内容）
- **现象**：在云端改动一个已 hydrate 的文件后，本地文件长期停留在旧内容，任何轮询都不再更新——DB 里 `state='synced'`、`ctag` 已等于最新远端值，但 `local_size(11191) != 远端 size(9603)`，且 `pending_ops` 为空。
- **根因**：`apply_remote` 每次都用 `record_from_remote` 把记录的 `ctag/size` 推进到最新远端值（哪怕内容没下载）；`decide_remote` 的变更检测只比 `ctag`。时序：①第一次 pass 见远端变 → `Download{overwrite:true}` + `state=DirtyRemote` 入队，同时 ctag 被推进；②下载还没落地，守护重启/中断（本例是我引入的 `CapabilityBoundingSet` 崩溃循环，见 hydration-and-placeholders 的运维记录）；③再次 apply 时 `known.ctag == remote.ctag` → `remote_content_changed=false` → 走 `(false,false)` → `state` 回落默认 `Synced`，`DirtyRemote` 与待下载意图一并丢失，且没有任何后续事件会重新触发。
- **修复**：`decide_remote` 的 `(false,false)` 分支即使 ctag 相等，也校验 `!local.is_dir && local.size != remote.size`——尺寸是内容相等的廉价必要条件,不一致即判 `Download{overwrite:true}`。既在重枚举时自愈已卡住的文件,又防复发。覆盖门禁 `local_content_unsynced` 对本地未改动文件返回 false,故会真正覆盖下载、无死循环。
- **回归测试**：`crates/gonedrive-core/src/sync/plan.rs::stale_content_under_a_matching_ctag_is_still_fetched`（无修复时走 Skip=RED，有修复走 `Download{overwrite:true}`=GREEN）。
- **后续硬化（待做）**：`apply_remote` 被动分支（RestampMtime / Skip-UpToDate）应沿用 `known.state` 而非默认 `Synced`,让合法的 `DirtyRemote`/`DirtyLocal` 不被重枚举覆盖;根因层面可让记录的 ctag 在内容真正落地前不推进。

## SC-007 · 已 hydrate 的按需文件在云端变化时被降级但不打洞 → 旧字节永久可读

- **首次发现**：2026-08-28（真机复现，与 SC-006 同一文件 `待购清单.xlsx` 的深层因）
- **现象**：用户曾打开（hydrate）过、但所在文件夹未被 pin 的文件，在云端改动后，本地一直读到旧内容且访问也不触发下载。
- **根因**：`apply_remote` 的 `Download` 分支只看 `wants_content`（pin 选择）。未选中时走 else：`state=Dehydrated` + 「`on_demand && !path.exists()` 才建占位符」。对一个**已实体化**的文件，`path.exists()` 为真 → 只把 DB 状态改成 dehydrated、**不真正打洞**，留下完整旧字节；HSM 见字节已在也不会触发 hydration，遂永久卡旧内容。
- **修复**：Download 分支新增 `materialised` 判断——用 `Occupancy`（`allocated >= apparent_size`，而非会被应用剥掉的 state xattr）判断盘上是否有真实内容；**已实体化即视为该下载**，走 `DirtyRemote` 原地重下、保持 hydrated（用户选定行为：已下载文件跟随云端自动更新，贴合 Windows 客户端）。else 分支去掉 `!path.exists()` 守卫，让未实体化的既有占位符也能刷新到新版本（materialised 守卫已保证不会截断真实内容）。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_cloud_edit_to_a_hydrated_on_demand_file_re_downloads_in_place`（已验证：无修复时 RED——文件被降级、读到旧字节；有修复 GREEN——原地重下为新内容）。

## SC-004 · 上传无 If-Match → 拉取与推送间他端改动被静默覆盖

- **首次发现**：2026-08-28（探索发现）
- **现象**：已知项上传用 `conflictBehavior=replace` 且不带 eTag，拉取 delta 与上传之间他端所做的改动被无声覆盖（丢失更新）。
- **根因**：`graph.rs` 上传不带 `If-Match`。
- **修复**：Phase 1.5——带 `If-Match: <etag>`，服务端 412 转冲突副本。（待做，跨"数据丢失"与"正确性"，主条目将并入 data-loss.md）
- **回归测试**：待补（Phase 1.5）。

## SC-008 · resync 扫尾按错误顺序删除 → 远端已删的嵌套目录被"复活"

- **首次发现**：2026-09-15（代码审计）
- **现象**：delta 游标失效（410）触发全量重枚举后，云端在此期间删除的、含子目录的文件夹会在下一次本地扫描时被当作新目录重新创建到云端。
- **根因**：`crates/gonedrive-core/src/db/items.rs::items_missed_by_resync` 注释声称 deepest-first，实际只 `ORDER BY is_folder ASC`。父目录 A 先于子目录 B 处理：`remove_local(A)` 得 `DirectoryNotEmpty` → `delete_subtree(A)` 连带清掉 B 的记录；轮到 B 时 `local_path` 已为 None，B 目录留在磁盘上没有任何记录。
- **修复**：用递归 CTE 计算每项深度，`ORDER BY level DESC, is_folder ASC, id`，子项永远先于父项处理。
- **回归测试**：`crates/gonedrive-core/src/db/tests.rs::a_resync_sweep_visits_children_before_their_parents`（已验证：无修复时 RED，有修复 GREEN）。

## SC-009 · 一个消失或非 UTF-8 命名的条目让整次本地扫描中止 → 删除永远传不出去

- **首次发现**：2026-09-15（代码审计）
- **现象**：目录里存在文件名不是合法 UTF-8 的文件，或全量扫描期间出现又消失的临时文件（编辑器探测文件），之后该账户的本地删除再也不会同步到云端，且全量扫描不断重跑失败。
- **根因**：`crates/gonedrive-core/src/sync/local.rs::read_dir` 用 `to_string_lossy` 得到一个磁盘上不存在的名字；`prepare_copied_placeholder` 对 ENOENT 直接返回 Err，而它运行在整棵树遍历之后、`enqueue_guarded_deletes` 之前，一个条目的失败丢掉了整次扫描的结果。
- **修复**：非 UTF-8 名字在枚举时跳过并记录警告；已消失的条目按"无事可做"处理而不是中止扫描。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_name_that_is_not_utf8_neither_uploads_nor_aborts_the_scan`（已验证：无修复时 RED）。

## SC-010 · 上传期间本地再次保存 → 自触发 412 → 伪冲突副本，原名回退到中间版本

- **首次发现**：2026-09-15（代码审计）
- **现象**：编辑器连续两次保存，第二次落在第一次的上传期间：文件名变成"冲突副本"，原文件名回到第一次保存的内容，而云端并没有其他人改过它。
- **根因**：`transfer/upload.rs` 在 PUT/最后一块提交**成功后**才比较文件版本，不一致就返回 Err；`queue.rs::run_upload` 因此在 `upsert_item_after_mutation` 之前退出，DB 仍是旧 etag。重试时 `If-Match` 撞上我们自己刚提交的版本，Graph 返回 412，走冲突分支。
- **修复**：上传成功即为成功。`UploadedFile.source_changed` 报告文件在传输中变化；队列提交云端已接受的版本（本地 stat 记录为被上传的那个版本，而不是描述符当前的元数据），再为更新的内容排队一次 `Upload`。
- **回归测试**：`crates/gonedrive-core/src/sync/transfer/tests.rs::a_local_edit_during_upload_is_committed_and_reported`。

## SC-011 · 磁盘空间不足时显式同步永不完成

- **首次发现**：2026-09-15（代码审计）
- **现象**：可用空间低于 `min_free_bytes` 时 `gonedrived sync` 永远挂着，GTK 的"立即同步"也不结束。
- **根因**：`queue.rs::drain_queue` 把下载操作直接过滤掉而不推进 `next_attempt_at`，它们永远"到期"；协调器的显式同步完成条件（无待办，或待办全部在退避中）两者都不满足。
- **修复**：被空间闸门挡下的下载调用 `fail_op` 推迟五分钟并记录原因，显式同步随即以"仍排队 N 项"完成。

## SC-012 · 同批 delta 里"删除 + 同名新建"被当成冲突 → 伪冲突副本被上传

- **首次发现**：2026-09-15（代码审计）
- **现象**：另一端用"先删后建"的方式保存文件（备份工具、rclone），本地凭空多出一份"冲突副本"并上传到云端；按需模式下旧占位符还会成为无记录的孤儿。
- **根因**：`remote/delta.rs::apply_staged_delta` 先应用全部 upsert 再处理 tombstone。新项 Y 应用时在路径上撞见旧项 X 的文件 → `(None, Some(local))` 判 `ConflictCopy`；随后 `upsert_item(Y)` 又撞上大小写折叠唯一索引（X 仍占着名字）。
- **修复**：先找出"名字将被同批 upsert 占用"的 tombstone（按 (parent, fold_case(name)) 匹配），在 upsert 之前应用它们。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_same_batch_delete_and_recreate_under_one_name_is_not_a_conflict`（已验证：无修复时 RED）。

## SC-013 · 占位符刷新后 `local_size` 陈旧 → 远端删除被误判为"本地已修改" → 孤儿占位符

- **首次发现**：2026-09-15（代码审计）
- **现象**：云端先改了一个从未下载的文件（占位符按新尺寸刷新），再把它删掉；本地占位符留在磁盘上，看得见、打不开、永远不会清理，还会反复尝试上传被拒。
- **根因**：`remote.rs` 刷新占位符时改写磁盘 size/mtime 但不写 `local_*`；tombstone 路径不区分占位符，`local_is_modified` 因尺寸不等返回 true → `KeepLocalAndReupload`。从 Download 分支直接创建的占位符也从不记录 stat。
- **修复**：tombstone 路径与 resync 扫尾对"自有且仍为 Dehydrated 的占位符"一律删除；刷新占位符时同步更新记录的本地 stat。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_refreshed_placeholder_is_removed_by_a_later_tombstone_not_kept_as_edited`（已验证：无修复时 RED）。
