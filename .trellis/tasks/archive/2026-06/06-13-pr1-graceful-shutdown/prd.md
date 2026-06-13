# PR1: Graceful shutdown for persistent USB connections

> 子任务 of [`06-13-adb-client-persistent-usb-robustness`](../06-13-adb-client-persistent-usb-robustness-graceful-shutdown-reader-fatal-suite-reporting-per-session-backpressure/prd.md)。修复缺陷 **B+D**（同根因）。

## Goal

消除 selftest 进程收尾时的 `could not enqueue CLSE/connection CLSE on drop: writer task gone` 警告，并切断由此产生的 stale-CLSE run 间自污染环（下一次 `usb_direct.*` 因撞残留 CLSE 而 SKIP）。根因：server 缓存的 `Arc<PersistentUsbConnection>` 从无优雅关闭路径，只能在进程退出时裸 Drop，而那时 writer task 已被 tokio runtime 拆除，fire-forget CLSE 入队失败 → 设备端孤儿流残留。

## Root cause (confirmed)

- `UsbDeviceBackend.conns: Mutex<HashMap<String, Arc<PersistentUsbConnection>>>` 缓存连接，无关闭 API。
- `InProcessServer::start` 把 `backend` move 进 spawned frontend task（`channels.rs:87-95`），selftest 侧拿不到 backend 引用；`InProcessServer::drop` 只 `task.abort()`。
- daemon `run_server` 用 `tokio::select!{ serve(), sigterm }`，SIGTERM 命中只是返回、走 `Ok(())`，backend Arc 随栈帧 drop。
- 两条路径最终都靠 `PersistentUsbConnection::Drop` / `SessionInner::Drop` 的 fire-forget CLSE，但 writer task 此刻常已停 → `try_send_fire_forget` 返回 `BrokenPipe` → warn + 设备端泄漏。
- `PersistentUsbConnection::close(mut self)` 是消费式，`Arc` 持有者无法调用。

## Requirements

1. **`PersistentUsbConnection` 新增 `&self` 优雅关闭 API**（如 `async fn shutdown(&self)`）：
   - 在 writer task 仍存活时 flush 连接级 CLSE（带 ack 确认，复用 `WriterHandle::send_with_ack`）。
   - 幂等：重复调用安全；与既有 `close(self)` / `Drop` 不重复发送（单一真相来源，复用内部 helper）。
   - 不得 abort reader/writer（让其自然在 Drop 收尾）；或在 flush 后温和触发——以"CLSE 确实上线"为准。
2. **`UsbDeviceBackend::shutdown(&self)`**：遍历 `conns`（必要时连同 `reverse` pump 持有的 session）逐个 `conn.shutdown().await`，清空缓存。
3. **触发点接入**：
   - `InProcessServer` 持有 backend 的 `Arc` 引用，停机时（新增 `async fn shutdown(self)` 或在 drop 前显式调用）先 `backend.shutdown().await` 再 abort accept task。selftest `run_through_server_phase` 末尾显式调用。
   - daemon `run_server` SIGTERM/ctrl_c 命中后，在返回前 `backend.shutdown().await`。需让 frontend 不再 move-away backend（持 `Arc` 共享）。
4. **drain-stale 加固**（已定）：`do_connect` 开头的 drain-stale 循环增强为防御性收敛——残留多帧时多轮 drain、和/或提高 CNXN stale-CLSE 重试上限/退避，确保即使优雅关闭偶尔漏掉也能自愈。

## Acceptance Criteria

- [ ] 连续 3 次 `adboost_cli selftest --no-interactive`（多设备）进程收尾**不再**出现 `could not enqueue CLSE ... writer task gone` / `could not enqueue connection CLSE ... writer task gone`。
- [ ] 连续 3 次中 `usb_direct.*` 不再因 `CNXN failed after 3 attempts (stale CLSE)` 而整套 SKIP（B 的污染环切断）。
- [ ] 新增单元测试：`shutdown(&self)` 在 writer 存活时把连接级 CLSE 送上 writer 通道（沿用 persistent.rs 既有无硬件 `#[cfg(test)]` 模式，断言 writer_rx 收到 CLSE）；幂等性（二次调用不 panic / 不重复发）。
- [ ] `cargo build` + `cargo clippy`（无新警告）+ 现有测试全绿。
- [ ] 行为变更处更新文档注释（persistent.rs 顶部 teardown 段、shutdown 契约、backend、daemon、channels）。

## Out of Scope

- A（reader 致命错误上报）→ PR2。
- C（per-session 背压丢 OKAY/CLSE）→ PR3。注意：本 PR 不改 reader 的 per-session 投递策略。
- IOKit `0xe00002ed` 底层规避。

## Technical Notes

- 关键文件：`adb_client/src/message_devices/usb/persistent.rs`（close/Drop/do_connect/SessionInner）、`adb_client/src/server/usb_backend.rs`（conns/reverse）、`adb_client/src/server/frontend.rs`（serve 消费 self；可能需 backend 访问器或调整所有权）、`adboost_cli/src/daemon.rs`（SIGTERM）、`adboost_cli/src/selftest/channels.rs`（InProcessServer）、`adboost_cli/src/selftest/mod.rs`（through-server 阶段收尾）。
- 约束（来自 spec）：reader never-block 不变量不动；新增 `RustADBError` 变体须在 `adb_cli_error.rs` 穷尽 match 加分类臂；日志全限定 `tracing::` + `PersistentUsb:` 前缀；锁点用 `?` 传播 `PoisonError`，不抄 `lock().unwrap()`。
- `frontend.serve(mut self)` 当前消费 self；接入 backend.shutdown 时优先让 `AdbServerFrontend` 暴露 `backend()` 访问器或让 daemon/InProcessServer 各自持 `Arc<UsbDeviceBackend>` clone，避免破坏 serve 的现有所有权语义。
- 实测基线日志：`/tmp/selftest_run1.log`、`/tmp/selftest_run2.log`。
