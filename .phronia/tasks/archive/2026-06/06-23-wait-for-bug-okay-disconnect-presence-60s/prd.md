# wait-for 握手两个独立 bug：少发一个 OKAY + disconnect 靠 presence 轮询卡 60s

## Goal

修复 xdb 在 `f2af4c4` 复测发现的两个**互相独立**的 `adb root`/`unroot` 重连握手收尾 bug（路由与功能已正常，root/unroot 实际生效，设备全程在线）。两个 bug 都在 adboost frontend / backend 契约层，xdb 无介入点。

## What I already know (verified against code @ f2af4c4)

外部需求方：xdb（启用 `server` feature，自定义 `DeviceBackend` 透传，复用共享 `PersistentUsbConnection`）。

### Bug 1：`serve_wait_for` 少发一个 OKAY（framing 不符 AOSP）— 严重度低
- AOSP client 对 `wait-for-*` 读 **两个** OKAY：accept + satisfied。
- **核实属实**：`handle_client` (frontend.rs:196-251) 读完请求**直接** `dispatch_host_service`，**从不发 accept OKAY**；`serve_wait_for` 满足时只发**一个** `protocol::okay()`（disconnect 分支 :665、device 分支 :696）。
- `serve_wait_for` 注释（旧）错误假设"smartsocket 层已隐含一个 OKAY"——实际没有。
- 对照：`forward` 家族用 `protocol::okay_twice()`（protocol.rs:128，注释明确 *"Writing only one desyncs modern clients"*）。**wait-for 漏了这个双-OKAY 约定。**
- client 现象：`error: protocol fault (couldn't read status)`，秒返回。

### Bug 2：`wait-for-disconnect` 用 presence 轮询，adbd 重启不掉 USB 时卡满 60s — 严重度中
- disconnect 分支 (frontend.rs:656-674) 把"transport 断开"近似为**轮询 `list_devices()` 直到 serial 消失**（200ms / 60s 上限）。
- **核实属实**：adbd 重启 ≠ USB 物理掉线。MTK 等设备 adbd 重启后 USB 不重新枚举，serial 始终在 `list_devices()` → `absent` 永远 false → 卡满 60s。即便真掉一下，掉+回落在 200ms 间隙也漏看。
- client 现象：`restarting adbd as ...` 后卡 ~60s，用户 ^C；再跑显示已生效（证明身份切换成功、设备从未离线）。
- **根因是我自己 MVP 选型的已知 caveat**：上个任务 research `aosp-wait-for-disconnect.md` 已预警"presence 轮询 ≠ AOSP 的 transport-teardown 语义；TCP 设备 offline 但仍 listed 时不解除"。真机 MTK 证明此近似不成立。

### 关键架构事实（决定 Bug 2 可行性）
- 已有 `LifecycleEvent`（仅 `Disconnected(String)`）+ `subscribe_lifecycle` seam，frontend 用 `handle_disconnects` 消费做 forward/reverse 清理（backend.rs / frontend.rs:1455+）。
- adbd 重启时 backend 缓存的 `PersistentUsbConnection` reader 死亡 → `is_alive()` 转 false → 下次 `get_or_open` 开**新**连接（正是上个任务加的重连重试路径）。**所以 backend 能观测到"该 serial 的 transport 代际更替"，且不依赖 USB 物理重枚举** —— 这是 disconnect 应当依赖的正确信号。

## Requirements

### 模块 A：Bug 1 — wait-for 发双 OKAY
- [R1] `serve_wait_for` 满足时（disconnect 分支 + device 分支）改发两个 bare OKAY，与 `forward` 家族 `okay_twice()` 一致。**不**采用报告的"形态 A"（smartsocket 统一 accept OKAY）——因为 adboost 并非统一在 smartsocket 层发 accept OKAY，而是每个需要双 OKAY 的服务自行发（forward 即如此），统一改会破坏 forward。修正 `serve_wait_for` 的错误注释。
- [R2] **两分支语义不同（user-confirmed）**：
  - **disconnect 分支无 FAIL**：满足 + 10s 兜底**都发双 OKAY**（兜底=假定已断开、干净返回，对齐原生；附 WARN 日志，D1）。
  - **device-present 分支保留单 FAIL**：等设备出现超时是真失败（设备没来），仍发单个 `fail("wait-for timed out")`。

### 模块 B：Bug 2 — disconnect 改为事件驱动（连接死亡信号），对齐原生
研究结论（`research/native-disconnect-mechanism.md`）：原生在**连接 I/O 层**即时检测断开（读泵 adbd 关闭瞬间报错 → Kick → 移出 transport_list），非轮询；只等"旧 transport 消失"，**不等设备回来**，故亚秒返回。adboost 的等价信号 = backend 缓存的 `PersistentUsbConnection` 的 **reader 任务在 adbd 关闭时致命报错而死亡**（`persistent.rs:1116` fatal break；与 USB 物理重枚举无关）。

