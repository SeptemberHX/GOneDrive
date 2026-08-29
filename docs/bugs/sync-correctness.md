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
