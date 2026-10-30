# back-to-back root/unroot 控制服务 OPEN 撞重枚举窗口失败（Stall 未重试 + CNXN 预算不足）

## Goal

修复真机暴露的 end-to-end 故障：wait-for 两 bug 修好后（`adb root` 不再卡 60s、~1s 返回），紧跟的 `adb unroot` 立刻撞上 adbd 重启的重枚举窗口，**控制服务 OPEN 本身失败**。让 back-to-back `root`/`unroot` 全程干净。

## Why now（因果链）

之前 `adb root` 因 wait-for-disconnect 卡 60s，**意外**给了设备 60s 充分重枚举时间；紧跟命令从不撞窗口。wait-for 修复移除该延迟（正确）后，把 backend OPEN 在重枚举窗口的健壮性不足**显形**。本 gap 属上一个任务（backend 重枚举重试）territory，是其盲区——之前 selftest/探针都有 2s settle 掩盖了它。

## What I already know (verified @ HEAD, working tree)

真机 trace（serial YTGUSCNFMFAIK7ZP，back-to-back root→unroot）两种交替失败：
```
open session failed: USB transfer error: endpoint stalled
open session failed: ADB request failed - CNXN failed after 8 attempts (stale CLSE or transient transfer error)
```

三个 gap（互相独立，但同一窗口）：

- **Gap A — `Stall` 不在 transient 家族**：`is_transient_connect_error`（persistent.rs:~119）只认 `Unknown(0xe00002ed)` + `Disconnected`，**故意排除 `Stall`**（当初决策"避免掩盖真实 stall"）。但真机证明：重枚举窗口端点会**合法地短暂 stall**。CNXN write/read 遇 Stall → 不重试 → 立即 `return Err`（persistent.rs:948/960），连 8 次预算都没用上。
- **Gap B — CNXN 预算 < 实测重开时间**：`CNXN_MAX_ATTEMPTS=8 × CONNECT_RETRY_SETTLE=100ms ≈ 800ms`。但 PR0 探针实测 reopen+shell 耗时 **487–1177ms**（中位 ~850ms，**max 1177ms**）→ 800ms 预算**刚好不够**，慢的那次耗尽 8 次后失败。
- **Gap C — CNXN 耗尽错误不可重试**：do_connect 8 次耗尽返回 `ADBRequestFailed("CNXN failed after N attempts...")`（persistent.rs:~990）。但 `is_retryable_open_error`（default_backend.rs:~99）只认 transient-transfer + `DeviceNotFound`，**不认 `ADBRequestFailed`** → `get_or_open` 的 10s 预算**根本没机会**重新 re-drive CNXN。两层重试预算事实上没有串联起来。

另：**控制服务 OPEN（`open_session(Root/Unroot)`）路径本身无任何重试**——"endpoint stalled" 那条就是 OPEN 的 send_open / await_open_response 撞 stall，直接冒泡。这条经 backend `open_session_with_reopen`（上个任务加的）应能 reopen 重试，但前提是错误被判为"连接已死"才 reopen——stall 不一定让 is_alive() 翻 false。需核实。

## Decision (ADR-lite) — v2 REVERSED by 真机 trace + research（2026-06-24）

> ⚠️ v1（connect 层调大内层预算 8→15 + 反放大挡住外层）已被真机 trace 推翻。证据见 `research/transport-reopen-vs-inplace-retry.md`。

**真机 trace 铁证**：`do_connect` 的 15 次重试**全部在同一个失效的旧 transport 上空转**（attempts 2..15 全是 `device disconnected`），因为 adbd 重启会让 USB **重新枚举到新 IOKit registry id**，旧 transport 的 endpoint 永久失效——`do_connect(transport: &mut T)` 只重发 I/O、**从不重开 transport**。日志里 22.670 开了**新** endpoint 后**立刻**成功。结论：**re-enumeration 只能靠重开新 transport 恢复，调大内层预算治标不治本**。这也与 AOSP 一致（reconnect handler 重建连接对象，而非在死 handle 上重试 I/O）。