- [R3] **入口存活检查为主路径**（数据：死亡常先于 wait 到达）：`serve_wait_for` 进入时查 pinned serial transport 是否已不存活 → 是则立即双 OKAY。新增 `DeviceBackend::transport_alive(serial)->bool`（默认实现回退到 presence：`list_devices` 含该 serial 即视为存活，使未适配 backend 不破坏）；`DefaultDeviceBackend` 查缓存连接 `is_alive()`。
- [R4] **Form 2 推事件为次要兜底**（覆盖"wait 早于死亡"的少数情形）：新增 `LifecycleEvent::TransportReset(String)`（**不复用** `Disconnected`——后者会让 `handle_disconnects` 误释放 forward/reverse 规则）。连接层 `PersistentConnection` 持 `closed` Notify，reader/writer 任一致命退出触发；`DefaultDeviceBackend` 为每条缓存连接 spawn watcher，映射成带 serial 的 `TransportReset` publish 到现有 `lifecycle` broadcast。
- [R5] `serve_wait_for` disconnect 分支：先 `subscribe_lifecycle()` → 入口 `transport_alive` 检查 → `select!`（TransportReset(pinned) vs 10s fallback），**移除 200ms presence 轮询**。订阅必须早于入口检查（broadcast 不重放，避免 TOCTOU）。
- [R6] **不需要 generation 计数器**：PR0 数据 + server 时序证明 root 流在 wait 期间不触发 reopen，"快速重开掩盖死亡"不可能发生；入口存活检查 + 早订阅已足够。（research 称"ideally"，实测可省，设计更简。）
- [R7] **默认 trait 安全**：`transport_alive` 默认回退 presence；`subscribe_lifecycle` 默认 closed stream → 未适配 backend 落到 10s fallback（非破坏、有界，不再 60s 挂死）。
- [R8] **有界 fallback = 10s**（观测死亡 max 250ms，10s 留足余量且远短于旧 60s；仅"adbd 未真重启"时触发）。到期发双 OKAY + WARN 日志（D1）。

## Decisions (user-confirmed, 2026-06-23)

- [D1] **fallback 超时语义**：到期发**双 OKAY**（当作已断开、干净返回，贴近原生），但**必须打一条 WARN 级日志**记录"disconnect 信号未到、走超时兜底 + serial + 等待时长"，保留问题分析能力（不要静默吞掉异常）。
- [D2] **死亡检测健壮性（前置验证）**：先用当前真机做**压测**（root/unroot 反复循环 N 次），抓关键数据确认 adbd 关连接是否稳定以 fatal `UsbTransferError` 让 reader 死亡（而非静默 `ReadTimeout` 卡住）。**先压测 → 看数据 → 再决定**是否需要主动 liveness probe。压测应在实现 Bug 2 事件方案**之前**跑，验证前提成立。
- [D3] Bug 1（双 OKAY）与 Bug 2（事件信号）虽都在 `serve_wait_for` disconnect 分支，但相互独立：Bug 1 可先独立修复 + 真机验证（消除 protocol fault），Bug 2 依赖 D2 压测结论。

## Decisions (user-confirmed direction, 2026-06-23)

- [Q1→倾向形态二（推模型 `LifecycleEvent::Reconnected`）] 复用现有 `subscribe_lifecycle` seam，与架构一致。最终以研究对齐原生后定案。
- [Q2→先调研原生再设计] **关键实测数据**：原生 `time adb root`/`unroot` 通常 **<1s** 返回（0.7–0.9s），偶发 ~4s，**从不卡 60s**。说明原生 disconnect 检测是**连接层即时事件**（adbd 关闭 socket → 传输层立刻 EOF），非轮询。设计须对齐此行为：
  - 架构契合点：adboost 中 adbd 重启时 backend 缓存的 `PersistentUsbConnection` reader 任务**立刻读到 EOF 而死亡**（`is_alive()` 转 false）——这是与原生等价的即时信号源，无需 200ms 轮询。
  - 兜底超时应大幅缩短到秒级（对齐原生 ~5s 内恢复），而非 60s；presence 是否保留由研究结论定。

## Acceptance Criteria

- [ ] `ADB_TRACE=all adb -s <serial> root`：无 `protocol fault`，wait-for 连接读到**两个** OKAY。
- [ ] adbd 重启但 USB 不重枚举时，`wait-for-disconnect` 在 adbd 重连**瞬间**返回（非卡 60s）。
- [ ] `unroot` / `root` 命令干净返回，不卡、不报 protocol fault。
- [ ] 单测：双 OKAY framing；代际信号驱动 disconnect 解除（mock backend）。
- [ ] 默认 trait 实现使未适配的外部 backend 回退到旧行为、不破坏。
- [ ] fmt / clippy（默认 + server,usb）/ 全测试绿。

## Out of Scope
- 不改 xdb（仅 adboost 开契约，xdb 后续对接）。
- 不改 `list_devices()` 的物理在线语义（设备在线就如实报告，Bug 2 不靠摘设备实现）。

