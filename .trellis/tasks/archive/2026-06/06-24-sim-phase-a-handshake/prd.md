# Sim harness Phase A — handshake state machine + mocks scaffold

> 父任务：[`../06-24-simulateddevice-software-adb-test-harness/prd.md`](../06-24-simulateddevice-software-adb-test-harness/prd.md)
> 研究依据：父任务 `research/{escaped-bug-history,protocol-state-edges,parity-bug-classes}.md`

## Goal

搭起整个 sim harness 的地基，并把**握手层**的所有边界穷尽式跑通。本阶段交付两个 mock 的骨架 + 门控 feature，验证 `PersistentConnection<T>` 的传输 seam 在一个有状态 adbd 模型下端到端可用——这是 B/C 的前提。完成后替换现有 3 个 `ScriptedTransport` 测试，且不损失覆盖。

## Scope（本阶段做什么）

- 新模块 `adboost/src/message_devices/sim/`，门控 `#[cfg(any(test, feature = "test-support"))]`；新增 `test-support` cargo feature（不进 default，不引入新 default 依赖）。
- **`SimulatedDevice`**（帧层，impl `ADBTransport` + `ADBMessageTransport`）：
  - 单个 `Arc<Mutex<SimState>>`，在 reader/writer 两个 clone 间共享；锁绝不跨 `.await`（对齐 `ScriptedTransport`，`persistent.rs:2861`）。
  - `react_to(msg)` 握手状态机：CNXN→CNXN(banner)|AUTH(TOKEN)、AUTH(SIGNATURE/RSAPUBLICKEY)→CNXN、可选 STLS 应答。
  - 出站队列 `VecDeque<ADBTransportMessage>`；`read_message` 队空时返回 `RustADBError::ReadTimeout`（idle≠故障契约）。
- **`DeviceProfile`**：banner/版本轴。预设 `android_11()`（legacy 版本→delayed_ack 协商为 false）、`android_16()`（全功能→windowed）、`unauthorized()`。让 `negotiate_delayed_ack`（`persistent.rs:483`）+ `DeviceFeatureSet::from_banner`（`:639`）真实跑起来。
- **`Scenario`**：握手相关故障注入——`transient_writes(n, err)`（NotResponding / Disconnected / 持续）、`stale_clse_then_cnxn(n)`、`die_after_reads(n)`（→ reader 致命错误，为 B/C 的死亡 seam 铺路）。
- **`ChunkedTransport` 骨架**：字节层 mock 的类型与 `ADBMessageTransport` impl 落地（具体故障场景 B4/B5/B7/B9 在 Phase B 填充），本阶段只确保它能完成一次正常握手，证明字节层 seam 成立。
- **死亡信号 seam（最小）**：`die_after_reads` → reader 致命 break → `is_alive()==false`（`persistent.rs:1952`）。完整 `TransportReset` 发布留给 Phase C。

## 必须穷尽的边界（来自 protocol-state-edges.md）

经由完整 `PersistentConnection::new` / `do_connect` 端到端覆盖：
- **CNXN**：CNXN-1..13（banner 解析、arg0 版本、host CNXN 版本门控与 arg1=1MiB、畸形/截断 banner、错误响应 cmd、写/读瞬态重试 NotResponding/Disconnected、超出在位预算 fail-fast、永久死句柄反放大、非瞬态快速失败、预算分离不变量）。
- **stale-CLSE drain**：DRAIN-1..5（单条/突发 stale、64 帧上限、握手前 drain、被 stale 耗尽 CNXN 预算）。
- **AUTH**：AUTH-1..6（TOKEN→SIGNATURE→CNXN、走 RSAPUBLICKEY、type≠TOKEN、未授权设备 10s 边界、签名载荷即 token、pubkey 末尾 NUL）。
- **delayed_ack 协商**：DACK-1..7（双端+版本→启用、legacy→禁用、banner 无 feature→禁用、本地 opt-out、阈值之上、子串假匹配防护、端到端 OPEN arg1 grant 捕获）。
- 超时相关一律用 `#[tokio::test(start_paused = true)]`。

## 守住的逃逸 bug（命名回归测试）

- **B1** delayed_ack 版本门（windowed OPEN at legacy 被 adbd 忽略）。
- **B2** data_check=0 帧被接受（magic-only 完整性）。
- **B-feat**（部分，banner 侧）：不同 banner 的 profile 驱动 per-device 协商（完整 server 侧在 C）。

## Acceptance Criteria

- [ ] `SimulatedDevice` / `ChunkedTransport` 均 impl `ADBTransport` + `ADBMessageTransport`，复用既有 framing/negotiation 助手，零重实现；状态在两个 clone 间共享，锁不跨 `.await`。
- [ ] 队空 `read_message` 返回 `RustADBError::ReadTimeout`，驱动 reader 的 `ReadStep::ReadTimeout => continue`。
- [ ] 上述 CNXN/DRAIN/AUTH/DACK 全部边界各有端到端测试。
- [ ] B1/B2 各有命名回归测试。
- [ ] 现有 3 个 `ScriptedTransport` 测试被 `SimulatedDevice` 等价替换，覆盖不减。
- [ ] `cargo test`（default 与 `--features test-support`）+ `cargo clippy --all-targets` 全绿；无新增 default 依赖。

## Out of Scope（留给 B/C）

- session / OPEN / OKAY / WRTE / CLSE / 流控字节流 → Phase B。
- `ChunkedTransport` 的故障场景（半帧/过量投递/写到第 k 字节失败）→ Phase B。
- `SimDeviceBackend` / `SimRegistry` / `TransportReset` / 前端 → Phase C。

## Technical Notes

- 传输被 clone 成 bulk-IN/bulk-OUT 两半（`persistent.rs:649`）：host→device 的 OPEN/OKAY/WRTE/CLSE 经 writer clone 的 `write_message` 进入共享状态，reader clone 的 `read_message` 据此应答。
- 模块文档写明诚实边界（见父 PRD「Honest boundaries」）。
