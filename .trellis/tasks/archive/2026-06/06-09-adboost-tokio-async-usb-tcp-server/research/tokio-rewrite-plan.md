# xp_adb_client → 全面 tokio async 重构方案

> 目标:把 `xp_adb_client` 重构为**原生 tokio async** 库,更贴合 Rust 现代用法、扩展性更好,作为 xdb 权威 adb server 的传输地基。
> 决策已定:**走 async 重构**。本文是落地蓝图(目标架构 + 模块改造清单 + 分阶段路线 + 风险闸门 + 测试策略)。
> 配套分析见 `ASYNC-MIGRATION-ANALYSIS.md`(收益/风险/坑点的完整论证)。
> file:line 基于 commit `8e91437`,实现前以仓库当前状态复核。日期:2026-06-08。

---

## 0. 指导原则

1. **保留 sans-io 核心,替换 I/O 适配器**(即分析报告的"选项 B",而非从零重写的"选项 A")。协议逻辑已是纯函数,async 化是给冻结的核心换 I/O 驱动,不是重写协议。
2. **core 不持有全局 runtime**:公开 API 不硬编码 `#[tokio::main]`,不藏全局 runtime。runtime 所有权永远在最外层(xdb 用自己的;未来 adb_cli sync wrapper 用私有的)。这是未来 `async core + sync wrapper` 反向包装的前提。
3. **自底向上,每步可独立交付、核心测试全程绿**。
4. **CLSE/Drop teardown 是 go/no-go 硬闸门**——做不稳就停,不硬上。
5. **MSRV / edition**:沿用 workspace 的 edition 2024 + Rust 1.88(async-fn-in-trait/RPITIT 原生可用)。

---

## 1. 目标架构

```
┌─────────────────────────────────────────────────────────────┐
│  消费者层                                                      │
│   xdb (async)        ← 原生 .await,零包装                      │
│   adb_cli / pyadb     ← 未来:async core + sync wrapper(block_on)│
└───────────────────────────┬─────────────────────────────────┘
                            │  async API: async fn / AsyncRead+AsyncWrite
┌───────────────────────────▼─────────────────────────────────┐
│  I/O 适配器层(本次重构替换的部分)                              │
│   - async reader task(替换 OS reader 线程)                    │
│   - tokio::sync::mpsc(替换 std::sync::mpsc)                   │
│   - AsyncRead/AsyncWrite impl(替换 std::io::Read/Write)        │
│   - async backpressure(timeout().await 替换 recv_timeout)      │
│   - 显式 async close() + best-effort Drop(替换 Drop 同步 CLSE) │
└───────────────────────────┬─────────────────────────────────┘
                            │  纯函数调用(不变)
┌───────────────────────────▼─────────────────────────────────┐
│  sans-io 核心层(本次重构保持不动,字节级幸存)                   │
│   - FlowControl(flow_control.rs,398 行,delayed_ack 窗口数学)  │
│   - classify_message(persistent.rs:79,reader 路由判定)         │
│   - SYNC v1 codec(sync_session.rs:classify/encode_sync_header) │
│   - shell-v2 帧解码(shell_v2_session.rs:decode_frame_header)   │
│   - banner/feature 协商(DeviceFeatureSet)                      │
└───────────────────────────┬─────────────────────────────────┘
                            │  endpoint.transfer().await
┌───────────────────────────▼─────────────────────────────────┐
│  USB 传输层                                                    │
│   nusb 0.2.3(async-native,解除当前的 transfer_blocking 压抑)  │
└─────────────────────────────────────────────────────────────┘
```

**不变的(资产)**:sans-io 核心 4 大块 + 45 个测试中的 29 个纯逻辑测试。
**要换的(I/O 适配器)**:reader 线程、channel 类型、Read/Write trait、背压等待、Drop 清理、nusb 调用方式。

---

## 2. 模块改造清单

