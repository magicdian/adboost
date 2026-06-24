# Sim harness Phase B — session 状态机 + 流控 + ChunkedTransport

> 父任务：[`../06-24-simulateddevice-software-adb-test-harness/prd.md`](../06-24-simulateddevice-software-adb-test-harness/prd.md)
> 前置：Phase A（`06-24-sim-phase-a-handshake`）必须先合入——复用其 `SimulatedDevice`/`DeviceProfile`/`Scenario`/`ChunkedTransport` 骨架。

## Goal

在 Phase A 的握手地基上，把 `SimulatedDevice` 扩成完整 session 状态机，并用 `ChunkedTransport` 填充字节层取消安全场景。穷尽 OPEN/accept/路由/流控/字节流/拆除/liveness 的所有边界——这些今天**完全没有端到端测试**（只有 I/O-free 的纯函数单测）。仍是纯客户端，不需 `server`。

## Scope

- **`SimulatedDevice` session 反应**：OPEN→OKAY(grant)|CLSE、host-OKAY→credit 记账、WRTE→回 OKAY、CLSE→EOF、device 主动 OPEN（accept 路径）、双 OKAY、早 CLSE。
- **`ChunkedTransport` 故障场景**（字节层，驱动活的 reader/writer loop）：
  - 跨读超时的半帧投递（B4 取消安全：帧被 1s 读超时切开，重组后下一帧仍解码正确，无 `ConversionError` desync）。
  - 一次读返回 >1 帧（B5 bulk-IN 过量投递/coalescing）。
  - 写到第 k 字节失败（B7 截断毒化）、写起始背压 0 字节提交→可恢复 `WriteTimeout`（B9）。

## 必须穷尽的边界（来自 protocol-state-edges.md）

- **OPEN**：OPEN-1..8（OKAY 成功、早 CLSE 快速失败非 10s 超时、OPEN 超时、register-before-OPEN 排序、ready-OKAY、send-window 从原子 seed、写失败 unregister、ack 通道异常帧）。
- **accept（device-initiated OPEN）**：ACC-1..5（accept、send window 从 OPEN arg1 seed、窗口是连接级非 per-OPEN、ready-OKAY 入队失败 unregister、reject 路径）。
- **reader 路由 / classify**：RTE-1..12（WRTE→data、OKAY→ack、CLSE→data、device OPEN→pending、unknown 丢弃、多 local-id 交错、register-mid-frame 排序、满队列下 credit 仍记账、CLSE 置 closed flag、丢 WRTE 告警、畸形 OKAY、raw tee 正交）。
- **流控**：FC-1..14（classic 无窗、windowed 32MiB、opener 从 0 阻塞到被 credit、record_sent 扣减、耗尽→阻塞→OKAY 恢复、过量发送负窗、负 delta、边界 0/MAX_PAYLOAD/i32 范围、溢出饱和、畸形 OKAY 长度∉{0,4}、per-WRTE chunk clamp、credit 走原子非 poke）。
- **session 字节流**：SES-1..15（WRTE 投递+回 OKAY、0 字节 WRTE、部分拷贝 re-buffer、CLSE→EOF、EOF 前排空、通道断→BrokenPipe、data 通道异常 cmd、写经 writer 带 ack、窗口未 credit 阻塞、远端关闭后写、writer 队列满背压、writer task 消失、in-flight ack 错误、取消安全读不丢帧不丢 credit、写半程 drop 干净）。
- **拆除 / Drop / CLSE**：TD-1..5。
- **liveness**：LIV-1..14（ReadTimeout 非致命、InvalidIntegrity 可恢复、ConversionError 致命、oversize 致命、IO 致命、control 关闭、WriteTimeout 可恢复、其他写错误致命、is_alive 需双半、DeathSignal 唤醒/已死、握手中死亡、session 中死亡、Drop 触发 DeathSignal）。
- 超时边界用 `start_paused`。

## 守住的逃逸 bug（命名回归测试）

- **B3a** 早 CLSE 快速失败（OPEN 被拒不再 10s 静默超时）。
- **B8** 半开连接 `is_alive()==false`（writer 死、reader 仅 idle 不得复用）。
- **B-recv** 短 sync 帧不再 panic（截断 DONE trailer）。
- **B4/B5/B7/B9**（经 `ChunkedTransport`，消费侧）：reader/writer loop 在半帧/过量/截断/背压下行为正确。

## Acceptance Criteria

- [ ] 上述 OPEN/ACC/RTE/FC/SES/TD/LIV 全部边界各有经活 `PersistentConnection` 的端到端测试。
- [ ] `ChunkedTransport` 能制造半帧跨超时、单次读多帧、写到第 k 字节失败三类字节流，并驱动 reader/writer loop 断言消费侧不变量。
- [ ] B3a/B8/B-recv/B4/B5/B7/B9 各有命名回归测试。
- [ ] `cargo test`（default 与 `--features test-support`）+ `cargo clippy --all-targets` 全绿。

## Out of Scope

- `SimDeviceBackend` / `SimRegistry` / 前端 / `TransportReset` / host 协议对齐 → Phase C。
- 真实 USB 字节边界、真实 TLS、真实 OS 错误码（仍归硬件/字节层既有测试）。

## Technical Notes

- session 测试需要活的 `reader_loop`/`writer_loop` 跑在 sim 之上（不只是 I/O-free 的 `classify_message`/`await_open_response`/`FlowControl`）。
- 复用 `encode_okay_payload`/`parse_okay_delta`/`INITIAL_DELAYED_ACK_BYTES`/`MAX_PAYLOAD`，绝不重实现流控记账。
