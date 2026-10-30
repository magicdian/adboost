# 持久连接 server 能力地基：真异步 ACK、device-OPEN 路由、裸消息通道、SYNC 多路复用

## Goal

把 `xp_adb_client`（fork of cocool97/adb_client v3.2.2）从"单向 client" 升级为 **权威 adb server 的传输/多路复用地基**，供 xdb 长期取代 adb server、拥有 :5037。本轮在 **XPENG 独有的 `persistent.rs`**（上游不存在、零合并成本）上实现 6 个能力 Ask，使 xdb 当 server 时能据实宣告能力、不静默丢包、支撑 reverse/scrcpy、大文件传输不被 USB-RTT 卡死。

## What I already know（两轮调研已确认，证据见 research/）

- fork = 上游 v3.2.2@365ef22 的 squash 快照，git histories 无关 → 合上游本就是手工 patch，非 merge。详见 [`research/00-fork-upstream-topology.md`](research/00-fork-upstream-topology.md)。
- **6 个 Ask 全部验证通过**，主战场全在 `persistent.rs`（826 行，XPENG 独有）→ **零上游合并成本**。详见 [`research/01-asks-verification.md`](research/01-asks-verification.md)。
- forward 原语已最优（`open_session(TcpConnect)`，`persistent.rs:275-343`）；`SessionReadHalf/WriteHalf/into_split()` 已 pub 导出。reverse 应放 xdb 层。详见 [`research/06-forward-async-synthesis.md`](research/06-forward-async-synthesis.md)。
- fork 全同步、xdb 全 tokio、nusb 是被 `.wait()`/`transfer_blocking` 压抑的 async-native。上游永久拒绝 async（PR #208 被拒 + #148/#201/#63/#147 零回应）→ fork 独立领跑。详见 [`research/05-forward-and-async-facts.md`](research/05-forward-and-async-facts.md)。

## Decisions（本轮已与用户敲定 — ADR-lite）

### D1. async 时机：Asks 先（同步 core），async core 推迟
- **Context**：nusb async-native 被压抑；xdb 全 tokio 裹 spawn_blocking；但 adb_cli + pyadb_client 是同步消费者。
- **Decision**：6 个 Ask 用**同步 core** 实现，但 **reader_loop 与 delayed_ack 窗口逻辑结构化成 "I/O 驱动可替换"**（为未来 sans-io async core C2 铺路）。async sans-io core（C2）推迟到本轮之后；xdb 侧先做 C1 薄门面收编 spawn_blocking（不在本 crate 范围）。
- **Consequences**：本轮不引入 tokio/async-fn；但写法上把"协议状态机 ⊥ I/O"作为设计约束，避免未来重写。

### D2. Ask #1 delayed_ack：真窗口化，建进状态机
- **Context**：用户初衷是"大文件 sync:/scrcpy 被 USB-RTT 卡死"（当前 stop-and-wait ≈ 6.5MB/s@10ms RTT，`persistent.rs:642-644`）。
- **Decision**：实现 **per-session `FlowControl`**（available_bytes / bytes_sent / bytes_acked），解析 OKAY 携带的字节计数，**pipeline 多个在途 WRTE**。窗口逻辑写进一个 I/O 无关的状态机组件（D1 的可替换驱动前提）。
- **⚠️ 前置研究**：OKAY 字节语义（累计 vs 增量、放 arg0 还是 payload）必须先回 AOSP 源码定真 → 见 `research/07-aosp-delayed-ack-wire-semantics.md`（进行中）。
- **Consequences**：吞吐解锁；但这是最高风险手术，改写两个 write half + 两个 read half；reader_loop 的 `try_send` 静默丢包隐患（`:251/:255`）必须一并解决。

### D3. MVP 范围：6 个 Ask 全部一刀切进本轮
- #6 诚实 banner、#2 device-OPEN 路由、#3 裸 channel、#4 SYNC v1 多路复用、#1 delayed_ack 窗口化、#5 shell-v2 退出码。
- 内部仍按依赖图分 PR 顺序（见 Implementation Plan），但都属本轮交付。

