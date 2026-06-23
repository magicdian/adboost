# USB/transport 断开时自动释放 forward/reverse 规则（可配置策略 + 回调）

## Goal

调用方反馈：以 server 模式运行（USB 设备），配置 `adb forward` 后**拔出 USB**，`forward --list` 中的规则仍然存在；而标准 adb 会在设备断开时自动释放该设备的 forward/reverse。adboost 当前缺少"断开 → 释放"这条接缝。

目标：让 adboost 在 transport（USB 拔出 / TCP 断开）消失时，**默认对齐标准 adb——自动释放该设备的 forward 与 reverse 规则**，同时按"标准默认 + opt-in 定制"理念，允许调用方 opt-out（保留规则自管）或注册回调自主决定释放时机。要从完整架构长远维护考虑，不做最小化实现。

## What I already know（代码现状）

**不对称是根因（又一例 tcp-async-path-missing-usb-guarantees 类问题，但这里是 forward vs reverse 的不对称）：**

- **Forward（host 发起）**：注册表 `ForwardRegistry`（`server/forward.rs:88`）是**服务器全局**，`Arc` 挂在 `AdbServerFrontend`（`frontend.rs:66`），**不绑定任何设备/transport 生命周期**。方法 `insert/remove/remove_all/list` 全为 `pub(super)`，**无对外清理 API**。`ForwardRule` 已存 `serial`（`forward.rs:77-82`），但**没有 `remove_by_serial`**。唯一清理途径：客户端显式 `killforward[-all]`，或 server `shutdown()`。
- **Reverse（device 发起）**：`ReverseEngine` 按 serial 存于 backend（`default_backend.rs:57`），生命周期绑 `PersistentUsbConnection`；连接死后 pump task 因 channel 关闭自然退出（`reverse_engine.rs`），但 `reverse` map 里的条目要到 `shutdown()` 才 `clear()`（`default_backend.rs:191`）。有 trait 方法：`open_reverse/reverse_remove/reverse_remove_all/list_reverse`（`backend.rs:246-280`）。

**断开检测位置（在 backend，不在 frontend）：**
- nusb hotplug watch 在 `subscribe_changes()`（`default_backend.rs:262`），但它**只服务 `host:track-devices` 客户端**——没有客户端订阅就没有内部断开信号；不能直接复用为生命周期清理源。
- stale 连接检测是**被动惰性**的：`get_or_open()`（`default_backend.rs:219`）和 `tcp_conn()`（`208`）在下次访问时用 `conn.is_alive()` 发现死连接并移除。

**架构矛盾**：断开信号在 backend 侧产生，但 forward 注册表在 frontend 侧。需要一条新的、独立于 `track-devices` 的"设备断开"事件通路，把 backend 的检测送达 frontend 的 forward 注册表（reverse 在 backend 内可自清）。

**现有 builder/policy 风格可对齐：**
- `AdbServerFrontendBuilder`：`.addr() / .capabilities()`（`frontend.rs:24-55`）。
- `ReversePolicy` 枚举（`usb/reverse_policy.rs`）：`RejectUnconfigured(default) / AllowAll / Custom(Arc<dyn Fn>)` —— **正是"枚举 + 回调"范式的现成样板**，新策略应模仿它。
- `DefaultDeviceBackend::with_reverse_policy()`（`default_backend.rs:100`）。

## Decisions（已确认）

1. **默认行为 = 断开即释放**（对齐标准 adb），可 opt-out。
2. **配置粒度 = 枚举策略 + 回调**：`OnDisconnect::ReleaseAll`（默认）/ `Retain` / `Notify(callback)`。
3. **走完整架构设计**，不做最小化实现；从长远维护考虑。

## Decision (ADR-lite) — 断开事件通路

