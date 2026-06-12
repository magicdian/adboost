# adboost 全面 tokio async 重构（USB + TCP + server）

## Goal

将 `adb_client` crate 从完全同步重构为**原生 tokio async** 库（对外品牌名 adboost）。保留 sans-io 协议核心不动，只替换 I/O 适配器层。core 不持有全局 runtime（runtime 所有权在最外层消费者）。MVP = 当前 adb_client 已有功能的全 async 对等实现，覆盖 USB + TCP + server/server_device 全部路径。后续在此基础上加 adb server 能力。

**范围边界（用户明确）**：本轮**只做 adboost 库本身**，不考虑 pyadb_client / adb_cli。等库功能完全实现后，再单独决定是否/如何适配消费者。

## What I already know

- 当前是**完全同步树**：零 tokio/async 痕迹（已 grep 确认）。
- nusb 已是 **0.2.3**（async-native）。已读源码核实：`Endpoint` 是**队列模型** —— `submit(buf)` 提交 + `next_complete().await`（**cancel-safe**，可直接进 `select!`）等完成；`transfer_blocking` 只是 `submit + wait_next_complete(timeout)` 的同步便捷封装。还有更高层 `EndpointRead`/`EndpointWrite`（`io/` 模块，内建 transfer 队列 + `with_read_timeout` + `until_short_packet`）。→ **async 路径原生，无需 spawn_blocking**。
- 染色源头单一：所有阻塞只源于两个 I/O 叶子 —— nusb `transfer_blocking`(`usb_transport.rs:193/203/364`) 和 `TcpStream`(`tcp_transport.rs`, `tcp_server_transport.rs`)。
- 染色无一例外往上穿透：`ADBTransport`/`ADBMessageTransport`/`ADBDeviceExt` 三 trait → persistent 多路复用层 → 所有 session codec → 三个消费者。约 16 个 `ADBDeviceExt` 方法 + ~40 内部调用点变 async。
- **三个消费者**：`adb_cli`（同步 CLI binary）、`pyadb_client`（PyO3，方法天生同步，需 block_on facade + GIL 处理）、`examples/mdns`（trivial）。
- MSRV 1.88 + edition 2024 → **AFIT/RPITIT 原生可用**。

## Sans-io 资产（冻结，回归基线，async 后一字不改幸存）

- `flow_control.rs` 整模块（delayed-ack 窗口状态机 + **16 测试**）。
- `persistent.rs:79` `classify_message`（纯 demux 路由）、`:109` `banner_advertises_delayed_ack`、`:1234` `apply_ack`、各纯 state holder/tag、`:1386-1549` 测试。
- `sync_session.rs` `encode_sync_header`/`SyncResponse::classify` + 4 测试；`shell_v2_session.rs` `decode_frame_header`/`ShellChannel::try_from` + 9 测试。
- `usb_transport.rs` `aligned_request_len`/`map_transfer_status`/`find_endpoints` + 测试。
- 协议层 `adb_transport_message.rs`/`message_commands.rs`/`models/*`（含 ADBRsaKey 签名、DeviceFeatureSet）/`error.rs`/`utils.rs`。
- **半纯**（抽出 I/O，逻辑可单测）：`read_with_ack`(`:1028`)、`windowed_write`(`:1128`) —— 建议重构成返回 next-action(Send/Block/Error) 的状态机。

## 最硬的闸门：CLSE/Drop teardown

三处 Drop 需在析构发 CLSE 或 join：`PersistentUsbConnection`(`:809`)、`MultiplexedSession`(`:1362`)、`SessionCleanup`(`:948/957-959`)。Rust stable 无 async Drop。**决策（P0-②）**：显式 `async close()`/`shutdown()` 发 CLSE 等确认（graceful），Drop 兜底 fire-and-forget 入队给 writer task + `handle.abort()`。
配套取消安全坑：`read_with_ack` 在 `data_rx.recv()`(`:1052`) 与发 OKAY(`:1070`) 之间被取消 → 永久丢窗口额度。

## Decision (ADR-lite) — P0 已锁定

**Context**：全面 async 化是大变更，根决策决定整个骨架。

