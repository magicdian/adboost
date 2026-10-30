# Expose TCP persistent connection building blocks for external device backends

## Goal

adboost 已经把 `DeviceBackend` trait 及其返回值类型（`MultiplexedSession` / `SyncSession` /
`ShellV2Session` / `DeviceEntry` …）公开，默认外部 crate 可以实现自己的 device backend。
但外部 backend 无法**持有一条持久化 TCP ADB 连接**：建连所需的具体类型 `TcpTransport`
不可命名，导致 `PersistentConnection<TcpTransport>` 既无法构造也无法作为字段存储。
本任务补齐这一公开契约缺口，让外部 backend 作者能用与 `DefaultDeviceBackend` 相同的积木搭建 TCP 连接，
并遵循"标准行为开箱即用、额外配置解锁定制"的库设计哲学。

## What I already know (verified in code)

- **真正的硬阻塞 = `TcpTransport` 不可命名**：`message_devices/tcp/mod.rs:4` 声明
  `pub(crate) mod tcp_transport`，所以 `pub struct TcpTransport`(tcp_transport.rs:97) 对外不可见。
  外部因此无法填入泛型参数 `PersistentConnection<TcpTransport>`，既不能 `new_from_tcp_addr`，
  也不能声明字段去 store。
- `PersistentConnection<T>` 本体**已可达**：`adboost::usb::persistent::PersistentConnection`
  是 pub struct（persistent.rs:357）在 `pub mod persistent` 里，且方法都是 pub 固有方法
  （`open_session` / `open_sync_session` / `open_shell_v2` / `is_alive` / `shutdown` / `new_from_tcp_addr`）。
  **不需要 trait-object / dyn**：具体实例上的固有方法直接可调。
- `new_from_tcp_addr(addr, key)` 是 pub 的标准构造器（persistent.rs:546），内部自建 `TcpTransport`
  并走通用 CNXN(+AUTH,+STLS) 握手，广播 `DeviceFeatureSet::default()`。
- TCP 侧**缺少** `*_with_features` 构造器；USB 侧有 `new_with_features`(persistent.rs:416)。
- `DeviceFeatureSet` **完全开放**：`pub struct`、10 个字段全 `pub`、有 `Default`，
  并从 crate root re-export（models/mod.rs:23 → lib.rs:78）。外部可直接
  `DeviceFeatureSet { ..Default::default() }` 拼自定义 banner，无需新增导出。
- 路径归属 smell：`PersistentConnection` 是传输无关的核心层，却住在 `message_devices::usb::persistent` 下；
  `PersistentUsbConnection` 已导出（usb/mod.rs:15），但 `PersistentTcpConnection` 只是
  `default_backend.rs:38` 里的私有类型别名。
- 仓库 `#![deny(warnings)]` 风格：若把 `pub type Alias = PersistentConnection<TcpTransport>` 放在
  `TcpTransport` 仍是 `pub(crate)` 的前提下，会触发 `private_interfaces` lint —— 所以暴露别名的前提是
  `TcpTransport` 先变 nameable。
- `DeviceBackend` trait 用了 `#[trait_variant::make(Send)]`（backend.rs:118），仓库已有 async-trait 先例
  —— 如果未来要做 Tier 3 的连接 trait 抽象，有现成模式可循。

## Requirements (evolving)

- [R1] 让 `TcpTransport` 可从外部命名（最小暴露：类型可命名即可，不新增可玩 pub 方法）。
- [R2] 提供并导出公开别名 `PersistentTcpConnection = PersistentConnection<TcpTransport>`，
       与 `PersistentUsbConnection` 对称。
- [R3] 提供 TCP 侧的 features 定制构造入口（标准 = `Default`，定制 = 显式传入 `DeviceFeatureSet`）。
- [R4] 把 `PersistentConnection` / `PersistentTcpConnection` / `PersistentUsbConnection` 收敛到一个
       传输中立的稳定公开路径（至少通过 re-export）。

## Open Questions (all resolved)

- [Q1] ~~R3 的 API 形态~~ → **已定：Options 结构**。引入 `TcpConnectOptions`
       （`features` + `private_key_path` + 未来旋钮，非穷尽 + `Default`），构造收敛为
       `new_from_tcp_addr(addr)`（标准零配置）+ `new_from_tcp_addr_with_options(addr, opts)`（定制）。
       这样 `TcpTransport` 只需"可命名"，无需暴露其构造细节。
- [Q2] ~~R4 的 re-export 落点~~ → **已定：crate root 直接 re-export**。在 `lib.rs` 加
       `pub use` 暴露 `PersistentConnection` / `PersistentTcpConnection` / `PersistentUsbConnection`
       / `TcpConnectOptions` / `TcpTransport`。物理搬迁文件留作独立整理任务（out-of-scope）。
- [Q3] ~~Tier 3 连接 trait 抽象~~ → **已定：本任务不做，记入待办**。当前无多态需求，YAGNI；
       trait 形状待真实"混合持有 USB+TCP"需求出现再定。
- [Q4] ~~`TcpTransport` 的暴露方式~~ → **已定：直接 `pub use TcpTransport`**，接受它带出 `new`。
       代价极小（`new` 产出的传输只能塞进 `PersistentConnection::new`，不构成误用陷阱），实现最简。

## Acceptance Criteria (evolving)

- [ ] 一个 crate 外的最小示例能：`use adboost::...; PersistentTcpConnection::new_from_tcp_addr(addr, key)`
      并把返回值存进结构体字段。