### D4. PR #198 / 上游 trait：完全不碰 trait
- **Context**：PR #198 把共享 `ADBDeviceExt` 改 v4 泛型，是未来唯一真合并摩擦点；但 `persistent.rs` 不走 `ADBDeviceExt`，零冲突。
- **Decision**：本轮**不碰 trait**（连 scope() 修复都不取——那修复针对 `ADBDeviceExt` 的 shell 线程，与我们独立的 reader 线程无关，零损失）。承认 fork 已是**独立 crate**：上游修 bug，fork 做 feature。
- **Consequences**：本轮范围最小化在 `persistent.rs` 及其紧邻支撑文件；不动共享 trait/commands。

## Requirements

### R1 — #6 诚实可配置 banner（地基，最先做）
- 把 `persistent.rs:127` 硬编码 banner 改为由 `FeatureSet` 生成，只宣告**本端真正实现**的 feature。
- 移除未实现的 `sendrecv_v2_*`（4 个）；`delayed_ack`/`shell_v2` 仅当对应 Ask 落地后才宣告。
- feature 名常量化（避免字符串散落）。

### R2 — #2 device-originated OPEN 路由 + #3 裸消息通道（合成一次 reader_loop 重设计）
- reader_loop（`:210-267`）识别 inbound `OPEN`（command==Open，目标 local_id 未注册）→ 路由到**可订阅的 inbound-OPEN 通道**（bounded + 明确溢出策略，单 reader 不可阻塞在满队列）。
- 暴露**裸消息收发**：旁路 session 注册表的 `subscribe_raw`（在现有 reader_loop 内 tee，**不开第二个 reader**——单 bulk-IN 约束）。
- 二者作为**一次** reader_loop 重设计完成，结构化成 I/O 驱动可替换（D1）。

### R3 — #1 delayed_ack 窗口化流控（最高风险核心）
- per-session `FlowControl` + 窗口化非阻塞写 + 按窗口策略发 OKAY；解决 `try_send` 静默丢包。
- 保留兼容性（见待定 Q4）。

### R4 — #4 SYNC v1 在持久连接上多路复用
- `PersistentUsbConnection::open_sync_session() -> SyncSession`，复用现有 reader_loop demux，走 SYNC v1 帧（STAT/LIST/SEND/RECV/DATA/DONE/OKAY/FAIL）。
- 退役 xdb 侧另开独占 `ADBUSBDevice` 的接口排他 workaround（`xdb-core/src/transport/usb.rs:198-224`）。
- SYNC v2 压缩**不做**（out of scope）。

### R5 — #5 shell-v2 内层帧解码
- `ShellV2Session` 包在 `MultiplexedSession` 之上（保持后者字节透明），解析 5 字节帧（id+LE 长度），分离 stdout/stderr，返回 `exit_code: u8`。参考 `adb_server_device_commands.rs:189-205`。
- 保留 v1 `shell_exec` 兼容。

## Acceptance Criteria

- [ ] xdb 当 server 时据实生成 banner；移除未实现 feature 后协议协商仍正确降级。
- [ ] `adb reverse tcp:P tcp:Q` 生效；默认模式 scrcpy（`reverse:localabstract:scrcpy`）能投屏（device-OPEN 被接住）。
- [ ] 裸通道能实现最小 `tcp:` 透传，正确性与 `open_session` 路径一致；reader_loop 无第二个竞争 reader。
- [ ] ≥100MB `sync:` push/pull 与 scrcpy 视频流不 stall；吞吐显著高于 64KB/RTT。
- [ ] 持久连接存活（shell 会话在）的同时 `adb push`/`adb pull` 成功且不二次 claim 接口。
- [ ] `adb shell <cmd>` 返回正确 exit code；stdout/stderr 分离。
- [ ] 满 channel 不再静默丢 WRTE/OKAY（窗口/背压有界且有错误路径）。

## Test Strategy（已确认）

