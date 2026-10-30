# forward/reverse 最优性 + async 战略：综合结论

> 来源：第二轮调研 workflow `wvaqnm827`（forward 设计 + async 战略 + issue #208/#201/#63/#147/#148 网络调研 + 综合）。
> 全量原始输出：`research/_material/workflow2-raw-output.json`。

## 执行摘要

- **forward 原语已经最优；reverse 还没建（且正确地不该是 crate 的职责）。** `open_session(TcpConnect(port))`（`persistent.rs:275-343`）就是对的自包含 forward 原语——无需外部 adb server，按 local_id 多路复用。host 侧 TCP listener 和 device-OPEN reverse handler 属于 **xdb server 层**，不属于 crate。
- **`ADBServerDevice::forward()`（`host:forward:` 代理路径）对 server 目标是死重，但保留它。** 它依赖外部 adb daemon，所以 xdb-as-server 绝不能用它；但它仍是合法的 **CLI-client 回退**（xdb 连别人的 adb server 时用）。别删，标注"client mode"，禁止 server 路径调用它。
- **async 建议：现在 `sync_core_async_wrapper`（C1 门面），协议 Asks 落地后做 sans-io async core（C2）。** 不是 pure-async（A 破坏 adb_cli + pyadb_client、XL 工作量、xdb 现在不需要），不是 sync+async 双轨（B、XL 维护 + 漂移风险）。
- **async 决策不阻塞 6-Ask MVP。** Ask #1/#2/#3 是活在同步 core 里的协议正确性工作，是 async core 的**地基/前提**，不是竞争对手。同步先行。
- **fork 在 API 哲学上已永久分叉，且这是对的。** 上游以"不愿暴露内部 API"为由拒了 PR #208，且对 #201/#63/#147/#148 全程零回应。xdb-as-server **必须**暴露 transport 原语。forward/async 表面无法上游化；窄的 bug 修复（帧错序/session 复用 #175/#207）仍值得回馈。
- **给用户的唯一真岔路：scale 赌注——"现在做 async core" vs "async 永不（同步 + spawn_blocking 直到设备数逼迫）"。** 其余决策都有单一可辩护答案。

## forward/reverse 裁决

**两条 forward 路径，区别就是重点：**

| 路径 | 机制 | 需外部 adb server? | server 时代角色 |
|------|------|-------------------|----------------|
| `host:forward:` 代理 | `ADBServerDevice::forward()` → `host:forward:{local};{remote}`（`adb_local_command.rs:70-71`）| **需要** | **server 死重**，仅留作 CLI-client 回退 |
| `open_session(TcpConnect)` | `PersistentUsbConnection::open_session(TcpConnect)` → `tcp:{port}`，返回 `MultiplexedSession` | **不需要** | **server 的 forward 原语**，按 local_id 多路复用 |

**事实核查（重要）**：forward agent 列了一条 gap"MultiplexedSession split halves 未导出"——**这条是陈旧的、已关闭**。综合 agent 自我修正并经我核对：`usb/mod.rs:7-9` 已 `pub use ... {MultiplexedSession, PersistentUsbConnection, SessionReadHalf, SessionWriteHalf}`，`into_split()` 在 `persistent.rs:433`，经 `lib.rs:36` `pub use message_devices::*` 可达。所以并发读写契约**已是 public**，只差给 `into_split()` 加个"stable"doc 注释——**是文档工作，不是代码**。

**权威 server 的 forward/reverse 架构：**
```
FORWARD（host 编排，crate 原语今天已足够）：
  tokio TcpListener on local port X
    └─ 每 accept(): open_session(TcpConnect(Y))
         └─ MultiplexedSession.into_split() → (read half, write half)
              └─ 中继字节 ↔ TcpStream；session drop → CLSE（Drop impl）

REVERSE（device-originated —— 不在 crate，属于 xdb）：
  device 注册 reverse:forward → 发 OPEN
    └─ persistent reader_loop 按 local_id demux OPEN  ← Ask #2
         └─ xdb reverse-registry 路由到 per-service handler
              └─ handler 连 reverse target（如 localhost:22）
                   └─ 桥接 WRTE↔data + OKAY 流控  ← Ask #3 raw relay
```

