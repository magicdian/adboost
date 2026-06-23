# backend 对刚重枚举设备缺 settle/retry（adb root 重连路径真实 bug）

## Goal

修复一个**真实生产场景** bug：USB 设备 adbd 重启后重新枚举的瞬间（`adb root`/`unroot`/`tcpip`/`usb`/reboot 触发），server backend 对该设备的首个连接/首个 OPEN 会撞上"端点已枚举但未就绪"的 IOKit 瞬态而失败，且**没有任何重试**。这正是 PR2 刚启用的 `adb root` 重连握手最易触发的路径——client 等到设备回来后立刻发下一个服务，backend `get_or_open` + 首个 `open_session` 与未就绪端点竞态。

## Why this is a production bug (not just selftest)

`adb root` 经 adboost 的真实链路：frontend 桥接 `root:` → `DefaultDeviceBackend::open_local_service` (default_backend.rs:450) → `get_or_open`（裸 `new_from_serial`，**零重试**，default_backend.rs:243-258）→ `open_session`。adbd 重启 → 连接死 → client 走 `wait-for-disconnect`（PR2）等设备回来 → 立刻发 `shell:` 等 → 再次 `get_or_open` + 首个 OPEN 撞未就绪端点。对比之下，selftest 的 `open_device_with_retry` 有重试兜底证明该模式有效，但 **backend 没有等价物**。selftest 只是先把它暴露出来。

## Confirmed facts (research/reenumeration-readiness.md)

- **IOKit 码名修正**（对照 pinned `io-kit-sys 0.5.0`）：`0xe00002ed` = `kIOReturnNotResponding`（**非** Aborted；Aborted 是 `0xe00002eb`），`0xe00002c0` = `kIOReturnNoDevice`。现有 spec line 507 命名有误，需一并修正。
- nusb 0.2.3 映射：`NoDevice(0xe00002c0)` → `TransferError::Disconnected`；`NotResponding(0xe00002ed)` → `TransferError::Unknown(0xe00002ed)`（落到兜底臂）。
- **两个竞态点**：
  1. **CNXN race**：`do_connect` (persistent.rs:817,819) 的写/读用 `?` 直接传播；现有 `CNXN_MAX_ATTEMPTS=8` 循环**只对 stale CLSE 重试**，不覆盖 transfer 瞬态。
  2. **首个 OPEN race**：CNXN 成功、`new()` 返回 Ok 后，首个 `open_session` 的 OPEN 帧经 writer 任务发送，撞瞬态 → writer 的 `Err(e) => break` 致命臂（persistent.rs:1271，故意为 truncation 安全设计）→ 连接死。
- `USBTransport::connect` (usb_transport.rs:381-416) 无 settle/就绪探测——claim 接口+取端点后首个 transfer 直接竞态。
- adboost 当前不检查任何 IOKit 码，仅 `Cancelled` 被特判；`TransferError` 变体经 `RustADBError::UsbTransferError` 保留，可作分类依据，但**码层无法可靠区分"重枚举瞬态"与"真拔线"**——只能靠 bounded 预算 + 枚举存在性交叉验证。
- AOSP 对齐意图：host transport 层有 bounded+backoff 的 reconnect handler，单次 (re)open 瞬态失败**不上报给用户**；`adb root` 后 client `wait-for-device`。我们的 `CNXN_MAX_ATTEMPTS` 循环即其类比。

## Requirements

### 模块 A：do_connect 覆盖 transfer 瞬态（修 CNXN race，惠及所有 consumer）
- [R1] 新增一个小分类器 helper（pure，可单测）判定"瞬态 transfer error"：匹配 `RustADBError::UsbTransferError(TransferError::Unknown(0xe00002ed))`（NotResponding）与 `TransferError::Disconnected`（NoDevice 0xe00002c0）。是否纳入 `Stall` 见 Open Question。
- [R2] `do_connect` 的 bounded 循环：CNXN 写/读遇**瞬态**时 settle 一小段（复用现有 ~100ms sleep idiom）后重试，而非 `?` 直接传播；非瞬态（`WrongResponseReceived`/`ADBRequestFailed` 等）仍 fail-fast。沿用 `CNXN_MAX_ATTEMPTS` 边界（或新建一个小的 connect-retry 边界）使真缺席设备仍快速失败。
- [R3] `do_connect` 泛型于 `T: ADBMessageTransport`，分类器须保证 TCP 的中性错误不会误判为 USB 瞬态（仅匹配 transfer-error family）。

