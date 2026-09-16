# 弱网韧性（Network resilience）

高铁、地铁、电梯——信号时断时续是常态。这一类归档"链路不稳时会不会丢数据、会不会卡死、会不会白白放弃传输"。

已经稳的部分（作为不变量守住，别回退）：下载走 `partial` 临时文件 + `fsync` + 原子 `rename`，中途断线**绝不**在目标位置留下半截文件；上传分片断点续传（见 SC-005）；hydration 失败对读操作返回 deny，应用报错而非永久阻塞。

---

## NR-001 · 下载中途掉线被判为永久错误 → 第一次失败就放弃

- **首次发现**：2026-08-28（弱网评估）
- **现象**：正在下载的文件一进隧道（连接在**流式读取响应体**时被重置/掐断），该传输在第一次失败就被放弃、文件标 `Error`，信号恢复也不再自动重试。
- **根因**：`error.rs::is_transient` 只认 `is_timeout/is_connect/is_request`，而 body 流阶段的断连是 `reqwest::Error::is_body`（或 IO `ConnectionReset/UnexpectedEof`），都不在列 → `permanent = !is_transient` 为真 → `engine.rs::drain_queue` 立即 give-up。
- **修复**：`is_transient` 纳入 `Error::Http` 的 `is_body()`，以及 `Error::Io` 的 `UnexpectedEof / ConnectionReset / ConnectionAborted / BrokenPipe`。
- **回归测试**：`crates/gonedrive-core/src/error.rs::a_dropped_connection_mid_transfer_is_retryable`。

## NR-002 · 无 read/idle 超时 → 半死连接挂起数分钟

- **首次发现**：2026-08-28（弱网评估）
- **现象**：只有 `connect_timeout(20s)`，没有请求/读取超时。信号掉后 TCP 连接半开（已建立但不再有数据），一次同步请求或一次 hydration（打开占位符）可能挂起到操作系统放弃该 socket（数分钟），最坏卡满 `HYDRATE_TIMEOUT=300s`。
- **根因**：`gonedrived/src/app.rs::build_http_client` 未设 read timeout。
- **修复**：加 `reqwest .read_timeout(30s)`——按**每次读**设限（不是整体传输），卡死的流 30s 内失败并转入重试，慢但活着的链路仍能完成大文件；登录用的客户端同样补 connect/read 超时。
- **回归测试**：配置层改动，由 NR-001 的重试分类 + 下方 NR-003 的"不放弃"共同保证行为；无独立单测（reqwest 超时不便在单测里构造）。

## NR-003 · 放弃过早 → 隧道 8 分钟后文件被永久放弃

- **首次发现**：2026-08-28（弱网评估）
- **现象**：失败 `MAX_OP_ATTEMPTS=8` 次（指数退避约 8.5 分钟）后放弃并删除 op、标 `Error`；一段较长的信号中断就足以耗尽次数，之后除非该项在云端再变或手动 FetchNow，不再自动重试。
- **根因**：`engine.rs::drain_queue` 的放弃条件 `permanent || attempts >= MAX_OP_ATTEMPTS` 把**瞬时**失败也计入永久放弃。
- **修复**：放弃条件改为**仅 `permanent`**。瞬时失败按封顶退避（≤ ~4 分钟一次）一直重试，直到成功或该项在云端变化——信号恢复即自动续上；只有真正永久的错误（非法名、项已删、本地权限）才放弃。移除 `MAX_OP_ATTEMPTS`。
- **回归测试**：与 NR-001 同一分类逻辑；瞬时错误不再进入 give-up 分支（`is_transient` 测试守住分类）。

## NR-004 · 下载不续传 → 大文件在烂链路每次从 0 重下

- **首次发现**：2026-08-28（弱网评估）
- **现象**：`download_to_file_limited` 恒以 `download(id, None)` 从头下载；每次断线都丢弃已下字节从 0 重来，一个比"平均掉线间隔"还大的文件可能永远下不完。
- **修复**：断点续传。瞬时失败时**保留** `partial` 临时文件；下次以 `Range: bytes=<已下长度>-` 续传，并带 `If-Range: <etag>` 校验——服务端仅在内容未变时回 `206 Partial Content` 流式续传，内容已变则回 `200` 整份重来，杜绝把旧前缀拼到新内容上。完成前仍有 `written == total` 完整性校验 + 原子 `rename` 兜底。永久失败才删 partial。
- **回归测试**：`crates/gonedrive-core/src/sync/transfer.rs::an_interrupted_download_resumes_from_its_partial`（预置半截 partial，只 mock 带 `Range`+`If-Range` 的 206；全量 GET 会 404，故只有真正走续传才能通过）。

