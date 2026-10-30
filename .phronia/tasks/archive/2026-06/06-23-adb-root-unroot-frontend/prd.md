# adb root 重连握手 & unroot 支持（frontend 服务覆盖）

## Goal

补齐 adboost server frontend 的服务覆盖面，让 `adb root` 之后的标准重连握手以及 `adb unroot` 都能被 frontend 正确接住——backend（xdb）已能透传任意 `Raw`，问题全部在 frontend 路由/白名单层。

## What I already know (verified against code @ 04c0e46)

外部需求方：xdb（启用 adboost `server` feature，自定义 `DeviceBackend` 透传 `Raw`）。
现场设备有两个 device（`YTGUSCNFMFAIK7ZP` + 合成 `..._hyp`），故强制 `-s`，client 走 transport-id 路径。

四个缺口，逐条核对属实：

- **1a 顶层路由缺前缀** — `dispatch_host_service` (frontend.rs:263-293) 识别 `host-serial:`/`host-usb:`/`host-local:`/`host:`，**无 `host-transport-id:`** → `host-transport-id:1:...` 落到 `unknown service`。
- **1b sub-service 缺 wait-for** — `dispatch_host_serial` (frontend.rs:408-459) sub 只有 get-state/get-serialno/features/list-forward/killforward-all/forward*/transport*/tport，**无 `wait-for-*`** → `unknown host-serial sub-service`。
- **1c disconnect 状态不支持** — `serve_wait_for` (frontend.rs:559) 仅当 `state == "device"` 处理，其余 FAIL；且当前只按 transport *kind* 轮询，无 pinned serial 概念。
- **2 unroot 不在白名单** — `is_control_service` (frontend.rs:1099-1105) 缺 `"unroot:"` → `local_service` (frontend.rs:1019) 回 `service not supported: unroot:`，到不了 backend。grep 全仓 `unroot` 零命中。

## Requirements (evolving)

### 模块 A：unroot 完整镜像 root（client 库 API 对称 + frontend 透传）
- [R1] `ADBLocalCommand` 新增 `Unroot` 变体，Display => `"unroot:"`（与 `Root => "root:"` 对称，adb_local_command.rs）。
- [R2] `ADBDeviceExt` trait 新增 `async fn unroot()`（adb_device_ext.rs，紧邻 `root` 声明）。
- [R3] 镜像 root 的 6 处实现：
  - `proxy/device_commands/` 新增 `unroot.rs`（`ADBProxyDevice::unroot`，照搬 root.rs，`proxy_connection(Unroot)`）。
  - `message_devices/commands/` 新增 `unroot.rs`（`ADBMessageDevice::unroot`，照搬 root.rs，`open_session(Unroot)` + assert Okay）。
  - `proxy/adb_proxy_device_commands.rs`、`message_devices/adb_message_device_commands.rs` 各加 trait 转发实现。
  - `usb/adb_usb_device.rs`、`tcp/adb_tcp_device.rs` 各加 `self.inner.unroot()` 派发。
  - 两个 `commands` / `device_commands` 的 `mod.rs` 注册 `unroot` 模块。
- [R4] `is_control_service` 加入 `"unroot:"`，使 frontend 把 client 发来的 `unroot:` 转为 `Raw("unroot:")` 透传（xdb 路径，独立于枚举）。

### 模块 B：adb root 重连握手（frontend 服务覆盖）
- [R5] 顶层 `dispatch_host_service` 新增 `host-transport-id:<N>:<sub>` 前缀路由：解析 N→serial（复用 `serial_for_transport_id`），再走 `dispatch_host_serial`（与 `dispatch_host_kind` 对称范式）。N 无效/无对应设备 → FAIL，对齐既有 transport-id 措辞。
- [R6] `dispatch_host_serial` 把 `wait-for-*` sub-service 路由到 `serve_wait_for`，并把 **pinned serial** 传入（与顶层 `host:wait-for-*` 共用实现，但顶层无 pinned serial）。
- [R7] `serve_wait_for` 支持 `disconnect` 状态：等待 pinned serial 从 `list_devices()` 消失后回第二个 bare OKAY；复用现有 60s `MAX_WAIT` 兜底 + 200ms 轮询。非 disconnect 状态维持现有 kind 轮询。

## Decision (ADR-lite)

**Q1 — disconnect 超时**：复用现有 60s `MAX_WAIT` 兜底，而非 native 的"无限等待"。
- Context：native server 永不超时、靠 poll client fd 退出；adboost `serve_wait_for` 是轮询式（200ms poll `list_devices()`），不监听 client socket 关闭。
- Decision：disconnect 路径沿用 60s 上界。adbd restart 后设备几秒内从 list 消失即恢复；超时仅命中"root 未真正重启"边缘，FAIL 是合理且无连接泄漏的安全网。
- Consequences：与 native 行为有界差异（60s vs ∞），但避免无限挂死的轮询任务泄漏。若日后引入 client-fd 感知，可再放宽。

**Q2 — unroot 粒度**：完整镜像 root 至所有层（trait + 6 处实现 + 枚举 + 白名单）。
- Context：root 是贯穿 trait/proxy/message/usb/tcp 的完整能力；仅加枚举变体会留下无人构造的悬空变体（frontend 走 Raw 不碰枚举）。
- Decision：unroot 与 root 完全平行，client 库获得 `device.unroot()`，同时 frontend Raw 透传满足 xdb。
- Consequences：改动 ~8 文件但零悬空代码，长期对称可维护。

## Key Research Correction

xdb 报告的 `host:transport-id:N:wait-for-...` 线格式有误。AOSP `format_host_command` 实际发 **`host-transport-id:<N>:wait-for-...`**（顶层 family 前缀，无 `host:`）。故 R5 路由的是顶层 `host-transport-id:` 前缀，与 `host-usb:`/`host-local:` 同级。双设备 `-s` 场景下 client 发完 disconnect 等待即结束（AOSP `previous_id != 0` 跳过后续 wait-for-device）。

## Acceptance Criteria (evolving)

- [ ] `adb -s <serial> unroot` 不再 `service not supported`，透传到设备（frontend Raw 路径）。
- [ ] `adb -s <serial> root` 全程无 `unknown service` / `unknown host-serial sub-service` 报错。
- [ ] `device.unroot()` 作为公开 client API 可用（与 `device.root()` 对称，USB/TCP/Proxy 三态都可调）。
- [ ] `ADBLocalCommand::Unroot.to_string() == "unroot:"`。
- [ ] 单测覆盖：host-transport-id 路由、wait-for sub 路由、disconnect 状态（pinned serial 消失后 OKAY）、unroot 白名单、Unroot Display。
- [ ] lint / clippy / 既有测试全绿。

## Definition of Done

- 单测补齐（frontend dispatch 表已有大量 `#[tokio::test]` 范式可循）。
- lint / clippy / test 绿。
- 行为变更点在代码注释中说明 AOSP 对齐依据。

## Out of Scope

- 不改 xdb。
- `ADBLocalCommand::Unroot` 枚举变体为可选（走 Raw 已足够）——默认不做，除非用户要求对称。

## Technical Notes

- 关键文件：`adboost/src/server/frontend.rs`，控制服务枚举 `adboost/src/models/adb_local_command.rs`。
- 既有对称范式：`dispatch_host_kind`（host-usb/host-local 复用 dispatch_host_serial）可直接照搬给 host-transport-id。
- `serial_for_transport_id` (frontend.rs:857) 已存在，可直接复用做 N→serial 解析。

## Research References

- (pending) `research/aosp-wait-for-disconnect.md`