**与前一轮 6-Ask 的关系**：reverse 的 crate 侧使能器 = Ask #2（reader_loop demux inbound OPEN）；真正 relay 的剩余缺口 = Ask #3（暴露底层 `Receiver<ADBTransportMessage>`，而非只有 `Read`/`Write`）。crate 提供 demux + raw channel，**xdb 拥有 reverse registry 与 per-service handler**。

## async 裁决：选 `sync_core_async_wrapper`（C1 → C2）

**5 个选项的工作量/收益（来自调研）：**

| 选项 | 工作量 | 关键裁决 |
|------|--------|---------|
| A 纯 async | L–XL + **破坏性** | 删掉所有 spawn_blocking、原生 nusb、单 task/device；但 adb_cli 要塞 `#[tokio::main]`、pyadb_client 要管 runtime → 对两个同步消费者更糟 |
| B sync+async 双轨 | XL | 2× API 表面、逻辑重复、漂移风险；不可取 |
| **C1 async 门面** | **S（2-3 周）**| 薄门面把 spawn_blocking 收成一处 owned 边界，停止 xdb ~40 处手搓 spawn_blocking；无扩展性增益但低成本、可渐进 |
| **C2 sans-io core** | **L（6-8 周）**| 抽出 `AdbStateMachine`（帧/CRC、OPEN/OKAY/WRTE/CLSE FSM、delayed_ack 窗口），同步 `USBTransport` adapter 不变（CLI/Python），加 `AsyncUSBTransport` 用原生 nusb future；可测、可扩到万级设备 |
| D 永远同步 | 0 | spawn_blocking 1-10% 税永存；~100 设备封顶；nusb async 能力浪费 |

**为什么 C（grounded in 四个约束）：**
1. **nusb 是 async-native 但被压抑**（`usb_transport.rs` `.wait()`+`transfer_blocking`，`:193/:364/:73/:227/:232`）→ C2 的 `AsyncUSBTransport` 直接 `endpoint.transfer(...).await`，**零新依赖**。
2. **xdb 全 tokio**，现在每调用裹 spawn_blocking（impedance：1000 并发 session ≈ 40000 个 blocking task + writer `Arc<Mutex>` 争用 + stop-and-wait 阻塞 worker）。C1 把它收成显式门面，C2 删掉热路径的阻塞税。
3. **双同步消费者是反对 A/B 的决定性约束**：adb_cli + pyadb_client 零 spawn_blocking，纯 async 强迫它们穿 runtime → 严格更糟。C 保留它们要的同步 core。
4. **sans-io（C2）是正确终局**：协议状态机与 I/O 解耦（quinn-proto/quinn 形态），让 Ask #1/#2/#3 无需 USB 硬件即可单测。

**async 如何与 Ask #1（delayed_ack）/ 单 reader 约束交互**：
- delayed_ack 窗口在当前同步 stop-and-wait 写路径（`persistent.rs:642-644`）下**无法发挥**。应把**窗口化逻辑实现在 sans-io core 的状态机里**（写一次），让 async adapter 成为真正 pipeline N 个 WRTE 的消费者。
- 单 reader 约束（1 OS 线程/设备，`persistent.rs:86`）在 ~100 设备 OK，万级不行。C2 换成 1 tokio task/设备 + per-session channel——**demux 逻辑（按 local_id）完全相同，只换 I/O 驱动**。所以 Ask #2 的 reader_loop 重设计要**结构化成 I/O 驱动可替换**，做一次即可让 C2 变便宜。

## async 如何重塑 6-Ask MVP 计划

