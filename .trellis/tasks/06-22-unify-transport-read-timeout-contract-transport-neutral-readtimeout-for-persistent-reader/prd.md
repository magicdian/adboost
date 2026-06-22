# Unify transport read-timeout contract (transport-neutral ReadTimeout)

## Goal

`ADBMessageTransport::read_message_with_timeout` 接受 timeout 参数，但 trait **未规定超时时返回哪个
error 变体**，导致各实现各行其是：USB 返回 `RustADBError::UsbTimeout`，TCP 返回
`RustADBError::IOError(ErrorKind::TimedOut)`。transport-generic 的 persistent reader loop 只能 match
具体变体，它只认了 `UsbTimeout`——于是 TCP 的超时被当成致命传输错误，拆掉整条持久连接。

本任务在 **trait 契约层面**统一"读超时"的表示：引入 transport 中立的 `RustADBError::ReadTimeout`，
USB / TCP 实现都返回它，reader 只认它。根治"每加一种 transport 就重演一次超时误判"的抽象泄漏。

## Bug 证据与根因

- **现象**：真机 selftest `tcpip.shell_through_tcp_device`：CNXN 握手成功（`host:connect registered`），
  约 1s 后 `WARN reader: PersistentUsb reader error (fatal): TCP read timed out` → `host:disconnect removed`
  → `open session failed: error sending data to channel`。
- **触发链**：TCP 空闲 → reader 的 `read_message_with_timeout(1s)`（persistent.rs:1143）超时 →
  TcpTransport 返回 `IOError(ErrorKind::TimedOut)`（tcp_transport.rs:62-65）→
  reader 分类 `Err(RustADBError::UsbTimeout) => ReadTimeout` 未命中（persistent.rs:1147）→
  落入 `Err(e) => ReadError(e)` → fatal 分支 break（persistent.rs:986-1012）→ 连接拆除。
- **根因（抽象泄漏）**：`ADBMessageTransport` trait（adb_message_transport.rs:36）的超时方法没有约定
  超时的 error 表示；transport 无关的消费者被迫硬编码某一具体 transport 的超时变体。
- **额外问题**：`UsbTimeout` 是 `#[cfg(feature = "usb")]` 门控的（error.rs:93-96）。把"超时"概念绑死在
  usb 特性上本身就是错的——TCP（可不依赖 usb 特性）需要一个非门控的超时变体。

## 为什么不选方案 A（reader 端补认 IOError(TimedOut)）

A 只是把两种具体超时形态都列进消费者 match，治标不治本：超时"真相"仍分散在各 transport，消费者仍需
知道每种 transport 的超时长相，第三种 transport（emulator/proxy/mDNS…）来了照样漏；且
`IOError(TimedOut)` 语义有歧义（底层 socket 因别的原因也可能抛 TimedOut）。B 在契约层单一来源，
新增 transport 时由契约引导返回正确变体，消费者永不需改 —— 与"长远维护"目标一致。

## Requirements

- [R1] 新增 transport 中立、**不特性门控**的 `RustADBError::ReadTimeout`（命名待定，见 Q1），
       表示"读操作在给定 timeout 内未拿到完整消息"。
- [R2] `ADBMessageTransport` trait 文档明确约定：`read_message_with_timeout` 超时 **必须** 返回该变体
       （把隐式契约写成显式契约）。
- [R3] TcpTransport 超时路径（tcp_transport.rs 读超时）返回新变体而非 `IOError(TimedOut)`。
- [R4] USBTransport 超时路径（`map_transfer_status`: `TransferError::Cancelled`）返回新变体。
- [R5] persistent reader（persistent.rs:1147）只 match 新变体归类为 `ReadStep::ReadTimeout`；
       该匹配臂不再依赖 usb 特性。
- [R6] 处理 `UsbTimeout` 的去留（见 Q2），并同步所有受影响的消费点与断言测试
       （usb_transport.rs:543-544 的映射断言等）。

## Open Questions (resolved)

- [Q1] ~~命名 + 写超时范围~~ → **已定**：新变体命名 `RustADBError::ReadTimeout`（与 `ReadStep::ReadTimeout`
       呼应）。**仅统一读超时**；写超时（TCP `IOError(TimedOut)` "TCP write timed out"，
       tcp_transport.rs:87-90）**不纳入本任务**，留作后续（语义与致命性和 read 不同，当前未暴露 bug）。
- [Q2] ~~`UsbTimeout` 去留~~ → **已定：废弃并移除** `UsbTimeout`，USB/TCP 全部收敛到 `ReadTimeout`
       （单一概念）。移除 error enum 变体属 **breaking change**，记入 semver（major 或明确记录）。
       须确认 `UsbTimeout` 无其它语义独特消费点（grep 显示仅 reader + usb_transport 映射 + 断言测试）。

## Acceptance Criteria

- [ ] reader loop 在 TCP 空闲超时下 `continue`（不再 fatal、不拆连接）；USB 行为不回归。
- [ ] 新变体非 `#[cfg(feature="usb")]` 门控；纯 TCP（无 usb 特性）构建下 reader 超时匹配臂可编译且正确。
- [ ] trait 文档写明超时返回契约。
- [ ] USB 超时映射的现有断言测试（usb_transport.rs:543-544 等）更新且通过。
- [ ] 新增单测：模拟 transport 超时 → reader 归类为 ReadTimeout → continue（不 break）。
- [ ] `cargo clippy --all-targets -- -D warnings`（default + usb/server/tcp 各特性组合）+ `cargo test` 全绿。
- [ ] 真机 selftest `tcpip.shell_through_tcp_device` 通过（手动验收项）。

## Definition of Done

- 契约统一 + 各 transport 实现 + reader 消费点 + 测试同步。
- 多特性组合编译/lint/test 绿（注意 usb / 非 usb、tcp、server 组合）。
- trait 契约与（若有）error 变体语义沉淀进 spec（error-handling / adb-wire-protocol 相关）。
- 语义版本：error enum 变更——若移除 `UsbTimeout` 属 breaking（评估并记录）。

## Out of Scope

- expose-tcp / host-serial 两个已完成任务的改动。
- reader fatal/recoverable 分类的其它重构（只统一超时这一类）。
- 写超时统一 —— 若 Q1 决定不纳入，则明确 out of scope。

## Technical Notes

- 关键文件：
  - `adboost/src/error.rs:93-96`（`UsbTimeout` 定义，特性门控）
  - `adboost/src/message_devices/adb_message_transport.rs:36`（trait 超时方法，契约缺失处）
  - `adboost/src/message_devices/tcp/tcp_transport.rs:50-92`（TCP read/write 超时返回 IOError(TimedOut)）
  - `adboost/src/message_devices/usb/usb_transport.rs:270-282`（`map_transfer_status`: Cancelled→UsbTimeout）+ :539-544（映射断言测试）
  - `adboost/src/message_devices/usb/persistent.rs:1130-1148`（read_step 分类）+ :975-1012（ReadStep 消费/fatal 判定）
- grep `UsbTimeout` 全量消费点：error.rs / persistent.rs:1147 / usb_transport.rs(映射+测试)。面可控。
- spec：error-handling.md（RustADBError 约定）、adb-wire-protocol-contract.md（如涉及超时与协议语义）。
- 真机验证链：解析 bug（已修，commit a80dfd0）之后暴露此超时 bug；修复后整条 TCP shell 链路应端到端打通。
