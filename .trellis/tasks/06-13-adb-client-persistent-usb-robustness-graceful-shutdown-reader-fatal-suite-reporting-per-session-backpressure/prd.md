# adb_client Persistent USB Robustness

## Goal

Selftest（`adboost_cli selftest --no-interactive`）在真机、尤其多设备场景下出现 flaky：`usb_direct.*` 时而 SKIPPED（stale CLSE）时而 FAILED（reader 致命错误），并伴随大量 `writer task gone` / `queue full, dropped OKAY/CLSE` 警告。根因集中在 `adb_client` 的 persistent USB 连接生命周期与背压策略。本任务系统性修复这组关联的健壮性缺陷，使 selftest 稳定通过、警告归零或可解释。

## What I already know

诊断已通过两次实测确认（`/tmp/selftest_run1.log`、`run2.log`，2 设备：QUALCOMM SA8155P-ADP + XPENG d02）。四类问题：

- **B（stale-CLSE 自污染环）**：`usb_direct` 在每次 run 最先跑（`selftest/mod.rs:90`）。上一次 run 收尾时缓存连接裸 drop、CLSE 没 flush，残留在设备端的 stream 让本次 `do_connect` 的 CNXN 连撞 3 次 stale CLSE（`persistent.rs:594-638`）→ 整个 suite SKIP。run 间自我延续。
- **D（writer task gone on drop）**：`UsbDeviceBackend.conns: Mutex<HashMap<String, Arc<PersistentUsbConnection>>>`（`usb_backend.rs:37`）从不优雅关闭。`InProcessServer::drop` 只 `task.abort()`（`channels.rs:112`），daemon 走 `select!{sigterm, serve()}` 直接 drop。进程收尾时 writer task 先于 connection 的 Drop 被 runtime 拆除 → `SessionInner::drop`/`PersistentUsbConnection::drop` 的 fire-forget CLSE 入队失败（`persistent.rs:1381,1432`）。B 与 D 是同一根因的两端。
- **A（reader 致命错误漏报为 case 失败）**：run 1 中第二台设备 reader 拿到 macOS IOKit `0xe00002ed`（kIOReturnAborted，claim 被抢/端点中断），整条连接死亡，该连接上后续每个 case 各自 `error sending data to channel` 报 FAILED。`run_usb_direct_suite` 只在**首次 open** 失败时 `skip_suite`（`mod.rs:256-265`），open 后 reader 中途暴毙就退化成 N 个语义混乱的逐 case FAILED。
- **C（per-session 背压丢 OKAY/CLSE）**：reader 的 per-session bounded queue（`SESSION_CHANNEL_SIZE=64`）满时按"reader 永不阻塞"策略 `try_send` 丢帧并 warn（`persistent.rs:811-817`）。丢 OKAY 破坏流控窗口记账；丢 CLSE 丢失关闭信号 → 反过来又喂给 B 的污染环。高吞吐（reverse/iperf3）下复现。

## Architecture constraints (inspected)

- `AdbServerFrontend::serve(mut self)`（`frontend.rs:106`）消费 self、无限 accept loop、无 shutdown 钩子；backend 经 `Arc<Self>` 共享。
- `PersistentUsbConnection::close(mut self)`（`persistent.rs:1336`）消费式，Arc 持有时无法调用 → 需新增 `&self` 优雅关闭 API。
- `is_alive(&self)`（`persistent.rs:1324`，基于 reader_handle.is_finished）已存在，可用于 A 的连接健康检测。
- reader 的 never-block 不变量是架构核心（`persistent.rs:17,811`），C 的修复不能破坏它。

## Requirements

按优先级拆为独立 PR，逐个交付：

- **PR1（B+D 根因：优雅关闭）**
  - `PersistentUsbConnection` 新增 `&self` 优雅关闭 API（如 `async fn shutdown(&self)`）：flush 连接级 CLSE（带 ack 等确认），可被 `Arc` 持有者调用。
  - `UsbDeviceBackend` 新增 `async fn shutdown(&self)`：遍历 `conns` 逐个优雅关闭。
  - 触发点接入：daemon SIGTERM 路径、`InProcessServer` 停机路径，在 abort/drop 之前 await backend shutdown。
  - **【已定】** drain-stale 加固：`do_connect` 开头的 drain-stale 循环加固为防御性收敛（残留多帧时多轮 drain / 提高 CNXN 重试上限），即使优雅关闭偶尔漏掉也能自愈。