| 模块 / 符号 | 当前(sync) | 目标(async) | 改造类型 | 风险 |
|-------------|-------------|--------------|----------|------|
| `usb_transport.rs:193/203/364` | `transfer_blocking` | `endpoint.transfer().await` | 解除压抑 | 低 |
| `usb_transport.rs:73/227/232` | `.wait()`(list/open/claim)| `.await` | 解除压抑 | 低 |
| `persistent.rs:128/213/469` reader | `thread::spawn` + 阻塞 read loop | 一个 tokio task,`transfer().await` 循环 | 替换驱动 | 中 |
| `persistent.rs:79` `classify_message` | 纯函数 | **不变** | — | 无 |
| `flow_control.rs` 全文 | 纯状态机 | **不变** | — | 无 |
| 各 session 的 channel | `std::sync::mpsc::sync_channel(64)` | `tokio::sync::mpsc::channel(64)` | 类型替换 | 中(`try_send`/`send` 语义)|
| `persistent.rs:665/1154/1193` 背压 | `ack_rx.recv_timeout(10s)`(停线程)| `tokio::time::timeout(d, rx.recv()).await`(停 task)| 替换 | 中(B1 收益)|
| `persistent.rs:1262/1287/1309/1333` I/O trait | `std::io::{Read,Write}` | `tokio::io::{AsyncRead,AsyncWrite}` | 重写 impl | 高(病毒式染色)|
| `sync_session.rs:71/119` push/pull | 泛型 `R:Read`/`W:Write` | `R:AsyncRead`/`W:AsyncWrite` | 染色 | 中 |
| `shell_v2_session.rs:143/209/232` execute | `self.inner.read()` | `self.inner.read().await` | 染色 | 中 |
| `persistent.rs:809/948/1362` 三处 Drop | 同步 `write_message(clse)` + `join()` | 显式 `async close()` + best-effort Drop;`join()` → signal-and-detach | **重设计** | **高(THE 闸门)** |
| session trait(若有)| sync 签名 | `async fn`(AFIT)或 `#[async_trait]` | 签名变更 | 中(Send/dyn)|

---

## 3. 分阶段路线(每阶段独立可交付)

### 阶段 0 — 立护栏 + 选型(0.5 周)
- 确认 29 个 sans-io 纯逻辑测试(FlowControl 16 + shell-v2 9 + SYNC 4)全绿,作为整个重构的回归基线。async 后它们应**一字不改**仍通过——若需改,说明逻辑和 I/O 耦合了,立即纠正。
- 定 runtime 策略:core 不持 runtime;reader task 由调用方的 runtime 承载。
- 加 `tokio`(features: `rt`/`net`/`io-util`/`sync`/`time`/`macros`)+ 评估 `tokio-util`(`AsyncRead` 适配器)。Cargo feature gate:async 路径建议先放在 `usb` 之上,或新增 `async` feature 并存过渡。

### 阶段 1 — 解除 nusb async 压抑(最低风险,先做)(1 周)
- `usb_transport.rs`:给 `read_message`/`write_message` 加 async 路径,`endpoint.transfer().await` 替代 `transfer_blocking`;`list_devices`/`open`/`claim_interface` 的 `.wait()` 改 `.await`。
- **同步路径暂时并存**,单独验证 async transport 能 CNXN/AUTH 通。
- 交付物:一个能 async 握手的 transport,sync 路径未删。

### 阶段 2 — async reader task(1.5 周)
- `persistent.rs` 的 `thread::spawn` reader → 一个 tokio task:`transfer().await` 循环 → `classify_message`(**字节级不动**)→ `tokio::sync::mpsc`。
- 背压策略原样保留:满时 `try_send` 失败 → `log::warn!` + 丢弃,reader 绝不阻塞(**禁用 `blocking_send`**,会卡 worker)。
- 单 bulk-IN reader 约束依旧:仍是**一个** reader task 拥有 IN 端点。
- 交付物:async 多路复用 reader,demux 逻辑复用。

### 阶段 3 — CLSE/Drop 闸门(go/no-go 关口)(1-1.5 周)
- 定方案(推荐组合):
  - graceful 路径:显式 `async fn close(self)` 发 CLSE + 等确认。
  - Drop 兜底:best-effort——把 CLSE 入队给 writer task(fire-and-forget),或 writer 留 `std::sync::Mutex` 在 Drop 里做**不跨 await**的阻塞小写(单 CLSE 帧很小)。
  - `PersistentUsbConnection::drop` 的 `handle.join()` → 改为向 reader task 发关闭信号 + detach(不在 Drop 里 await task)。
- **硬闸门**:写 kill-mid-stream + drop-without-close 集成测试,证明 teardown 不泄漏 remote session。**过不了就停,回退到 sync core + spawn_blocking,不要硬上。**
- 交付物:经测试验证的 async 生命周期管理。

### 阶段 4 — AsyncRead/AsyncWrite + 上层染色(2 周)
- 实现 `persistent.rs:1262-1333` 的 `tokio::io::{AsyncRead,AsyncWrite}`(优先用 `tokio_util` / 基于 channel 的适配器,**别手搓 `poll_read`/`poll_write`**)。
- 染色传播:`SyncSession::push/pull`、`ShellV2Session::execute`(`read()` → `read().await`)。
- 3 处背压等待转 `tokio::time::timeout(..).await`——**B1 的核心收益(停 task 不停线程)在这步兑现**。
- 删除阶段 1 并存的 sync 路径。
- 交付物:全 async 的 session API。

