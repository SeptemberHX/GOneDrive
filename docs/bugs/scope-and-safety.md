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
- **修复**：Phase 3 补 `service.rs` 的单测（越根路径拒绝、整盘 dehydrate `AccessDenied`）。
- **回归测试**：待补（Phase 3）。

<!-- 关联 data-loss 的 DL-001 批量删除阀也是作用域安全的一部分，主条目见 data-loss.md。 -->