- **PR2（A：连接级故障如实上报）**
  - `run_usb_direct_suite` 在 open 成功后，对每个 case 前/后检测连接存活；reader 致命死亡时，将"剩余 suite"统一报为带致命原因的 FAILED（或一条连接级 FAILED + 其余明确 skip），不再退化为 N 个 `error sending data to channel`。
  - 多设备背靠背 claim 的时序加固（inter-device settle）作为可选项（见 Open Questions）。
- **PR3（C：背压不丢控制信号）— 【已定】彻底方案**
  - CLSE（关闭信号）可靠化：reader 在分类到 CLSE 时直接置 `shared.closed`（必要时唤醒等待方），不依赖 data 队列投递；数据队列满也不丢关闭语义。
  - OKAY/窗口 ack 彻底化：控制信号（OKAY/CLSE）与数据帧分离——OKAY/窗口 ack 不再与数据共用会被丢弃的 bounded queue，改为独立通道或在 reader 私有状态内直接合并窗口记账，结构上保证流控信号不丢。
  - 仍不得破坏 reader never-block 不变量。

## Acceptance Criteria

- [ ] 连续 3 次 `adboost_cli selftest --no-interactive`（多设备）均无 FAILED；`usb_direct.*` 稳定 PASS（不再 run 间 flaky SKIP/FAIL）。
- [ ] 进程收尾不再出现 `could not enqueue CLSE/connection CLSE on drop: writer task gone`（或降级为仅在真正异常路径出现并可解释）。
- [ ] 高吞吐 case（reverse_iperf3）不再出现 `dropped CLSE message`；`dropped OKAY` 归零或不影响窗口正确性。
- [ ] reader 致命错误时 selftest 输出清晰的连接级失败原因，而非多条 `error sending data to channel`。
- [ ] 新增/更新单元测试覆盖：`&self` 优雅关闭 flush CLSE、CLSE 在数据队列满时仍被观测。
- [ ] `cargo build` / clippy / 现有测试全绿。

## Definition of Done

- 每个 PR 独立可编译、可测、可验收；按 PR1→PR2→PR3 顺序提交。
- 单元/集成测试更新（persistent.rs 已有 tests 模块，沿用其无硬件测试模式）。
- clippy / 现有测试 green。
- 行为变更处更新模块文档注释（persistent.rs 顶部架构注释、close/shutdown 契约）。

## Out of Scope

- tcpip 通道实现（`tcpip.shell_echo` 的 pre-wired SKIP 是已知占位，不在本任务）。
- `through_server.shell_exit_code` 的 SKIP（host 协议本就不透传 exit code，设计如此）。
- reverse_echo/iperf3 在缺 `nc`/`iperf3` 时的能力探测 SKIP（合理）。
- macOS IOKit `0xe00002ed` 的底层规避（属 nusb/平台层；本任务只保证如实上报 + 时序加固，不深入驱动层）。

## Decision (ADR-lite)

**Context**: 四类关联缺陷（B/D 同根因，A 报告退化，C 背压丢控制帧）需按风险可控的方式分批交付。
**Decision**:
- PR 顺序锁定 PR1(B+D 优雅关闭) → PR2(A 故障如实上报) → PR3(C 背压彻底化)，逐个交付待验收。
- PR1 一并加固 `do_connect` 的 drain-stale，提供自愈兜底。
- PR3 采用**彻底方案**：控制信号（OKAY/CLSE）与可丢弃的数据队列分离，结构上保证流控/关闭信号不丢，而非仅扩容。
**Consequences**: PR3 改动触及 reader 核心 demux 与 SessionChannels 结构，回归风险最高 → 放最后、测试覆盖最重；PR1/PR2 风险低、收益快（先消除 selftest flaky 主因）。

## Open Questions

（全部已解决）

## Technical Notes

- 关键文件：`adb_client/src/message_devices/usb/persistent.rs`（close/shutdown/Drop/reader_loop/classify）、`adb_client/src/server/usb_backend.rs`（conns 缓存）、`adb_client/src/server/frontend.rs`（serve 生命周期）、`adboost_cli/src/daemon.rs`（SIGTERM）、`adboost_cli/src/selftest/{mod,channels}.rs`（InProcessServer、usb_direct suite）。
- 实测日志：`/tmp/selftest_run1.log`（含 reader fatal + 3 FAILED）、`/tmp/selftest_run2.log`（13 SKIPPED，纯污染环）。