**Decisions**：
1. **Crate 策略**：原地改 `adb_client`，**直接替换 sync**（不 feature-gate 并存）；消费者用顶层 `block_on` facade 适配。保留 git 历史、便于 cherry-pick 上游、单一 codepath。crate 改名发布 MVP 后再议。
2. **Teardown**：显式 `async close()` + best-effort Drop（fire-and-forget 入队 + `abort()`）。
3. **Trait 染色**：AFIT（async-fn-in-trait，无 Box future 分配）+ 破坏性改参数为 `AsyncRead`/`AsyncWrite`；`trait_variant` 保 Send + dyn/`boxed()` 对象安全；block_on facade 桥接 sync `File`/`Vec`。
4. **Server/TCP 节奏**：与 USB core **一起 async 化**（`tokio::net::TcpStream` + `tokio-rustls` + `tokio::io::split` 替 thread relay），全库一次到位。
5. **消费者处置（P1-②）**：本轮**只做库**。从 `Cargo.toml` workspace `members` 暂时移除 `adb_cli`、`pyadb_client`、`examples/*`（依赖旧同步 API 会编译不过）；它们留在磁盘但不参与本轮构建/CI。adboost 库独立演进，`cargo build`/clippy/test 只跑库本身。pyadb runtime 模型、block_on facade、adb_cli 改造全部推迟到库稳定后的独立任务。

**Consequences**：MVP 范围 = 全库（USB+TCP+server）一次性全 async，破坏性 API 变更；消费者全部需改造；引入 tokio + tokio-util + tokio-rustls 依赖。

### P1-① writer 共享原语 → **writer-task 模型（方案 3）已锁定**

放弃 `Arc<Mutex<USBTransport>>` 共享锁。改为：所有出站帧（OPEN/OKAY/WRTE/CLSE/raw）经一个 `tokio::sync::mpsc` 投递给一个**专属 writer task**，由它独占 OUT 端点串行 `submit + next_complete().await`。
- 依据：已确认 reader task 从不写 OUT 端点（只 read IN + 路由），OUT 物理单写者；当前 9 处 `writer.lock()`（`persistent.rs:347/660/708/957/1071/1179/1372` 等）全部收敛为入队。
- 零锁争用、单写者语义被结构强制、与 teardown 方案同一条 channel（Drop 发 CLSE = `try_send` fire-and-forget）。
- **写完成语义（选项 1，混合）**：OKAY / CLSE 纯入队 fire-and-forget（不需结果）；WRTE 入队时附 `oneshot::Sender`，writer task 发完回传 `Result`，`windowed_write` `.await` 回执后再 `record_sent` / 报 `BrokenPipe`——保住 flow-control 记账时序与背压正确性。

### P1-③ 取消安全策略 → **已锁定（选项 1：关键临界区取消安全 + 部分帧即关闭）**

- **窗口记账（必须正确）**：靠 writer-task 模型结构性消除取消缺口，**不改 OKAY 协议时序**。机制：`read_with_ack` 中唯一 await 是 `data_rx.recv().await`（tokio mpsc recv **cancel-safe**，取消不丢消息）；recv 返回后发 OKAY 是 `okay_tx.try_send()`（同步、非阻塞、无 await）→ "取走 WRTE → OKAY 入队"之间无 await 点，取消无法卡在中间，窗口额度不丢。OKAY 仍在消费者读侧发出（时序同当前同步实现，无可观察协议变化）。
  - ⚠️ 反面教训（已否决）：不要"reader 收到 WRTE 即回 OKAY"——data channel 有界、满则丢 WRTE（`:541`），提前 ack 会 credit 已丢弃的数据，掩盖数据丢失。
- **部分帧**：单个 `read_message().await`（一个 ADB 帧）是 nusb cancel-safe 原子单位；跨多 message 的 SYNC/shell-v2 帧组装中途取消 → **标记 session 流损坏并 close（发 CLSE）**，不回滚。文档声明"取消进行中的 session I/O 会终止该 session"。
- **临界区纪律**：持锁/记账绝不跨外部 await；WRTE+`record_sent` 在拿到 oneshot 回执后于单个非 await 段完成。

### 已定（推导/惯例，无需用户拍板）

