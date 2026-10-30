# Export composable ReverseEngine for external DeviceBackend impls

## Goal

让 adboost 把 reverse 数据路径做成对任何"自建 server"型 `DeviceBackend` 实现者都可组合的**公开稳定 API**。当前 reverse 的状态机（`ReverseState` + pump + bridge + 控制命令）全是 `pub(super)`/内部自由函数，绑死在 `UsbDeviceBackend` 内。依赖方 xdb 希望像 sync/shell_v2 那样把自己的 reverse 降为"几行委托"。产出一个稳定的 `ReverseEngine`，其内部实现可自由优化而不波及依赖方。

## What I already know

- **reverse 协议有两个面**：
  - 控制面（建/删/列规则）：发一条 `reverse:*` service 命令，**与底层链路无关**。
  - 数据面（accept 设备 inbound OPEN → 拨号 host 目标 → 桥接字节流）：**归属取决于链路**。
- **USB 直连场景**（`server/usb_backend.rs`）：adboost 自己当 ADB server，数据面必须自己做 → 这正是 `ReverseState`+pump+bridge 的职责。
- **5037 proxy 场景**（`proxy/device_commands/reverse.rs:9`）：adboost 只把 `reverse:` 命令转发给真正的 adb server，`.map(|_| ())` 即完；**无 pump / 无 bridge / 无 ReverseState**——数据面由那个 adb server 全包。
- 结论：`ReverseEngine` 是 **"自建 server"型后端专属** 的数据面引擎，不是链路无关的通用件。proxy 型后端应转发命令而非用本引擎。

### 现状代码锚点

- `server/reverse.rs:28` `ReverseState`（`pub(super)`，字段 rules/policy/pump_started）+ `ensure_pump`(:81) + `run_reverse_pump`(:126) + 8 个单测(:203-287)。
- `server/usb_backend.rs:75` `reverse_state()`（get_or_open → entry.or_insert → ensure_pump）；`:236-277` 四个 trait 方法委托；`:284` `run_reverse_command` 自由函数（控制命令）。
- `server/frontend.rs:864` `bridge_session_public`（`pub(super)`，pump 的 host-dial 侧复用 forward/local 的双向 copy）。
- `server/backend.rs:106` `ReversePolicy`（pub，`RejectUnconfigured`/`AllowAll`/`Custom`）；`:147` `DeviceBackend` trait 的四个 reverse 方法（默认返回 unsupported）。
- `usb::` 已导出可组合件 `SyncSession`/`ShellV2Session`/`PersistentUsbConnection`/`MultiplexedSession`（`message_devices/usb/mod.rs`），且不依赖 `server` feature。
- `PersistentUsbConnection` 公开原语：`incoming_opens()`（单消费者）/ `accept_device_open()` / `send_raw()` / `open_session()`。

## Locked Contract (已与 xdb 团队确认)

```rust
pub fn new(conn: Arc<PersistentUsbConnection>, policy: ReversePolicy) -> Arc<Self>;
pub async fn open(&self, remote: &str, local: &str) -> Result<()>;
pub async fn remove(&self, remote: &str) -> Result<()>;
pub async fn remove_all(&self) -> Result<()>;
pub async fn list(&self) -> String;
```

三条显式保证（写进 doc 契约）：

1. **pump 先于 listener 就绪**：`open()` 内部顺序固定为 `ensure_pump → 下发 reverse:forward: 命令 → add_rule`。`open()` 返回时入站 pump 必已就绪，设备 listener 建立后首个 inbound 连接不丢。调用方无需单独 `start`。
2. **per-connection 粒度**：engine 不持有/不管理 serial。多设备由调用方按 serial 维护各自的 `Arc<ReverseEngine>`。
3. **`ReversePolicy` 保持 pub**，含 `Custom`/`AllowAll`/`RejectUnconfigured` 三变体。

## Requirements