- **sans-io 无硬件单测**：把协议状态机（delayed_ack 窗口推进、ADBTransportMessage 帧编解码、SYNC v1 帧、shell-v2 5 字节帧、device-OPEN/raw 路由判定）抽成 I/O 无关组件，单元测试喂字节序列断言状态转移与输出帧，**不依赖真机 USB**。
- 尤其 PR3（#1 窗口化，最高风险）：窗口推进（32MiB 初始、payload int32 delta、负值背压、overflow 不关流）、`try_send` 改背压后无静默丢包，全部用 sans-io 单测覆盖。
- 真机验证仅用于端到端验收（reverse/scrcpy、≥100MB push/pull、shell exit code）。

## Definition of Done

- 单元测试：协议状态机（窗口化、帧编解码、SYNC v1、shell-v2 帧）与 I/O 解耦后可无硬件单测。
- `cargo clippy`（pedantic）/ fmt / build 零警告（见 `.trellis/spec/backend/zero-warning-gate.md`）。
- 真机验证：reverse/scrcpy、大文件 push/pull、shell exit code。
- `persistent.rs` 的公共 API 变更有 doc 注释（`into_split()` 标 stable）。

## Out of Scope

- async sans-io core（C2）、tokio 化、纯 async API —— 推迟（D1）。
- SYNC v2 + 压缩（brotli/lz4/zstd）—— 推迟。
- 触碰共享 `ADBDeviceExt` trait / PR #198 对齐 / commands/* / server_device/* —— 不做（D4）。
- reverse 的 registry + per-service handler —— 属 xdb server 层，不在 crate。
- `ADBServerDevice::forward()` —— 保留作 CLI-client 回退，server 不用、不改。

## Research References

- [`research/00-fork-upstream-topology.md`](research/00-fork-upstream-topology.md) — fork=v3.2.2 squash 快照，persistent.rs 独有，零上游合并成本。
- [`research/01-asks-verification.md`](research/01-asks-verification.md) — 6 个 Ask 带行号对抗式验证 + 推荐 API。
- [`research/02-upstream-pr-strategy.md`](research/02-upstream-pr-strategy.md) — PR #184 ignore / #198 不碰。
- [`research/03-adb-protocol-truth.md`](research/03-adb-protocol-truth.md) — ADB 线协议真相（含 delayed_ack 待定真点）。
- [`research/04-synthesis-sequencing.md`](research/04-synthesis-sequencing.md) — 依赖图 + 排序。
- [`research/05-forward-and-async-facts.md`](research/05-forward-and-async-facts.md) — forward 两路径 + async 硬事实。
- [`research/06-forward-async-synthesis.md`](research/06-forward-async-synthesis.md) — forward 最优性 + async 战略裁决。
- `research/07-aosp-delayed-ack-wire-semantics.md` — （进行中）AOSP delayed_ack OKAY 字节语义定真。

## Technical Notes

- 依赖图（来自 research/04）：`#6（闸门）` → `#2+#3（一次 reader_loop 重设计）` ‖ `#1（核心写/读语义）` → `#4（骑在写/读半上）`、`#5（leaf）`。
- 单 bulk-IN reader 约束：reader_loop 独占 IN 端点（`persistent.rs:84-88` + `usb_transport.rs:310` 持锁）；所有路由改动都在这一个 loop 内分流。
- I/O 驱动可替换：把帧/CRC、OPEN/OKAY/WRTE/CLSE FSM、delayed_ack 窗口抽成 I/O 无关组件（sans-io 形态），同步 USBTransport adapter 不变，为未来 async adapter 留口。

### Spec 约束（来自 `.trellis/spec/backend/`，实现必须遵守）