**Context**：断开检测在 backend（hotplug watch + stale 检测），forward 注册表在 frontend；`host:track-devices` 那条流依赖外部客户端订阅，不能复用为内部生命周期信号。
**Decision**：采用**方案 1** —— 在 `DeviceBackend` trait 上扩展一条**独立的设备生命周期事件流**（如 `subscribe_lifecycle()`，发 `DeviceDisconnected(serial)`），独立于 `track-devices`。backend 是设备生命周期真相源，主动驱动 watch task；frontend 订阅并作为消费者按策略释放 forward，reverse 通过既有 `reverse_remove_all` 清理。
**Consequences**：trait 新增方法（带 default 实现保兼容）；backend 需主动跑一个常驻 watch，而非依赖外部订阅者；与现有 `subscribe_changes` 对称，自定义 backend 也能参与。

## Decision (ADR-lite) — 策略放置 + 主动清理 API

**Context**：forward 注册表归 frontend；reverse 在 backend。需要一个集中的策略决策点，并给外部调用方主动清理入口（现有 `ForwardRegistry` 全 `pub(super)`，外部够不到）。
**Decision**：
- (a) 策略配置入口放 **`AdbServerFrontendBuilder`**：`.on_disconnect(OnDisconnect::...)`。frontend 收到断开事件后统一释放 forward（注册表）+ 调 `backend.reverse_remove_all(serial)`，决策点集中一处。
- (b) **暴露主动清理 API**：在 frontend（或其共享句柄）上提供**单独清理**与**全部清理**两个公开方法（覆盖该 serial 的 forward + reverse，以及全量）。Retain 策略下调用方用它自管，Notify 回调里也调它。
**Consequences**：需要一个能在 `serve()`（消费 self）之后仍可被外部持有/调用的句柄来挂主动清理 API（见 Q4）。

## Decision (ADR-lite) — 回调语义 + 句柄形态

**Context**：Notify 需要明确职责边界；`serve(mut self)` 消费 self，外部需独立句柄才能调主动清理 API。
**Decision**：
- (Q3) `OnDisconnect::Notify(callback)` **纯通知**：签名 `Fn(&str /*serial*/) + Send + Sync`（包 `Arc`，仿 `ReversePolicy::Custom`）。adboost 不替它清理，回调自行调主动清理 API 或选择不清。
- (Q4) 引入轻量 **`ForwardHandle`**（克隆友好，内部 `Arc<ForwardRegistry>` + `Arc<dyn DeviceBackend>` 或等价）。frontend 在 `serve()` 前可 `.handle()` 取出，`serve()` 之后句柄仍有效。方法 `release(serial)` / `release_all()`，覆盖 forward + reverse。Notify 回调可捕获此句柄。
**Consequences**：`OnDisconnect` 枚举三分支（ReleaseAll/Retain/Notify(Arc<dyn Fn>)）+ Debug 手写（仿 `reverse_policy.rs`）。`ForwardHandle` 成为对外公开类型，需文档化。

## Decision (ADR-lite) — forward/reverse 统一语义

**Context**：reverse 连接死后 pump 自然退出（数据通路已停），残留的只是 `reverse` map 条目（list 显示 + 内存），不像 forward 残留还占 host TCP 端口。
**Decision**：**统一语义**——单个 `OnDisconnect` 策略同时管 forward 与 reverse。ReleaseAll：两者都清；Retain：两者都留（reverse map 条目保留，list 仍显示）；Notify：只通知，两者都不动。`ForwardHandle.release()/release_all()` 同时清两者。
**Consequences**：心智模型最简单，与"对齐标准 adb（设备走了，转发都没了）"一致。release 路径需调 `backend.reverse_remove_all(serial)` + 从 backend 的 `reverse` map 移除条目（需补一个 backend 入口让 frontend 触发 per-serial reverse 清理，而不止 shutdown 时 clear）。

## Technical Approach（汇总）

