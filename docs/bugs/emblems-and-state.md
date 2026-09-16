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

## ES-002 · 传输完成会把"离线"覆盖成"空闲"

- **首次发现**：2026-09-15（第三轮审计）
- **现象**：远端轮询失败、守护进程已置为离线，但托盘和面板显示"空闲"，`last_error` 也被清空；用户以为一切正常，实际什么都没在同步。
- **根因**：`crates/gonedrived/src/app.rs` 的事件转发任务维护一份私有 `SyncStatus`，每收到一个引擎事件就整体覆盖共享状态（`*shared.write().await = current.clone()`）。它不知道协调器刚刚设置了 `offline`/`keyring_locked`/`signed_out`，于是一个仍在运行的传输切片发出 `Finished` 就把账户级判断抹掉了。
- **修复**：转发器仍可报告任何活动状态，但不得在账户级故障之上宣布 `idle`——"空闲"的含义是"没有待办且一切正常"，而此时后半句为假。`error` 刻意不在此列：那一条是转发器自己为失败的周期设置的，必须能被下一个干净周期清除。
- **补充修复（同一条目）**：转发器还会把它自己旧的队列计数一起写回去——它只在 `Finished` 分支读取待处理队列，其余事件都在回放上一阶段的数字，于是 `StatusChanged` 信号与随后 `Status()` 读到的值互相矛盾。现在只有真正读过队列的那一个分支才发布计数，其余事件沿用共享状态里的值。
- **限制**：仍不是单一写者。正确的结构是让转发器成为状态的唯一写者、`set_state` 改为投递事件；循环体内有多处 `await`（活动记录、回填表），整段持有状态写锁会引入锁序风险，因此没有强行改造。上面两条把该结构缺陷的两个可观察后果都堵住了。
- **回归测试**：`crates/gonedrived/src/app/tests.rs::a_finished_transfer_may_not_declare_idle_over_an_unreachable_account`（覆盖判定规则；转发任务本身需要引擎实例，尚无集成测试）。

## ES-003 · 延期的远端变更永久卡住而用户完全看不见

- **首次发现**：2026-09-16（报告复核）
- **现象**：某个文件在网页版上看得见，本地怎么等也不出现。日志里只有一行 debug 级别的"还是无法本地表示"，`summary.failed` 是 0，界面一切正常。
- **根因**：`remote/delta.rs::apply_deferred_remote` 对重放失败只 `warn!`，既不计入 summary，也不发事件，更没有任何退避或上报机制。延期在正常情况下只持续一轮（父目录就在同一批 delta 的几行之后），所以这条路径从来没人注意。
- **修复**：`deferred_remote_payloads` 连同"已经等了多少秒"一并返回（表里本就有 `updated_at`）。等待超过 6 小时的条目在重放再次失败时计入 `summary.failed` 并发 `Failed` 事件，然后 `touch_deferred_remote` 重置计时——每 6 小时报一次，而不是每轮轮询报一次。