### 阶段 5 — xdb 迁移(2-3 周,在 xdb 仓库)
- 删收益最大的 spawn_blocking:smartsocket bridge(`adb_listener.rs:710/726`)、relay monitor(`device_manager.rs:657`)→ `tokio::io::copy` / `select!`。
- 其余 44 处 spawn_blocking 逐步收。
- 退役那个独占 ADBUSBDevice 的 SYNC workaround(`xdb-core/src/transport/usb.rs`)。
- 交付物:xdb 原生 async 调用 adb_client,无 spawn_blocking 热路径。

### 阶段 6(可选,未来)— sync wrapper 回归 adb_cli/Python
- 见 `ASYNC-MIGRATION-ANALYSIS.md` §9:`block_on` wrapper(reqwest::blocking 模式)。
- 注意 4 个坑:嵌套 runtime panic、reader 需 `multi_thread` 或常驻线程、block_on 不可嵌套、Drop 字段顺序(`rt` 放最后)。

**总估**:阶段 1-4(core 库)约 6-7 周;阶段 5(xdb)2-3 周。约 18% LOC 改动,集中在 I/O 管道,协议逻辑冻结。

---

## 4. 坑点速查(详见分析报告 §6)

| 坑 | 一句话规避 |
|----|-----------|
| async-Drop 不存在 vs Drop 要发 CLSE | 阶段 3 闸门;显式 `close()` + best-effort Drop |
| `Read/Write`→`AsyncRead/Write` 病毒式染色 | 用 `tokio_util` 适配器,别手搓 poll |
| std mpsc ↔ tokio mpsc 语义 | reader 里禁用 `blocking_send`,保留 try_send + warn |
| async-fn-in-trait Send/dyn | 多线程 runtime 要 `Send`;dyn 用 `#[async_trait]` 或 RPITIT + Send bound |
| `Arc<Mutex>` 跨 `.await` 持有 | 持锁期间绝不 await;先放锁再 await |
| `block_on` / runtime 嵌套 | 重构中间态禁止在 async 里 block_on |
| 选项 A 丢 sans-io 护栏 | 走选项 B,逻辑保持纯函数,别塞进 task 闭包 |
| 取消安全 | WRTE+窗口记账在单个非 await 临界区完成,取消不留半状态 |

---

## 5. 测试策略

- **sans-io 单测(无硬件,最高优先)**:29 个纯逻辑测试是回归基线,async 后必须**不改动**通过。新增 async 逻辑也优先抽纯函数单测。
- **async 集成测试(tokio::test)**:reader task 路由、背压 timeout、`AsyncRead/Write` 往返;用内存 mock transport(实现 async transfer 的假实现)喂字节序列,不插真机。
- **teardown 测试(阶段 3 闸门,关键)**:kill-mid-stream、drop-without-close、并发 drop——断言无 remote session 泄漏、无 panic。
- **真机端到端**:reverse/scrcpy、≥100MB push/pull、shell exit code、多 session 并发吞吐。
- **质量门**:`cargo clippy --all-targets --features <async> -- -D warnings`(pedantic)+ fmt + MSRV 1.88。

---

## 6. 与上游的关系(沿用已确认策略)

- `persistent.rs` 等是 XPENG 独有文件,上游不存在 → async 重构**零上游合并成本**。
- 上游永久同步(仍用 blocking rusb,拒绝 async)→ async 工作结构上无法上游化,fork 独立领跑。
- 共享 trait(`ADBDeviceExt`)若要 async 化会碰 PR #198 的未来摩擦面——本次重构应**尽量只动 persistent 层的 async I/O,延后碰共享 trait**;若必须,届时单独评估。

---

## 7. 决策输入(开工前确认)

1. **runtime 策略确认**:core 不持 runtime(强烈建议),reader task 由外层 runtime 承载——同意?
2. **feature gate 过渡**:async 路径放 `async` feature 与 sync 并存一段,还是直接全切?(并存更稳,但维护两路一阵子。)
3. **阶段 5 时机**:xdb 迁移与 core async 化同步做,还是 core 先稳定再迁?
4. **是否立 Trellis 任务正式推进**:本文是蓝图;真正开工建议走 Trellis 流程(brainstorm → prd → 分阶段实现),每阶段一个可交付里程碑。

---

## 附:关键 file:line(同分析报告)

- reader loop:`persistent.rs:469-571`(thread 句柄 128/213)
- `Read`/`Write` impl:`persistent.rs:1262/1287/1309/1333`
- 三处 Drop:`persistent.rs:809`(connection)/`948`(SessionCleanup)/`1362`(MultiplexedSession)
- 背压等待:`persistent.rs:665/1154/1193`
- nusb 压抑:`usb_transport.rs:193/203/364`、`73/227/232`
- sans-io 纯核心:`flow_control.rs`、`persistent.rs:79`、`sync_session.rs`、`shell_v2_session.rs`
