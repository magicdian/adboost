# PR3: Per-session backpressure must never drop control signals

> 子任务 of [`06-13-adb-client-persistent-usb-robustness`](../06-13-adb-client-persistent-usb-robustness-graceful-shutdown-reader-fatal-suite-reporting-per-session-backpressure/prd.md)。修复缺陷 **C**（彻底方案）。

## Goal

消除 `PersistentUsb: session <id> queue full, dropped OKAY/CLSE message` 警告及其正确性隐患：reader 的 per-session bounded queue（`SESSION_CHANNEL_SIZE=64`）满时按 never-block 策略 `try_send` 丢帧。丢 **CLSE** 丢失关闭语义（潜在 hang / 喂给 stale-CLSE 污染环）；丢 **OKAY** 破坏 delayed_ack 流控窗口记账（潜在发送侧停滞或窗口漂移）。彻底方案：把控制信号（CLSE 关闭、OKAY 窗口信用）从可丢弃的数据队列中**结构性分离**，使其永不丢失，同时严格保留 reader never-block 不变量。

## Root cause (confirmed)

- reader_loop 把 OKAY→`ack_tx`、WRTE/CLSE→`data_tx`（均 bounded 64），满时 `try_send` 失败即丢 + warn（`persistent.rs:886-892`）。高吞吐 reverse/iperf3 teardown 期复现（实测 `pr1/pr2` 每 run 2-3 条）。
- CLSE 丢失：消费者排空队列后再 poll 得不到 EOF → 潜在 hang；关闭信号丢失也回喂 B 的污染环。
- OKAY 丢失：窗口 delta credit 丢失，`FlowControl` 记账漂移。

## Requirements (thorough, structurally lossless)

1. **CLSE 关闭信号无损（C1）**：
   - `SessionChannels`（reader 视图）新增 `closed: Arc<AtomicBool>`，与 `SessionInner.closed` 同一 `Arc`。
   - reader 分类到 CLSE（ack 或 data 路由）时**直接 `closed.store(true)`**，与 try_send 无关 → 队满也不丢关闭语义。仍 try_send 到 data 通道以保证有序、及时的 EOF（best-effort）。
   - 重构 `poll_read_impl`：关闭→EOF 的判定移到**数据队列排空之后**（先交付缓冲的 WRTE，再 EOF），既不 hang（丢 CLSE）也不丢已缓冲数据。
2. **OKAY 窗口信用无损（C2，彻底）**：
   - 新增共享 `Arc<AtomicI64>` 信用累加器（reader 写、write 半读，同一 Arc 经 SessionInner + SessionChannels 共享）。
   - reader 分类到 OKAY 时解析 4 字节 LE i32 delta 并 `fetch_add` 到原子（无损）；对 `ack_tx` 的 bounded send 降级为纯 wakeup/handshake poke——满时**不再 warn / 不丢信用**（信用已在原子里）。
   - write 半把原子作为**唯一信用来源**：`send_flow.apply_delta(credit.swap(0))` 取代 `on_okay_payload(payload)`；ack_rx 上的 OKAY 消息退化为"去查原子"的唤醒信号。
   - classic 模式（空 payload，delta=0）：原子恒为 0，stop-and-wait 仍靠 ack 消息到达 rendezvous，行为不变（classic ack 队列至多 ~1，永不溢出）。
   - 握手 grant：首个 OKAY 的 grant delta 经原子传递；accept 后 `apply_delta(credit.swap(0))` seed 发送窗口。
3. **reader never-block 不变量不破坏**：所有 reader 侧仍 `try_send`/原子写，绝不阻塞。
4. WRTE 数据帧在队满时仍可能丢（never-block + 有界内存的固有取舍，非本 PR 目标；只保证不静默——保留 data 丢帧的 warn）。

## Acceptance Criteria

- [ ] 连续 3 次多设备 `selftest --no-interactive`（含 reverse_iperf3 高吞吐）：**0 条** `dropped CLSE message` / `dropped OKAY message`。
- [ ] reverse_iperf3 吞吐不回退（与 PR2 基线量级相当，验证流控窗口未被破坏）。
- [ ] 全部既有 persistent.rs 单测（含 device-verified 的 open/okay/window/cancel-safety）仍绿。
- [ ] 新增单测：reader 在 data 队满时 CLSE 仍置 closed（读端最终得 EOF，缓冲 WRTE 不丢）；reader 在 ack 队满时 OKAY delta 仍入信用原子（write 端窗口被正确 credit）；poll_read 排空后才 EOF 的顺序性。
- [ ] `cargo build` + clippy + 全测试绿；0 FAILED selftest。

## Out of Scope

- B+D（PR1）、A（PR2）。
- WRTE 数据帧的零丢失（与 never-block + 有界内存固有冲突）。
- 调大 `SESSION_CHANNEL_SIZE`（治标；本 PR 走结构性无损）。

## Decision (ADR-lite)

**Context**: 控制信号与数据共用可丢弃 bounded 队列，never-block 下队满即丢，破坏关闭/流控正确性。
**Decision**: 控制信号结构化为原子（closed: AtomicBool、credit: AtomicI64），reader 直接更新（无损），bounded 队列仅承载数据 + 作唤醒/握手 poke。窗口信用单一来源 = 原子。
**Consequences**: 触及 reader demux、SessionChannels/SessionInner 结构、poll_read/poll_write/apply_ack、open_session/accept_device_open 的握手 seeding——回归风险最高，故放在最后并重测；classic 路径刻意保持不变以缩小风险面。

## Technical Notes

- 关键文件：`adb_client/src/message_devices/usb/persistent.rs`（reader_loop 路由 886-908、`SessionChannels` 1591、`SessionInner` 构造 3 处、`apply_ack` 1745、`poll_read_impl` 1783、`poll_write_impl` 1912、open_session 1089-1140 grant seeding、accept_device_open 1275）。`flow_control.rs` 的纯状态机不变（`apply_delta` 已存在）。
- 不变量来源：`database-guidelines.md`（单 reader、never-block、delayed_ack 契约：OKAY delta 是 OKAY payload 的 i32 LE、signed、可负、累加；初始窗口 32MiB；opener 发送窗口起始 0）；`adb-wire-protocol-contract.md`（CLSE-on-data 的 open 拒绝快速失败；reader 帧对齐不变量）。
- 实测基线：`/tmp/pr2_run{1,2,3}.log`（每 run 2-3 条 dropped OKAY/CLSE，全部 PASS——证明当前仅噪音+隐患，未致失败）。
