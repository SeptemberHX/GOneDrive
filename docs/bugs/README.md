# Bug 档案

每一个曾经导致数据丢失、损坏或明显误用的缺陷，都在这里按类别归档一份，配一条会复现它的回归测试。目的只有一个：**同一类 bug 不再回来**。

## 约定

- 每类一个文件，见下方目录。
- 每条 bug 至少写清：**现象** / **根因**（`文件:行`）/ **修复** / **回归测试**（测试文件 + `#[test]` 名，读者能直接跑）/ **首次发现日期**。
- 修复的提交信息里引用本条目的标题，方便双向追溯。
- 回归测试是这份档案的“防线”——没有测试的条目不算修复完成。

## 类别

| 文件 | 覆盖 |
|---|---|
| [`data-loss.md`](data-loss.md) | 任何会**丢失或损坏用户数据**的路径：误删、打洞清零、静默覆盖、丢失更新 |
| [`hydration-and-placeholders.md`](hydration-and-placeholders.md) | 按需下载 / 占位符生命周期：级联下载、拷零、部分回填时间戳漂移、进度窗 |
| [`emblems-and-state.md`](emblems-and-state.md) | 角标语义与 xattr/DB 状态一致性 |
| [`scope-and-safety.md`](scope-and-safety.md) | 作用域与破坏性动作护栏：整盘一键、越根路径、批量删除阀 |
| [`sync-correctness.md`](sync-correctness.md) | 同步正确性：重命名=移动、resync 清扫、大小写碰撞、cTag 抢先、hydrated 文件降级 |
| [`network-resilience.md`](network-resilience.md) | 弱网韧性：中途掉线的分类与重试、read 超时、不过早放弃、下载断点续传 |

运行全部回归测试：`cargo test --workspace`（当前 191 项）。