## Technical Notes
- 关键文件：`adboost/src/server/frontend.rs`（serve_wait_for / handle_client）、`adboost/src/server/backend.rs`（DeviceBackend trait + LifecycleEvent）、`adboost/src/server/default_backend.rs`（实现信号）、`adboost/src/server/protocol.rs`（okay_twice）。
- 真机验证不可省（USB 重枚举时序 + client 双 OKAY 读取，单测覆盖不到）。

## PR0 — 真机压测探针（已交付，D2 前置门）

`adboost/examples/root_disconnect_probe.rs`（公共 API 探针，零侵入）。复现 server 场景：一条**保活**连接上发 `root:`/`unroot:`，循环 N 次，每轮抓 5 项数据：
1. 控制服务后 `is_alive()` 是否翻 false（事件方案前提）；
2. 死亡延迟 ms：reply→`is_alive()==false`（定 fallback 上界）；
3. 死亡 vs 静默 stall 分类（决定要不要 liveness probe）；
4. serial 是否离开 USB 枚举、离开多久（证 presence-poll 不可靠）；
5. 重开+shell 可达性与耗时。
末尾打印 SUMMARY + DECISION HINTS（stalls==0 → 无需 probe；max 死亡延迟 → 定 fallback；never_left>0 → 证 presence-poll 根本性失效）。

运行：
```text
RUST_LOG=adboost=debug cargo run -p adboost --features usb,tracing-init \
    --example root_disconnect_probe -- YTGUSCNFMFAIK7ZP 20 2>probe.log | tee probe.out
```
关注：`probe.out` 的 SUMMARY；`probe.log` 里 `PersistentUsb reader error (fatal)` 是否每轮都出现（= reader 致命死亡，事件前提成立）。数据回来后据此定稿 Bug1+Bug2 完整设计。

## PR0 压测数据（真机 YTGUSCNFMFAIK7ZP，20 周期，2026-06-23）

- **reader 死亡 20/20，stall 0/20** → 事件前提成立，**无需 liveness probe**（D2 结论：用当前设备实测，stalls==0）。
- **死亡延迟 max 250ms**（avg 141，含 20ms 轮询粒度）→ fallback 超时定 **10s**（远超观测、远短于旧 60s；adbd 真重启时死亡亚秒，fallback 仅为"未重启"兜底，实际几乎不触发）。
- **19/20 serial 从未离开 USB 枚举** → 铁证 presence-poll 根本失效。1/20（cycle 19）短暂离开 469ms，但仍在 157ms 死亡 → 死亡事件对"重枚举/不重枚举"两子情形都鲁棒。
- **reopen+shell 0 失败**（487–1177ms）→ 设备总恢复，且 reopen 远慢于死亡。
- **5/20 周期 reply 为空 + 死亡延迟 0**（adbd 撕流快过应答读取）→ **证实"已发生竞态"真实且常见：死亡常先于 wait-for-disconnect 到达**。

### 数据驱动的设计定稿（简化 + 鲁棒）
1. **入口存活检查是主路径，事件是次要**：死亡几乎总先于 wait 到达，故 `serve_wait_for` 进入时先查 pinned serial 的 transport 是否已不存活——是→立即 OKAY×2（覆盖 ~95% 常见情形）。
2. **server 时序排除了 reopen 竞态**：`root:` 与 `wait-for-disconnect` 之间 client 不发任何 device 命令 → 不触发 `get_or_open` reopen → "快速重开掩盖死亡"在 root 流不可能发生 → **不需要 generation 计数器**（research 称"ideally"，数据证明可省，设计更简）。即便极端情形发生，也退化为 10s fallback→OKAY（干净返回，仅稍慢）。
3. **订阅顺序**：先 `subscribe_lifecycle()` 再做入口存活检查（避免 TOCTOU：subscribe 后的死亡事件必被捕获；subscribe 前已死则入口检查兜住）。broadcast 不重放，故入口检查对"已发生死亡"是必需的。
4. 仍发 `TransportReset(serial)` 事件，用于"wait 早于死亡到达"的少数情形（入口检查时仍存活 → select! 等事件 vs 10s）。

### 最终 serve_wait_for disconnect 分支（伪码）
```text
let mut events = backend.subscribe_lifecycle().await;     // 先订阅
if !backend.transport_alive(pinned_serial).await {        // 入口检查（主路径）
    return okay_twice();                                  // 已断开
}
let deadline = now + DISCONNECT_FALLBACK(10s);
loop select {
    ev = events.recv() => if matches TransportReset(pinned) { return okay_twice() }
    _ = sleep_until(deadline) => { warn!("wait-for-disconnect fallback fired serial=.. waited=.."); return okay_twice() }  // D1
}
```
需 backend 新增 `transport_alive(serial)->bool`（默认实现可回退；DefaultDeviceBackend 查缓存连接 is_alive）+ `LifecycleEvent::TransportReset`。

## Research References
- 复用上个任务的 `archive/2026-06/06-23-adb-root-unroot-frontend/research/aosp-wait-for-disconnect.md`（已记录 AOSP 双-OKAY 时序 + transport-teardown 语义 + presence 近似的 caveat）。
- (pending) `research/transport-epoch-signal.md` — 信号形态选型。