- 提取 `ReverseEngine`，构造签名 `new(Arc<PersistentUsbConnection>, ReversePolicy) -> Arc<Self>`，持有 conn。
- 把控制命令（现 `run_reverse_command`）吸进 engine，使控制面+数据面统一归 engine 所有，backend 四方法降为一行委托。
- 公开 API 仅暴露 5 个意图级方法；pump 调度 / policy 判定 / bridge 实现 / 单消费者 receiver 管理全部私有。
- `ReversePolicy` 保持 pub 并可被 `usb::` 路径与 `server::` 路径同时访问（迁移用 `pub use` 兜底，不破坏现有 import）。
- `UsbDeviceBackend` 改为委托 `ReverseEngine`，行为等价（无协议变化）。
- 现有 8 个 reverse 单测随实现迁移并保持通过。
- doc 注释写明：本引擎仅适用于"自建 server"型后端；proxy/有外部 adb server 的后端应转发 `reverse:` 命令。

## Decision (ADR-lite)

**Context**: reverse 数据面被绑死在 `UsbDeviceBackend` 内部，需提取为对外稳定 API。三处归属决策需拍板。

**Decision**（已确认）:
- Q1 → **`ReverseEngine` + `ReversePolicy` 落 `usb::`**。数据面只用 usb 原语、不依赖 `server` feature，和 `SyncSession`/`ShellV2Session` 同层对称。`server::ReversePolicy` 改为 `pub use crate::usb::ReversePolicy` 兜底，现有 import 不破；xdb 不开 server feature 也能用。
- Q2 → **bridge 提为公开 `usb::bridge_tcp_session(host: TcpStream, session: MultiplexedSession)`**。解开 engine 对 `frontend::bridge_session_public` 的跨模块私有依赖；frontend 的 forward/local 与 reverse pump 共用同一实现（消除 half-close 双处维护）；xdb 亦可复用做 forward 桥接。
- Q3 → **不做 `ReverseManager`**。`serial -> Arc<ReverseEngine>` 缓存由调用方维护，保持 engine per-connection、不持 serial；避免过早把连接缓存策略吸进库。

**Consequences**:
- `usb::` 出现"reverse 引擎"——传输层模块承载一个偏上层的概念，靠 doc 注释说明适用场景（仅"自建 server"型后端）化解认知成本。
- `bridge_tcp_session` 成为公开契约，其 half-close 语义今后属对外行为，变更需谨慎。
- 须验证 `ReversePolicy` 迁移后 `server` 侧所有引用经 re-export 仍编译通过。

## Acceptance Criteria

- [ ] `ReverseEngine` 按锁定契约导出，5 个方法签名一致。
- [ ] 三条保证在 doc 注释中显式声明。
- [ ] `UsbDeviceBackend` 四个 reverse 方法委托 engine，行为等价。
- [ ] reverse 单测全部迁移并通过；`cargo test` / clippy 绿。
- [ ] `ReversePolicy` 三变体仍 pub，现有 import 路径不破。
- [ ] 一个最小示例/doctest 演示外部 backend 四行委托。

## Definition of Done

- 单测迁移 + 必要新增（engine 构造/委托）。
- `cargo clippy` / `cargo test` 绿。
- doc 注释更新（契约 + 三保证 + 适用场景）。
- 无 reverse 协议行为变化（纯可见性/位置/职责合并重构）。

## Out of Scope

- 不改 reverse 线协议行为、policy 语义、pump 调度算法。
- 不为 proxy 型后端引入 engine（它们转发命令）。
- 不实现 `ReverseManager`（除非 Q3 决定做）。
- 不扩展 reverse 到非 tcp: 目标。

## Technical Notes

- 单消费者约束：`incoming_opens()` 只能取一次；须保证一个 conn 只建一个 engine（`pump_started` 已保证 pump 幂等；engine 单例由调用方 map 保证）。
- bridge 现仅用公开类型（`TcpStream` + `MultiplexedSession::into_split`），搬出 frontend 无额外暴露成本。
- 若 `ReverseEngine` 落 `usb::`，则它不应依赖 `server` feature——需确认 `ReversePolicy` 及其依赖不牵连 server-only 类型。