- **reader 背压**：保留 `try_send`（满则丢 + `log::warn`），守住 `persistent.rs:461` 标注的"reader 绝不阻塞"硬不变量。tokio mpsc 的 `try_send` 语义对等，禁用 `send().await`/`blocking_send`。
- **构造器**：握手是 I/O，不能在 sync ctor 里做 → 直接把现有 `new()/new_with_features()/new_from_ids*()` 构造器签名 async 化（`async fn`），不引入 builder。
- **error.rs**：新增 `tokio::task::JoinError`（task panic）+ 超时/取消变体（一旦 spawn task 即必需）。
- **executor**：硬绑 tokio（不做 executor-agnostic 抽象，简单、生态强）。

### 已推迟到库稳定后的独立任务

- pyadb runtime 模型（module-level Runtime + GIL-release block_on vs executor 线程 vs pool；多 Python 线程并发需求）
- block_on facade 放 crate 内 `sync` 模块 vs 仅消费者
- adb_cli 改造（`#[tokio::main]` vs sync main + block_on boundary）

## Requirements

- **只做 adboost 库本身**（`adb_client` crate）。全库 async：USB（persistent 多路复用 + nusb 队列模型 + writer-task）、TCP message device、server/server_device。
- sans-io 核心冻结，纯逻辑测试一字不改通过。
- core 不持 runtime（消费者持有）。executor 硬绑 tokio。
- writer-task 模型：单 OUT-端点 writer task；OKAY/CLSE fire-and-forget，WRTE 带 oneshot 回执。
- 显式 `async close()` 生命周期管理 + best-effort Drop（fire-and-forget CLSE 入队 + `handle.abort()`）。
- 取消安全：窗口记账取消缺口结构性消除（recv cancel-safe + OKAY 同步入队）；部分帧取消即 close。
- AFIT 三 trait（`ADBTransport`/`ADBMessageTransport`/`ADBDeviceExt`）+ `trait_variant` Send 变体 + dyn/`boxed()` 对象安全；`shell/exec/pull/push` 参数破坏性改 `AsyncRead`/`AsyncWrite`。
- 从 workspace `members` 移除 `adb_cli`/`pyadb_client`/`examples/*`，本轮不参与构建。

## Acceptance Criteria

- [ ] sans-io 纯逻辑测试（flow_control 16 + sync 4 + shell-v2 9 + classify/banner 等）async 后**不改动**通过（回归锚）。
- [ ] async 集成测试（内存 mock transport，impl async transfer 喂字节序列）：reader 路由、背压 timeout、`AsyncRead`/`AsyncWrite` 往返、writer-task WRTE oneshot 回执。
- [ ] teardown 闸门测试通过：kill-mid-stream / drop-without-close / 并发 drop → 无 remote session 泄漏、无 panic。
- [ ] 取消安全测试：mid-`read_with_ack` 取消不丢窗口额度；mid-frame 取消正确 close。
- [ ] `cargo build -p adb_client --all-features` + `cargo clippy --all-targets --all-features -- -D warnings`（pedantic）+ `cargo fmt --check` + `cargo test -p adb_client` 全绿（MSRV 1.88）。

## Definition of Done

- 测试新增/更新（sans-io 单测原样保留 + async 集成 + teardown + 取消安全）。
- Lint / typecheck / CI 绿（仅库范围）。
- 破坏性 API 变更：在库内文档/README 记录新 async API 与迁移说明。
- workspace `members` 调整已提交，CI 配置同步（若有）。

## Out of Scope

- **pyadb_client / adb_cli / examples 的 async 适配**（库稳定后独立任务）。
- 新增 adb server 服务端能力（本次只做现有功能 async 对等）。
- crate 正式改名发布到 crates.io（API 稳定后再议）。
- 支持 tokio 以外的 runtime（硬绑 tokio）。

## Technical Notes

- 调研报告全文：`/private/tmp/claude-501/-Users-magicdian-Documents-personal-project-adboost/033c5dd0-2640-4a0a-b398-7f21d992ca3b/tasks/w0diyh0pj.output`（含完整 blocking-point inventory、染色表、消费者成本）。
- 蓝图：`/private/tmp/TOKIO-REWRITE-PLAN.md`。
- 关键文件：`adb_client/src/message_devices/usb/{persistent.rs,usb_transport.rs,sync_session.rs,shell_v2_session.rs,flow_control.rs}`、`adb_device_ext.rs`、`message_devices/{adb_message_transport.rs,adb_message_device.rs,adb_session.rs,session_stream.rs}`、`message_devices/tcp/tcp_transport.rs`、`server/tcp_server_transport.rs`、`server_device/adb_server_device_commands.rs`。