- **Context**：恢复 re-enumeration 必须重建 transport（`new_from_serial`→`USBTransport::new_by_serial`→新 endpoint）。只有外层 `get_or_open`/`retry_within` 做这件事。v1 的"反放大解耦"恰恰**挡住了外层恢复 re-enumeration**——那个解耦才是 bug。
- **Decision（反转 v1 的三个决策）**：
  1. **内层 `do_connect` 缩小**：transient-transfer 臂从 15 次缩回小预算（≈2-3 次，只治"同 handle、adbd 短暂没应答"的真瞬态）。`CNXN_MAX_ATTEMPTS` 还原（stale-CLSE drain 可保留自己的适度 bound——那是真同-handle 场景）。
  2. **外层 `get_or_open`/`retry_within` 主导 re-enumeration 恢复**：每次 poll 已 `new_from_serial` 重建 transport（trace 证明有效）。把 `is_retryable_open_error` 改为**重开窗口族判定**：retry `UsbTransferError(Stall | Disconnected | Unknown(_))` + `ADBRequestFailed`（CNXN 耗尽=旧 transport 死的信号）+ `DeviceNotFound`；**`InvalidArgument`/`Fault` 致命不重试**，`DeviceBusy` 仍致命。bound 维持 `OPEN_RETRY_BUDGET=10s`/`500ms`。
  3. **家族式分类、终结 whack-a-mole**：`0xe00002d8`(NotReady)/`0xe00002ed`(NotResponding) 都是 `Unknown(_)`，不再逐码列。外层按 `TransferError` 变体族判定（`Unknown(_)` 全收、靠 10s wall-clock + 重建 transport 保持诚实）。**注意：宽 `Unknown(_)` 只能用于外层重开判定**，绝不能用于内层 in-place 循环（会在死 handle 上空转——正是本 bug）。
- **不乘积**：内层缩到小常数（几百 ms），外层 wall-clock 10s 每 poll 重建——总时长 ≈ 外层预算，非乘积。v1 怕的乘积在"内层小"后不复存在，耦合反而是唯一能恢复 re-enumeration 的解。
- **Q2 自查澄清**：当前 22.670 的恢复其实是 **client 下一条命令**触发的全新 `open_local_service`→`get_or_open`（偶发、非设计）——所以单条命令对用户报错。修复让外层在**同一次**调用内重开重试，不再依赖下一条命令。
- **Consequences**：成功路径不变慢；re-enumeration 在外层 10s 内重开恢复；真 unplug/fault bounded 失败；改动集中在 `is_retryable_open_error`（外层）+ 内层缩预算 + 分类家族化。

## Requirements (v2)

- [R1] 内层 `do_connect` transient-transfer 臂缩到小预算（≈2-3，只治同-handle 短暂 not-ready）；`CNXN_MAX_ATTEMPTS` 还原到原值附近。stale-CLSE drain 保留适度 bound。
- [R2] 外层 `is_retryable_open_error` 改为重开窗口族判定：retry `UsbTransferError(Stall | Disconnected | Unknown(_))` + `ADBRequestFailed` + `DeviceNotFound`；`InvalidArgument`/`Fault`/`DeviceBusy` 不重试。
- [R3] 家族式分类，不再逐 IOKit 码列举（`Unknown(_)` 全收）。外层判定不再依赖 `IOKIT_NOT_RESPONDING` 常量精确匹配（内层窄判定可保留）。
- [R4] `open_sync_session`/`open_shell_v2`（走裸 `get_or_open`）自动继承重开恢复；`open_local_service`（`open_session_with_reopen`）first-OPEN race 路径不变。
- [R5] 防乘积不变量：内层 ≤ 小常数，外层 wall-clock 10s 主导，总 ≈ 外层预算。

## Acceptance Criteria

- [ ] 真机 back-to-back：`adb root; adb unroot` ×4 连发**全部干净**，无 `endpoint stalled` / `CNXN failed` / `0xe00002d8`。
- [ ] `time adb root`/`unroot` 仍亚秒返回（成功路径不变慢）。
- [ ] 真 unplug / fault bounded 失败（≤10s），不无限重试、不掩盖。
- [ ] 单测：外层 `retry_within` 闭包"首次 ADBRequestFailed(CNXN耗尽)→重开成功"（纯闭包，无硬件，default_backend.rs 已有 retry_within 测试可扩展）；`is_retryable_open_error` 族判定（Stall/Disconnected/Unknown 可重试，InvalidArgument/DeviceBusy 不可）；内层缩预算后 ScriptedTransport 仍正确。
- [ ] **反放大不变量测试**：内层 attempts 是小常数（确认未回到 15）。
- [ ] fmt / clippy（默认 + server,usb + example）/ 全测试绿。

## Out of Scope
- 不改 wait-for（已验证完成，独立提交）。不改 xdb。

## Technical Notes
- 关键文件：`persistent.rs`（is_transient_connect_error :119 / do_connect CNXN loop :926-990 / send_open+await_open_response 的 OPEN 路径）、`default_backend.rs`（is_retryable_open_error :99 / get_or_open :324 / open_session_with_reopen）。
- PR0 探针数据（上一任务 research 已存）：reopen 487–1177ms 是预算定标依据。可复用 `examples/root_disconnect_probe.rs` 复测 back-to-back（缩短/去掉 cycle 间 2s sleep 即逼近真机连发）。
- 真机验证不可省（窗口时序单测覆盖不到）。

## Research References
- 复用 `.trellis/tasks/archive/2026-06/06-23-backend-get-or-open-settle-retry-root-bug/research/reenumeration-readiness.md`（IOKit 码、两层重试结构、测试 seam）。
