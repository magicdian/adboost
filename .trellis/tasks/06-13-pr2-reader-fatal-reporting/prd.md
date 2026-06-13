# PR2: Report reader-fatal connection death honestly in usb_direct suite

> 子任务 of [`06-13-adb-client-persistent-usb-robustness`](../06-13-adb-client-persistent-usb-robustness-graceful-shutdown-reader-fatal-suite-reporting-per-session-backpressure/prd.md)。修复缺陷 **A**。

## Goal

当 usb_direct 的 `PersistentUsbConnection` 在 open 成功后、reader task 中途因致命错误（如 macOS IOKit `0xe00002ed` kIOReturnAborted，多设备背靠背 claim 被抢）暴毙时，当前 `run_usb_direct_suite` 会让剩余每个 case 各自产生一条语义混乱的 `error sending data to channel` FAILED。本 PR 让这种连接级故障被**如实、清晰地上报为连接级失败原因**，而非 N 条误导性 per-case 错误；并对多设备背靠背 claim 的时序做防御性加固以减少触发。

## Root cause (confirmed, run1)

- `run_usb_direct_suite`（`mod.rs:260`）只在**首次 open** 失败时 `skip_suite`；open 之后 reader 中途死亡无检测。
- run1：第二台设备 reader 在 open 后立刻拿到 `USB transfer error: unknown (error 0xe00002ed)`（fatal），连接死亡 → 3 个 case 全部 `error sending data to channel` FAILED，掩盖了真正的连接级根因。
- `PersistentUsbConnection::is_alive()`（基于 `reader_handle.is_finished()`）已存在，可作连接存活探针。

## Requirements

1. **连接存活感知的 case 执行**：在 usb_direct 每个 case 前检测 `conn.is_alive()`：
   - 若 reader 已死 → 该 case（及其后所有 case）直接报为**连接级 FAILED**，带清晰原因（"persistent connection died (USB reader task exited; e.g. the OS aborted the device claim)"），不再运行、不再产生 `error sending data to channel`。
   - 若 case 运行后失败且此刻 `!is_alive()` → 用连接级根因**标注**该失败原因（保留底层错误但点明真因）。
   - 利用 async fn 惰性：把未 await 的 case future 传入 guard helper，存活才 await。
2. **多设备时序加固**：usb_direct 跨设备循环（`mod.rs:90-92`）在设备之间加入小幅 settle 延迟，降低背靠背 claim 触发 IOKit abort 的概率。
3. 不改 `PersistentUsbConnection` 行为；纯 selftest 侧（`mod.rs`）改动 + 复用既有 `is_alive()`。

## Acceptance Criteria

- [ ] 模拟/真实 reader-death 时，usb_direct 输出**一条清晰的连接级失败原因**（每个剩余 case 同一可读原因），不再出现裸 `error sending data to channel`。
- [ ] reader 存活的正常路径行为不变（PASS 仍 PASS，真实 case 失败原因不被错误标注）。
- [ ] 连续 3 次多设备 `selftest --no-interactive`：要么 usb_direct 全 PASS（settle 生效消除 claim race），要么在确有 reader-death 时给出清晰连接级失败而非混乱多错。
- [ ] 新增单元测试：guard helper 在连接死亡时短路为连接级原因、存活时透传 case outcome、失败+死亡时标注根因（用可控的 is_alive/outcome 桩，无需硬件）。
- [ ] `cargo build` + clippy + 现有测试全绿。

## Out of Scope

- B+D 优雅关闭 → PR1（已完成）。
- C per-session 背压 → PR3。
- IOKit `0xe00002ed` 的底层规避（驱动/nusb 层）；本 PR 只做如实上报 + 时序加固。
- through_server / 标准 suite 的等价改造（usb_direct 是 PersistentUsbConnection 直连路径，是本缺陷唯一现场；ADBProxyDevice 经 server，故障模型不同）。

## Technical Notes

- 关键文件：`adboost_cli/src/selftest/mod.rs`（`run_usb_direct_suite`、设备循环）、`adboost_cli/src/selftest/cases.rs`（persistent_* case 签名：均 `&PersistentUsbConnection -> Outcome`，可统一 guard）、`adboost_cli/src/selftest/report.rs`（`Outcome`）。
- 设计：guard helper `async fn guarded_persistent_case(conn, fut) -> Outcome`，未 await 的 future 惰性传入；`is_alive()` 假后短路。
- 实测基线：`/tmp/selftest_run1.log`（reader fatal + 3 FAILED 现场）。