### 模块 B：get_or_open bounded 重试（修首个 OPEN race + 短暂缺席）
- [R4] 把重试逻辑抽成一个小的 pure 策略函数（`open_with_retry(budget, deadline_clock, || …)` 形态，镜像 selftest 的 `open_device_with_retry`），可用"失败 N 次后 Ok"的闭包单测，独立于 USB 绑定。
- [R5] `DefaultDeviceBackend::get_or_open`（及其覆盖首个 OPEN 的调用点）在 bounded 预算内：`new_from_serial` 瞬态失败、或连接在首个 `open_session` 上死掉（`is_alive()` 为 false），则丢弃重开重试。额外覆盖 `new_by_serial → DeviceNotFound`（设备短暂未枚举），这是模块 A 结构上看不到的。
- [R6] **不可**在 `conns` mutex 持有期间跑多秒重试循环——会串行化所有 `get_or_open` 调用方。须在锁外重试或谨慎缩小锁范围。

### 模块 C：契约测试 + spec
- [R7] 新增 `#[cfg(test)]` 的 scripted mock `ADBMessageTransport`（`VecDeque` 编排 write/read 结果）：前 N 次 `write_message` 返回瞬态 err 后成功，`read_message` 返回 canned CNXN banner；断言 `do_connect`/`new_with_features` 在瞬态后成功。锁定 R2 行为。
- [R8] 单测 R4 的 retry 策略（闭包失败 N 次后 Ok / 一直失败到预算耗尽则 Err）。
- [R9] 更新 `.trellis/spec/backend/server-host-protocol.md`：修正 IOKit 码名；新增 backend-level 子节说明经 server 的 consumer（`adb root` 重连路径）不再需自带重试；记录瞬态分类靠 `TransferError` 变体 + bounded 预算（绝非码层单判）。对齐 memory `prefer-root-cause-fix-at-contract-layer` / `tcp-async-path-missing-usb-guarantees`。

## Resolved decisions (user-confirmed)

- [Q1→定] 瞬态 family **纳入** `TransferError::Disconnected`（NoDevice 0xe00002c0）+ `TransferError::Unknown(0xe00002ed)`（NotResponding）。两者真实日志都出现过，必须都覆盖。
- [Q2→定] **do_connect** 沿用 `CNXN_MAX_ATTEMPTS=8` + 100ms settle（握手层，快）；**backend** 用时间预算 **~10s / 500ms poll**（覆盖重枚举窗口，职责不同各自合理）。
- [Q3→定] **不纳入** `Stall`（保守，避免掩盖真实端点 stall）。

## Acceptance Criteria

- [ ] 契约测试：scripted transport 前 N 次 CNXN 写瞬态后，`do_connect` 仍成功（无需硬件）。
- [ ] 单测：retry 策略闭包"失败 N 次后成功""一直失败到预算耗尽"两路径。
- [ ] 瞬态分类器单测：`Unknown(0xe00002ed)` / `Disconnected` 判为瞬态；`WrongResponseReceived` 等判为非瞬态。
- [ ] 真缺席设备（一直失败）在 bounded 预算内 fail-fast，不挂死。
- [ ] `conns` mutex 不在重试期间持有（代码审查 + 注释说明）。
- [ ] fmt / clippy（默认 + server,usb）/ 全部测试绿。
- [ ] 真机验证：`adb -s <serial> root` 后紧接 `adb -s <serial> shell echo ok` 不再因首个 OPEN 瞬态失败（配合新自动化 root_unroot selftest 用例稳定通过）。

## Out of Scope

- 不动 `USBTransport::connect`（选项 c，过于侵入、与 do_connect 首个 CNXN 重复）。
- 不改 writer loop 的致命臂（选项 d，truncation 回归风险）——首个 OPEN race 由 backend 重开覆盖。
- 不改 xdb。

## Decision (ADR-lite)

**Context**：post-re-enumeration 瞬态有两个竞态点（CNXN、首个 OPEN），分散在共享传输层与 server backend；selftest 有 consumer 重试但 backend 没有。
**Decision**：双层修复——(b) `do_connect` 覆盖 transfer 瞬态修 CNXN race（惠及所有 consumer，最小爆炸半径，复用 `CNXN_MAX_ATTEMPTS` idiom）；(a) `get_or_open` bounded 重试修首个 OPEN race + 短暂缺席（owns 连接生命周期 + `is_alive` 回收的那一层，正是 `adb root` 生产路径）。共享 USB 传输层保持不动。
**Consequences**：library-wide CNXN race 在单一共享握手处一次修复；生产 server `adb root` 路径在 owns lifecycle 的那层修复。与 AOSP "bounded reconnect retry + 不上报单次瞬态" 对齐。

## Research References

- [`research/reenumeration-readiness.md`](research/reenumeration-readiness.md) — IOKit 码解码、两竞态点 file:line、候选修复层对比与推荐、AOSP 对齐、测试 seam。