- **不得新增 `lock().unwrap()`**：新锁点用 `?` 传播 `RustADBError::PoisonError`（`persistent.rs` 现有 9 处 unwrap 是已知 tech debt，禁止复制）。见 `error-handling.md`。
- **nusb 双锁模型**：IN/OUT 端点各自 `Arc<Mutex>`，**绝不合并成一把锁**（阻塞读会饿死写）。reader_loop 重设计必须保留。见 `database-guidelines.md`。
- **新增 `RustADBError` 变体** → 必须在 `adb_cli/src/models/adb_cli_error.rs` 加分类 arm（穷尽 match，禁止 `_` 兜底）。外来错误用 `#[from]`+`transparent`，域错误用 `{0}` 格式串。
- **USB 超时结构化匹配**：`Err(RustADBError::UsbTimeout)`，**禁止** `err.to_string().contains("timed out")`（nusb 与 libusb 措辞不同）。
- **代码复用**：`MultiplexedSession`（`:673-802`）与 `Session{Read,Write}Half`（`:547-666`）的读写字节流逻辑**已重复**——窗口化/帧解码改动必须**抽共享**，不可两处各写一遍（见 `code-reuse-thinking-guide.md`）。注意 `ADBTransportMessage` 当前**未 derive Clone**（`adb_transport_message.rs:12-16`），#3 的 tee 需要它可克隆。
- **质量门**：`cargo clippy --all-targets -- -D warnings`（pedantic）+ `--features usb` 本地复验（CI 不覆盖 usb）；MSRV 1.88.0；fmt 默认。
- **测试**：inline `#[cfg(test)] mod tests`（无 `tests/` 目录），`assert_eq!` 带描述串第三参；测试里 `.expect()` 可接受。
- **日志**：全限定 `log::<level>!`（不 `use log::`），inline 捕获 `{e}`；持久 USB 子系统用 `PersistentUsb:` 前缀；不记密钥/payload 内容（trace 仅记 size）。
- **目录约定**：`mod.rs`-per-dir；新类型进 `models/`，新 inherent op 进 `commands/<verb>.rs`（仅 impl 块）；public 通过父 `mod.rs` re-export。

## Decisions（API 形状细节，已敲定 — ADR-lite 续）

### D5. #2 device-OPEN API = 订阅 channel（pull）
- `PersistentUsbConnection::incoming_opens() -> Receiver<ADBTransportMessage>`，上层（xdb）自行轮询/select。
- bounded 队列 + 溢出丢最旧并计数（单 reader 不可阻塞）。未来可包成 async stream。**不**用回调（回调在 reader 线程跑有阻塞风险）。

### D6. #3 裸通道 API = 低阶原语 `subscribe_raw` + `send_raw`（承诺为稳定 public API）
- `subscribe_raw(filter) -> Receiver<ADBTransportMessage>`（在 reader_loop 内 tee）+ `send_raw(msg) -> Result<()>`。
- 上层做 id 翻译与流控。**不**在 crate 内做 RelaySession（中继语义交给 xdb）。明确承诺稳定 public API（刻意加深与上游分叉，符合 D4 独立 crate 定位）。

### D7. #1 兼容性 = 保留阻塞 Read/Write，下层换窗口
- `MultiplexedSession` + `Session{Read,Write}Half` 的 `Read`/`Write` trait 语义保留（现有调用方无感）；底层把 stop-and-wait 换成 32MiB 窗口状态机 pipeline。**不**新增独立窗口 API、**不**改 sender-task 模型（后者留给未来 async core）。

### D8. #6 banner = 可配置 FeatureSet + 构造注入
- `DeviceFeatureSet` 结构 + `new_with_features()` 注入 + `device_features()` 查询。为未来 per-connection 协商留口。

## delayed_ack 线协议定真（research/07，已纠正早期错误）

- OKAY 字节计数在 **payload**（4 字节 LE **int32 有符号**），**不在 arg0/arg1**；classic 模式 OKAY payload 为空。
- 语义是 **DELTA（增量）**：`available_send_bytes += acked_bytes`，可为负（背压预留）。
- 初始窗口 = **32 MiB**（`INITIAL_DELAYED_ACK_BYTES`），经 OPEN.arg1 授予；`MAX_PAYLOAD`=1MiB 是每包上限（两个不同概念）。
- feature 串 `delayed_ack`；两端都宣告才启用；overflow 不关流（纯自限速）。
- AOSP 单一有符号 running window 累加 delta；`bytes_sent`/`bytes_acked` 仅作 debug 计数，承载字段是 `available_bytes` 累加器。