1. **新增 `OnDisconnect` 策略类型**（新文件，仿 `usb/reverse_policy.rs` 风格）：枚举 `ReleaseAll(default) / Retain / Notify(Arc<dyn Fn(&str)+Send+Sync>)` + 手写 Debug。
2. **`DeviceBackend` trait 扩展生命周期事件流**：`subscribe_lifecycle()` 发 `DeviceDisconnected(serial)`，default 实现返回空/立即关闭流以保兼容；`DefaultDeviceBackend` 用 nusb hotplug + stale 检测驱动一个常驻 watch，diff 出消失 serial 后发事件。同时补 per-serial reverse 清理入口。
3. **`ForwardRegistry` 补 `remove_by_serial`**（`forward.rs`），按 serial 批量 abort listener。
4. **`ForwardHandle`**（Arc 内核公开类型）：`release(serial)` / `release_all()`，统一清 forward + reverse。
5. **frontend** 订阅 `subscribe_lifecycle()`，在常驻 task 中按 `OnDisconnect` 策略处理断开事件；`AdbServerFrontendBuilder.on_disconnect(...)` 配置；`frontend.handle() -> ForwardHandle`。
6. **测试**：单元（策略分支、remove_by_serial、handle）+ 契约测试（USB/TCP 两路径断开后规则状态一致）。
7. **文档**：默认行为变更说明、配置入口、对 xdb 的迁移提示。

## Implementation Plan (small PRs / 一 bug 一任务 的精神拆 commit)

* **PR1（脚手架 + 契约）**：`OnDisconnect` 类型 + `ForwardRegistry::remove_by_serial` + `ForwardHandle` 骨架 + 单元测试。无行为接线。
* **PR2（事件流接缝）**：`DeviceBackend::subscribe_lifecycle` trait 方法（default 兼容）+ `DefaultDeviceBackend` 驱动 watch 发断开事件 + per-serial reverse 清理入口。
* **PR3（接线 + 默认行为）**：frontend 订阅事件、按策略释放；builder `.on_disconnect`；`frontend.handle()`。契约测试覆盖 USB/TCP。
* **PR4（文档 + 迁移）**：行为变更文档、spec 更新（server-host-protocol.md 补 forward 生命周期契约）。
* **PR5（selftest 端到端）**：adboost_cli interactive 阶段新增 `case_usb_forward_release_on_unplug`——官方 `adb -P` 驱动 in-process server 注册 forward，拔出 USB 后断言 `forward --list` 该 serial 规则自动消失。这是契约测试（mock 事件）触不到的真实 hotplug 路径（nusb watch→diff→事件→释放），直接复现调用方报告的现象。独立 case，复用 `wait_for_absence`；无 adb/无设备 → Skipped；遵守 reboot-last 排序不变量。

## Requirements (evolving)

* transport 断开（USB 拔出 / TCP 断开 / 连接 reader 死亡）时，默认自动释放该 serial 的 forward + reverse 规则。
* 提供策略配置：ReleaseAll(默认) / Retain / Notify(callback)。
* forward 与 reverse 行为一致（同一套断开语义覆盖两者）。
* 默认行为变化要有清晰的文档与变更说明（影响 xdb 等现有调用方）。

## Acceptance Criteria (evolving)

* [ ] USB 拔出后 `forward --list` 不再残留该设备规则（默认策略下）。
* [ ] reverse 规则同样在断开时释放，map 条目不再泄漏到 shutdown。
* [ ] Retain 策略下规则保留（调用方自管），有测试覆盖。
* [ ] Notify 回调收到断开的 serial，可自主释放，有测试覆盖。
* [ ] 契约测试覆盖 USB 与 TCP 两条路径的一致性。

## Definition of Done

* Tests added/updated（单元 + 契约测试，覆盖 USB/TCP 两路径）。
* Lint / typecheck / CI green。
* 文档更新：默认行为变更、配置入口、对调用方的迁移说明。
* 回滚考虑：默认行为改变属于行为变更，需评估对 xdb 的影响。

## Out of Scope (explicit)

* 暂不处理"断线重连后自动恢复规则"（属 Retain 策略下调用方自管范畴，可作后续）。

## Technical Notes

* 关键文件：`server/forward.rs`、`server/frontend.rs`、`server/backend.rs`、`server/default_backend.rs`、`usb/reverse_engine.rs`、`usb/reverse_policy.rs`、`usb/persistent.rs`。
* 设计需回答：断开事件如何从 backend → frontend（forward 注册表）流动，且独立于 `host:track-devices`。