**基本不重排——它澄清了 Asks 是 async core 坐落其上的地基：**
- **Ask #1 现在就做，同步，但作为状态机改动而非写路径 hack**：把 `:642-644` 的 stop-and-wait 改成"发 WRTE → 继续；OKAY 单独记账"。作为未来 sans-io FSM 的一部分写一次。**不要等 async，也不要在 `Write` impl 里打补丁。**
- **Ask #2 + #3 驱动 reader_loop 重设计，且该重设计 async-中立**：同步/async demux 逻辑相同，只换 I/O 驱动。**为 #2/#3 重设计 reader_loop 一次，结构化成 I/O 驱动可替换**——这一个决定让 C2 后续变便宜。
- **Ask #3 强制一个 forward agent 低估的 public API 新增**：暴露裸 `Receiver<ADBTransportMessage>`（不只 `Read`/`Write`），relay/reverse 现在就需要，与 async 无关。
- **Ask #4/#5/#6 不受 async 战略影响**。

**"async 优先" vs "async 永不"岔路**：
- async 永不 = 同步 + spawn_blocking 永存。仅当设备数 ≲100 且单请求开销 <1% 时可辩护。
- async 优先（在 Asks 之前）= 错误顺序。会在还不存在的协议层外面建 async I/O 驱动。
- **推荐顺序 = Asks 先（同步 core，I/O 驱动可替换）→ async core（C2）后。async 是 Asks 的消费者，不是竞争者。**

## 上游/社区角度

- **PR #208 拒绝是结构性的，非风格性**：上游"不愿暴露 ADBSession 内部类型"，引导用高层 `forward()`；xdb-as-server 需要的恰恰相反（暴露 `open_session`+`TcpConnect`+裸 channel 去**实现** forward/reverse）。两种哲学不能在一个 crate 共存。
- **社区需求验证 fork 方向但不验证可上游性**：#201(USB forward)/#63(USB reverse)/#147(细粒度 remove)/#148(async for Iced) 全部确认未满足需求，且全部**数月零维护者回应**。
- **nusb-vs-rusb 分裂使分叉不可逆**：上游仍 blocking rusb 0.9.4 无 async 依赖；fork 已 async-native nusb 0.2。fork 的 async 工作（C2）结构上无法上游化。
- **值得上游的：窄的正确性修复**——帧错序/session 复用 bug（#175、PR #207："responses used X for local_id instead of Y"、"got CLSE instead of OKAY"）。**上游修 bug，fork 做 feature。**

**裁决：永久 feature/架构分叉 + 偶发 bug-fix 回馈通道。** 按独立 crate 规划路线图，停止把"与上游对齐 forward/reverse/async"当目标。

## 待用户敲定的决策（真岔路）

1. **scale 赌注——async core 现在做还是推迟？** xdb-as-server 的真实设备扇出目标？≲100 设备 → C1 门面够用、L 工作量的 C2 过早；几百-上千设备×多 session → 现在就 commit C2（1 线程/设备 + stop-and-wait 是天花板）。**这是唯一的真岔路。**
2. **delayed_ack 窗口范围（Ask #1）：仅正确性 还是 真 pipeline 吞吐？** 前者是同步 core 小修；后者强烈主张把窗口逻辑建进 sans-io FSM（即把 C2 拉前）。
3. **裸 channel 暴露（Ask #3）：确认作为 public API commit？** reverse 和真 relay 需要暴露底层 `Receiver<ADBTransportMessage>`，这加深上游拒绝的分叉，是刻意承诺。
4. **reverse 归属：确认在 xdb 而非 crate？** 推荐把 reverse registry + per-service handler 放 xdb server 层，crate 只留 OPEN-demux。
5. **`ADBServerDevice::forward()` 去留：保留作 CLI-client 回退 还是 彻底切除？** 保留近零成本、保住"连外部 adb server"模式；切除简化心智模型为"xdb 永远是 server"。
6. **上游关系：明确宣布独立 crate 地位？** 决定是否保留名义上游跟踪（为 #175/#207 偶发 bug fix），这影响在 PR #198 ADBDeviceExt 重塑上要花多少力气减摩擦。