- [ ] 外部能用自定义 `DeviceFeatureSet` 建立 TCP 连接（标准路径仍零配置）。
- [ ] `open_session` / `open_sync_session` / `open_shell_v2` / `is_alive` / `shutdown` 对外可调。
- [ ] `cargo build` / clippy 在 `deny(warnings)` 下绿，无 `private_interfaces` 等可见性 lint。
- [ ] 新增/更新文档说明外部 backend 如何搭 TCP 连接。

## Definition of Done

- 公开 API 变更有测试覆盖（至少一个 doc-test / 集成测试模拟外部调用路径）。
- Lint / typecheck / CI 绿。
- 文档更新（external backend 集成说明）。
- 语义版本影响评估（新增公开 API → minor）。

## Technical Approach

1. **R1/Q4 — 暴露 `TcpTransport`**：`tcp/mod.rs` 把 `pub(crate) mod tcp_transport` 改为 `mod tcp_transport;`
   并加 `pub use tcp_transport::TcpTransport;`（与 `ADBTcpDevice` 同款导出方式）。
2. **R3/Q1 — `TcpConnectOptions`**：新增结构体，字段 `features: DeviceFeatureSet` +
   `private_key_path: Option<PathBuf>`，`#[non_exhaustive]` + `Default`（`Default` 即标准行为，
   等价当前 `new_from_tcp_addr` 的隐含语义），并提供 `with_features` / `with_private_key_path` 链式 setter。
   - `PersistentConnection<TcpTransport>` 上新增 `new_from_tcp_addr_with_options(addr, opts)`，
     内部解析 key path（沿用 `get_default_adb_key_path` fallback）→ `TcpTransport::new` →
     `Self::new_with_features(transport, key, opts.features)`。
   - 既有 `new_from_tcp_addr(addr, key)` 保留（标准零配置路径），实现改为委托
     `new_from_tcp_addr_with_options`，避免逻辑重复。
3. **R2 — `PersistentTcpConnection` 公开别名**：在 `usb/persistent.rs`（或 mod）加
   `pub type PersistentTcpConnection = PersistentConnection<TcpTransport>;`，与
   `PersistentUsbConnection` 对称。`default_backend.rs:38` 的私有别名改为复用这个公开别名。
4. **R4/Q2 — crate root re-export**：`lib.rs` 加 `pub use` 暴露 `PersistentConnection`、
   `PersistentTcpConnection`、`PersistentUsbConnection`、`TcpConnectOptions`、`TcpTransport`。
5. **测试 + 文档**：加 doc-test / 集成测试模拟"crate 外构造并持有 TCP 连接 + 自定义 features"；
   更新外部 backend 集成说明。

## Decision (ADR-lite)

**Context**：`DeviceBackend` trait 已公开（鼓励外部实现 backend），但建立/持有持久化 TCP 连接的积木
（`TcpTransport` 不可命名）未公开，导致外部 backend 无法搭 TCP 连接。需在"补齐能力"与"控制公开 API 面"间取舍。

**Decision**：
- 暴露 `TcpTransport`（仅需可命名，接受其 `new` 一并可见）。
- 用 `TcpConnectOptions`（`Default`=标准、setter=定制）承载 features 等定制，而非配对 `_with_features` 构造器。
- 公开对称别名 `PersistentTcpConnection`，并从 crate root re-export 整个 Persistent* 家族。
- 不抽连接 trait（无多态需求，YAGNI）；不物理搬迁源文件（先 re-export）。

**Consequences**：
- 公开 API 新增 → semver minor。
- `TcpConnectOptions` 为未来定制旋钮（超时、window、TLS 策略）提供非破坏扩展点。
- 体现"标准开箱即用 + 配置解锁定制"的库哲学。
- 遗留两笔整理待办：①连接 trait 抽象（多态需求出现时）；②把 Persistent* 物理迁出 `usb` 模块。

## Out of Scope

- 物理搬动 `PersistentConnection` 源文件出 `usb` 模块（先靠 re-export 收敛路径，避免大 churn）。
- Tier 3 连接 sealed trait / dyn 逃生口（无多态需求）。
- TCP 之外的其它定制旋钮（超时、window size、TLS 策略）—— `TcpConnectOptions` 预留扩展位，本次不实现。

## Implementation Plan (small PRs)

- **PR1**：暴露 `TcpTransport`（R1）+ 公开别名 `PersistentTcpConnection`（R2）+ crate root re-export
  骨架（R4），附最小 doc-test 证明 crate 外可命名/构造/存储。`default_backend.rs` 私有别名改为复用公开别名。
- **PR2**：`TcpConnectOptions` + `new_from_tcp_addr_with_options`，既有 `new_from_tcp_addr` 委托之（R3）；
  加自定义 features 的集成测试。
- **PR3**：文档（外部 backend 集成 TCP 连接指南）+ 收尾，记录两笔整理待办。

## Technical Notes

- 关键文件：
  - `adboost/src/message_devices/tcp/mod.rs`（TcpTransport 可见性）
  - `adboost/src/message_devices/usb/persistent.rs`（PersistentConnection + 构造器）
  - `adboost/src/message_devices/usb/mod.rs`（别名导出现状）
  - `adboost/src/server/default_backend.rs:38`（私有 PersistentTcpConnection 别名 + connect 参考实现）
  - `adboost/src/models/device_feature_set.rs`（DeviceFeatureSet，已全 pub）
  - `adboost/src/lib.rs`（crate root re-export）
- 调用方 = xdb，pin 在 adboost rev 1269b8a。
