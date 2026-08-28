# 角标语义与状态一致性

> 文件管理器角标（在线/本地/固定）的语义，以及 xattr 与 SQLite 两处状态的一致性。核心：**角标必须如实反映内容是否在本地；xattr 与 DB 不得静默分歧。**

---

## ES-001 · 文件夹角标误标为"本地对钩"

- **首次发现**：2026-08-27
- **现象**：只要文件夹里有任一文件被下载，文件夹就显示本地对钩；用户要求的是"仅显式右键留存、或全部子项都在本地，才算对钩，否则显示云"。
- **根因**：旧逻辑"任一文件在本地即标本地"。
- **修复**：`Engine::recompute_folder_mark` 改为——显式 pin 或全部直接子项本地（文件 `Synced && local_ino`，子目录自身 Hydrated）才 Hydrated，否则 Dehydrated；自底向上重算（`materialize_all` 逆序）；`hydrate`/`dehydrate`/`set_pin`/`finish_hydration` 变更后 `refresh_folder_ancestors`。会话全盘复验：3098 个对钩文件夹 0 违规。
- **回归测试**：`crates/gonedrive-core` 中折叠/角标相关集成测试；会话全盘断言（每个对钩文件夹的所有子项确在本地）。
- **提交**：`Folder is a check only when fully local or explicitly kept`。

## ES-002 · xattr 与 DB 状态失同步（潜在，放大数据丢失）

- **首次发现**：2026-08-28（探索发现）
- **现象**：xattr 与 `items.state` 可分歧；`repair_states` 是唯一对账者，Nautilus 直接读 xattr、守护进程读 DB，崩溃恢复窗口内两者可显示不同状态。更危险的是失同步会让 dehydrate 的 `state==Dehydrated` 捷径跳过护栏而打真实字节。
- **根因**：占位符状态双权威（xattr 权威用于恢复，DB 用于决策），无内容级交叉校验。
- **修复（已完成，随 DL-004）**：dehydrate 护栏改为内容哈希驱动的 `Engine::local_content_unsynced`——**不再信任 DB 的 Dehydrated 状态**：满分配文件必须哈希匹配已录内容才放行，故 xattr/DB 失同步（DB 说 Dehydrated 但磁盘满内容且未上传）不再能绕过护栏打真实字节。主条目见 [data-loss.md](data-loss.md) DL-004。
- **回归测试**：`freeing_space_refuses_an_in_place_edit_that_kept_size_and_mtime`（同一门禁覆盖失同步机制）。
