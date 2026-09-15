# 按需下载 / 占位符

> 占位符生命周期、回填、缩略图与浏览行为。核心：**浏览不应触网、不应改动元数据；回填只补内容不动时间戳。**

---

## HP-001 · 部分回填后未恢复 mtime → 浏览/缩略图把文件时间戳改成"现在"

- **首次发现**：2026-08-27
- **现象**：进入 `桌面备忘/Todo` 等文件夹，里面文件的修改时间全变成物化/浏览的时刻，而非真实远端时间。
- **根因**：任何部分读取（缩略图嗅探头部、浏览、`read()` 一段）触发 HSM 的 range 回填，`fill_from` 用 `pwrite` 写入稀疏文件，把 mtime 顶成墙钟时间。只有**整文件** `finish_hydration` 会恢复远端时间，**部分 range 回填从不恢复**。`crates/gonedrive-core/src/sync/engine.rs` 的 `stage_range` 与 `hsm_client.rs` 的 `Event::Filled` 处理缺恢复步骤。
- **修复**：range 回填完成（`Event::Filled { full:false }`）后调用 `Engine::restore_mtime`，仅在 mtime 实际漂移时（stat 比对）重盖远端时间，避免给每个被浏览文件都开写。`materialize_all` 增加自愈：已存在占位符时间/大小漂移则重盖，误标 Hydrated 却是空洞的文件重建。
- **回归测试**：`crates/gonedrive-core/src/hydrate.rs::a_partial_fill_moves_the_mtime_and_restoring_puts_it_back`。
- **提交**：`Restore a placeholder's time after a range fill; heal drifted stamps`。

## HP-002 · staging 文件残留在用户文件夹（`.NAME.gonedrive-partial`）

- **首次发现**：2026-08-27
- **现象**：读文件后目录里留下一堆 `.xxx.gonedrive-partial`（点前缀隐藏，但仍在）。
- **根因**：helper 消费完 staging 文件后应删除，但删除失败或旧版 helper 会留下，直到 1 小时后的扫描清扫。
- **修复**：守护进程在 `Event::Filled` 时按 `staging_sibling(path)` 主动删除自己 staged 的文件（此时回填已完成、staging 已用尽）；1 小时扫描清扫保留为兜底。见 `crates/gonedrived/src/hsm_client.rs`。
- **回归测试**：`staging_sibling` 命名的单测（`transfer.rs`）+ 会话中真实读取后 0 残留的端到端复验。
- **提交**：`Sweep the staged file when a fill completes, not an hour later`。

## HP-003 · 点开一张图导致其余图片被下载 + 多个进度窗堆叠

- **首次发现**：2026-08-27
- **现象**：点开文件夹里第一张 PNG，其它图片也被下载，且弹出多个下载进度窗。
- **根因**：Nautilus 缩略图会读每个文件头部；浏览类读者的读取被当成消费者触发整文件回填。进度窗无单例约束。
- **修复**：浏览类读者（缩略图/文件管理器嗅探）永不触网——本地有则服务本地窗口，否则放行读空洞、离线作答（`app.rs::stage_for` 的 browsing 分支）；进度窗用单一 tracked 子进程（`ensure_progress_window`）。
- **回归测试**：`crates/gonedrived/src/app.rs` 的 `thumbnailers_and_previews_browse_never_consume` / `file_managers_browse_and_applications_consume`；会话端到端复验（读 1 张，兄弟 5 张 du 不变）。
- **提交**：`Browsing never downloads; folder emblems; silent placeholder refresh`。

## HP-004 · 整文件回填失败后路径锁与 `Hydrating` 状态永久泄漏

- **首次发现**：2026-09-15（代码审计）
- **现象**：离线或令牌过期时打开一个按需文件失败，之后该文件所有读取都要等 helper 300 秒超时，"立即获取"与释放空间挂死，进度窗永远显示，用户对该文件的编辑/删除被本地扫描忽略，直到重启守护进程。
- **根因**：守护进程 `stage_for` 在 `begin_hydration` 之后出错只回复 `Hydrated{ok:false}`，指望 helper 的 `Filled` 事件触发 `fail_hydration`；但 helper（`worker.rs`）对任何 `Deny` 都回 `Filled{ok:false, full:false}`，守护进程把 `full:false` 当作"浏览拒绝，无需清理"。双方互相依赖，无人兜底。`set_hsm(None)` 清空进行中表时同样不回退 DB 状态。
- **修复**：`stage_for` 出错时自行 `fail_hydration`（释放 InFlight 与路径锁，DB 回到 `Dehydrated`，发 `HydrationFinished{ok:false}`）；helper 丢失时对所有进行中的回填做同样处理。
- **回归测试**：待补（需要可注入失败的引擎桩）。
