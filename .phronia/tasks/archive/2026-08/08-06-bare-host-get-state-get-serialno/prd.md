# 支持裸 `host:get-state` / `host:get-serialno`（transport-any 单设备数据查询）

## Goal

adboost 作为 `:5037` adb server 前端时，裸 `host:get-state` / `host:get-serialno`
（无 `host-serial:` 前缀）目前落入 `dispatch_host_service` 的兜底臂 `other =>`，
返回 `FAIL unknown host service: get-state`，导致依赖它的客户端（AOSP `adb root`
/`unroot` 在发 `root:`/`unroot:` 前先调 `adb_get_state()`）整体中止。

目标：让裸 `get-state`/`get-serialno` 按 **transport-any 语义**回复 —— 解析
当前唯一设备（0/多则回 AOSP 措辞的 `FAIL`），并与其带前缀形式（
`host-serial:<serial>:get-state`）**字节一致**，从而解锁 `adb root`/`unroot`。

下游：xdb（xpeng-debug-bridge）在 bump commit rev 后依赖此修复完成真机验证。

## Requirements

- 裸 `host:get-state` / `host:get-serialno` 按 transport-any 解析（复用
  `resolve_single_serial`，即 `resolve_single_by_kind(None)`）：
  - 0 设备 → `FAIL("no devices/emulators found")`（`no_devices_msg(None)`）
  - 多设备 → `FAIL("more than one device/emulator")`（`ambiguous_msg(None)`）
  - 单设备 → 复用 `dispatch_host_serial` 的既有 `get-state`/`get-serialno` 实现，
    保证与带前缀形式 byte 一致。
- 回复 framing：`OKAY` + `%04x`+payload（`okay_data` / `reply_or_overflow`），
  与 `host_data_query_payload` 家族（version/features/devices/devices-l）相同。

## Acceptance Criteria

- [ ] 单设备：`host:get-state` → `OKAY0006device`；`host:get-serialno` → `OKAY`+序列号
- [ ] 零设备：`host:get-state` → `FAIL` + `no devices/emulators found`
- [ ] 多设备：`host:get-state` → `FAIL` + `more than one device/emulator`
- [ ] 裸 vs 带前缀一致性：单设备下 `host:get-state` 与 `host-serial:<serial>:get-state` 字节一致
- [ ] 编译 + clippy + `--features server,usb` 测试全绿

## Definition of Done

- Tests added/updated（frontend.rs `round_trip`，参照
  `host_transport_id_routes_to_dispatch_host_serial` 模板）
- Lint / typecheck / CI green
- `.trellis/spec/backend/server-host-protocol.md` 补裸 transport-any 数据查询条目
- 下游 rev bump 说明并入交接记录

## Technical Approach

扩展 **`host_data_query_payload`**（frontend.rs:390），把 `get-state` /
`get-serialno` 收进既有"裸 host 数据查询"统一分派点，而非在
`dispatch_host_service` 的 `match svc` 里加三个路由 arm。行为：

- 它们形态上就是"host 数据查询"（`OKAY`+帧，无 routing），与
  `version/features/devices/devices-l` 同类；归入同一个分派点保持
  "裸数据查询只有一个入口"。
- transport-any 解析复用 `resolve_single_serial()`（与 forward、
  transport-any 一致），AOSP 措辞自动对齐。
- 单设备 payload 复用 `dispatch_host_serial` 既有实现 → 裸/带前缀 byte 一致。
- **签名泛化**：`host_data_query_payload` 返回类型改为
  `Option<Result<String, String>>` —— `None`=非数据查询、`Ok(payload)`=单设备、
  `Err(reason)`=AOSP FAIL 措辞（0/多设备）。消费处（`dispatch_host_service:297`）
  对 `Err` 写 `protocol::fail(reason)`、`Ok` 写 `okay_data`。

## Decision (ADR-lite)

**Context**: 报告方建议在 `dispatch_host_service::match svc` 加 3 个 arm。
用户拒绝"单点修复"，要求从项目整体架构（单一 funnel + 数据查询单一分派点、
诚实能力）出发。

**Decision**: 在 `host_data_query_payload` 扩展 `get-state`/`get-serialno`
（transport-any → `dispatch_host_serial`）。**不**在 `match svc` 加路由 arm。
**不实现 `get-devpath`**。

**Consequences**: 
- 裸数据查询保持单一入口（`host_data_query_payload`），future 裸单设备查询
  落同一个点。
- `get-devpath` 不实现：`DeviceEntry` 无 devpath 字段，诚实能力原则下
  （"绝不承诺它不达"）不硬回。如需，另开特性加字段，而非塞进本修复
  "堵报错"。

## Out of Scope

- `host:get-devpath`（需给 `DeviceEntry` 加真实字段，属独立特性）
- xdb 侧 `ReverseEngine` 连接-已死缓存重建（附录 §7，纯 xdb 修复）
- 其他未实现 WOULD-be 裸单查询

## Technical Notes

- `dispatch_host_service` (frontend.rs:274-385)；`host_data_query_payload`
  (390-398)；`resolve_single_serial` (823-827)；
  `dispatch_host_serial` get-state/get-serial (455-465)；
  `dispatch_host_kind`/`dispatch_host_transport_id` (524-572) 是既有
  "resolve→funnel" 模板。
- spec：`.trellis/spec/backend/server-host-protocol.md`（transport-selection
  funnel 契约、`host:features` 双轴、`host:connect` 数据查询 framing）。
- 测试参照 `host_transport_id_routes_to_dispatch_host_serial` (frontend.rs:2832-2842)。