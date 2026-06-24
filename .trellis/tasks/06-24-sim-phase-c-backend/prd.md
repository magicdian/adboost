# Sim harness Phase C — SimDeviceBackend + 前端重连/对齐

> 父任务：[`../06-24-simulateddevice-software-adb-test-harness/prd.md`](../06-24-simulateddevice-software-adb-test-harness/prd.md)
> 前置：Phase A + B 合入——复用 `SimulatedDevice`（完整 session）+ `Scenario` 死亡注入。

## Goal

用 `SimDeviceBackend` + `SimRegistry` 通过已经与传输无关的 `DeviceBackend` trait（`server/backend.rs:220`）端到端驱动 smartsocket 前端，把今天**只能插 MTK 手机 + 手动 `adb root` 循环**复现的重连/再枚举 bug，以及 host 协议对齐，变成确定性测试。需 `server` + `test-support`。

关键缺口（来自 parity 研究）：现有 90 个前端测试跑在 `MockBackend` 上，其 `open_local_service` 是 `unimplemented!()`（`frontend.rs:1611`），`LifecycleEvent` 是手工喂入（`:1821`）——**session 桥接路径**与**真实死亡→事件发射**今天都没测。本阶段正是补这两块。

## Scope

- **`SimDeviceBackend`**（impl `DeviceBackend` trait，新文件 `server/sim_backend.rs`，门控 `#[cfg(any(test, feature = "test-support"))]`）：
  - `list_devices`/`subscribe_changes`：由 `SimRegistry` 提供可编程设备集（带 `TransportKind::Usb`/`Local`、`DeviceState`、`capabilities` 来自 `DeviceProfile` banner）。
  - `open_local_service` → sim 支撑的 `MultiplexedSession`（真实桥接，补上今天的 `unimplemented!()` 缺口）。
  - `transport_alive`（`backend.rs:278`）读 sim 连接的 `is_alive()`；`subscribe_lifecycle` 在连接真实死亡时发 `TransportReset`（非 `Disconnected`——规则保留）。
  - `device_capabilities`：按设备 banner 做 per-device `host:features` 协商。
- **`SimRegistry`**：`checkout()` 每次铸全新 `SimulatedDevice`（重枚举模型：旧 handle 永久死，只有重开能恢复）；`restart()` 翻转当前设备 dead——背靠背 root/unroot 的字面建模；增删设备驱动 `track-devices` 快照。

## 必须覆盖的场景（来自 parity-bug-classes.md + escaped-bug-history.md）

- **host 协议对齐**（class 2，positive 路径几乎全可复现）：
  - `host-usb:`/`transport-usb`（`adb -d`）只选 USB-kind 设备，`host-local:`/`transport-local`（`-e`）选另一个；`resolve_single_by_kind`（`frontend.rs:795`）。
  - transport-id 分配 + `tport` 8 字节 LE 回包。
  - 按设备 `host:features` 诚实协商（不向 feature-less 设备 over-advertise→`shell,v2` 被 CLSE，**B-feat** 完整 server 侧）。
  - `host:devices`/`devices-l` body 与 state 串；`host:track-devices` 变更快照流。
  - 未知 service → 干净 FAIL（不 hang/panic）。
- **重连 / 再枚举集群**（class 死亡 seam，今天全靠真机）：
  - **B10** `wait-for-*` 回两个 OKAY（accept + satisfied），非单 OKAY 的 `protocol fault`。
  - **B11** wait-for-disconnect 由 `TransportReset` 事件驱动，**非** presence 轮询（MTK adbd 重启不离开枚举也能 sub-second 解除，非 60s 挂死）。
  - **B12/B15**（反应层，非 OS 层）：连接死亡后旧 handle 永久死，`SimRegistry::checkout` 重铸新设备→重开成功（15/15 在位失败后首次重开成功的字面复现）；reopen 层 reaction 可测，IOKit 新 registry id 本身不测（诚实边界）。
  - **B14** 两级重试预算串联：内层 CNXN 耗尽返回 `ADBRequestFailed` 被外层 `is_retryable_open_error` 认作可重试→重开。
  - Disconnect 释放 forward/reverse 规则 vs restart（`TransportReset`）保留规则。

## 守住的逃逸 bug（命名回归测试）

B10、B11、B14，加 `back_to_back_root_unroot_recovers_via_reopen`、`wait_for_disconnect_unblocks_on_reader_death`、B-feat（server 侧 per-device 门控）。

## Acceptance Criteria

- [ ] `SimDeviceBackend` impl `DeviceBackend` trait（不 fork `DefaultDeviceBackend`）；`open_local_service` 返回 sim 支撑的真实 `MultiplexedSession`，桥接路径端到端被测。
- [ ] 真实连接死亡发射 `TransportReset`（而非手工喂事件），`wait-for-disconnect` sub-second 解除且 forward/reverse 规则保留。
- [ ] host-usb/transport-usb `-d`/`-e` 选择、tport、per-device features、devices/track-devices 各有端到端断言（对齐 AOSP 线节）。
- [ ] `SimRegistry::checkout`/`restart` 复现背靠背 root/unroot：旧 handle 死、重开成功。
- [ ] B10/B11/B14/B-feat + 两个重连测试各有命名回归测试。
- [ ] `cargo test --features server,test-support` + `cargo clippy --all-targets --features server,test-support` 全绿。

## Out of Scope（诚实边界）

- 真实 socket connect / TLS（`host:connect` 真实建连仍归 `DefaultDeviceBackend`/硬件）。
- 发现**未实现**的 native adb 前缀（需对 real adb 二进制 diff，sim 够不到）。
- IOKit 重枚举到新 registry id 本身（OS artifact，只测 reopen 反应）。

## Technical Notes

- `DeviceBackend` 已是传输无关（session 方法返回 `MultiplexedSession`/`SyncSession`），这是直接 impl 的前提；只有 `DefaultDeviceBackend::get_or_open`（`default_backend.rs:371`）锁死 USB。
- `TransportReset` vs `Disconnected` 的规则保留语义见 `backend.rs:135-156`、`serve_wait_for`（`frontend.rs:636`）。
- 既有 `round_trip`/`round_trip_select`/`round_trip_tport` 测试 harness（`frontend.rs:1666+`）是 wire 断言的先例，可沿用其风格。