## NR-005 · 429 重试预算耗尽被当作永久失败 → 限流时放弃传输

- **首次发现**：2026-09-15（代码审计）
- **现象**：服务端持续限流时，某个上传/下载在五次 in-call 重试后被彻底放弃并计入失败，与队列层"限流绝不是放弃的理由"的承诺相矛盾。
- **根因**：`crates/gonedrive-core/src/graph.rs::send` 的 `429 | 503 | 504 if attempt < MAX_ATTEMPTS` 分支耗尽后落入 `_` → `Error::Graph{429}`，而 `error.rs::is_transient` 只认 `status >= 500`。
- **修复**：耗尽预算时返回 `Error::Throttled(delay)` 交给队列退避；`is_transient` 同时把 `Graph{429}`/`Graph{408}` 视为暂时性（覆盖分片上传的同类路径）。
- **回归测试**：`crates/gonedrive-core/src/error.rs::exhausted_throttling_is_still_transient`。

## NR-006 · 令牌端点临时故障被报成"需重新登录"

- **首次发现**：2026-09-15（代码审计）
- **现象**：login.microsoftonline.com 返回 5xx/429 或网关的 HTML 错误页时，界面跳到"已登出"，用户以为凭据失效。
- **根因**：`crates/gonedrive-core/src/auth.rs::post_token` 把任何非 2xx 都映射为 `Error::AuthRequired`；`app.rs::core_error_status` 据此置 `signed_out`。
- **修复**：新增 `Error::TokenEndpoint { status, message }`。只有结构化的 4xx（`invalid_grant` 等）才判定需登录；5xx/429/408 与无法解析的响应体为暂时性错误，UI 显示"离线"而非"已登出"。
- **回归测试**：`crates/gonedrive-core/src/auth.rs::a_dead_grant_requires_a_new_login`、`::a_token_endpoint_outage_is_not_a_sign_out`、`error.rs::a_token_endpoint_outage_is_transient_but_a_dead_grant_is_not`。

## NR-007 · 调度切片到期删除续传前缀 → 超过 5 秒的下载永远从零重来

- **首次发现**：2026-09-15（第三轮审计）
- **现象**：队列下载（固定文件、全量同步模式）中，任何在一个传输切片内下载不完的文件反复从 0 开始，永不完成；弱网大文件表现为"一直在下载、进度反复归零"。
- **根因**：`crates/gonedrive-core/src/sync/transfer.rs` 在失败时只对 `is_transient()` 的错误保留 `.gonedrive-partial`；`Error::Cancelled` 不是瞬时错误，于是被当作"永久失败"删除前缀。而守护进程的传输切片 `TRANSFER_SLICE = 5s` 到期时，进度回调返回 false，正是产生 `Cancelled` 的路径（`coordinator.rs`、`queue.rs::run_download`）。按需回填路径不设 deadline，因此不受影响。
- **修复**：取消不是对传输的判决。`Cancelled` 与瞬时错误一样保留续传前缀。
- **回归测试**：`crates/gonedrive-core/src/sync/transfer/tests.rs::a_scheduling_slice_ending_leaves_the_partial_resumable`（已验证：无修复时 RED）。

## NR-008 · 十五分钟清扫删掉正在退避的下载的续传前缀

- **首次发现**：2026-09-15（第三轮审计）
- **现象**：弱网下大文件反复从零重下。与 NR-007 症状相同但路径不同。
- **根因**：`local.rs` 的扫描顺带清理超过 15 分钟没动过的 `.gonedrive-partial`。判据只有年龄：一个在建连阶段就失败的下载并不会写入任何字节，它的 partial 时间戳因此停住，而该操作只是在退避队列里等待重试——清扫把它当孤儿删掉。
- **修复**：删除前反查该暂存文件是否属于某个仍在队列里的下载。暂存名由目标名派生（过长时取哈希），无法反推，所以改为正向计算：取出所有排队中的下载目标，逐个算出它们会用的暂存名再比对。只有真的遇到陈旧 partial 时才查询，正常扫描零开销；查询失败时保留文件（续传前缀比那点空间值钱）。
- **回归测试**：`crates/gonedrive-core/tests/sync_integration.rs::a_queued_downloads_partial_file_survives_the_janitor`（同时断言无人认领的暂存文件仍会被清掉；已验证：无修复时 RED）。
