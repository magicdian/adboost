# forward/reverse 与 async 的硬事实（代码考古）

> 来源：本人通读 fork 的 forward.rs/reverse.rs/adb_local_command.rs/usb_transport.rs + xdb relay.rs/usb.rs + PR #208 diff。
> 这是第二轮调研 workflow `wvaqnm827` 的地基，独立于其结论先行落盘。

## A. forward/reverse：fork 里有两条概念不同的路径

### 路径 (a)：`host:forward:` 代理 —— 依赖外部 adb server
- `ADBServerDevice::forward(remote, local)`（`server_device/commands/forward.rs:9-18`）→ `proxy_connection(ADBLocalCommand::Forward)`。
- `ADBLocalCommand::Forward` 格式化为 **`host:forward:{local};{remote}`**（`adb_local_command.rs:70-71`）——这是 **smartsocket host-service 串**，发给一个**已存在的 adb server**，**不是** device transport 协议。adbd 本身不认 `host:forward:`。
- 同理 `Reverse` → `reverse:forward:{remote};{local}`（`:75-76`）——但 `reverse:` 是 **device transport service**，adbd 认。
- 即：**Forward 走 server，Reverse 走 device**。这是 ADB 的真实分层：`adb forward` 的本地监听+编排是 **adb server（host 侧）的职责**；`adb reverse` 才真正下到 device（且依赖 device-originated OPEN = Ask #2）。

### 路径 (b)：`open_session` + `TcpConnect` —— 自包含，无需 adb server（来自用户的 PR #208）
- PR #208 把 `open_session` 从 `pub(crate)` 提到 `pub`（`adb_message_device.rs:33-35`），并加 `ADBLocalCommand::TcpConnect(u16)` → `tcp:<port>`（`adb_local_command.rs:89`）。
- example `tcp_forward/main.rs`：`device.inner_mut().open_session(&TcpConnect(8080))` 直接对 device 开一条到 `tcp:8080` 的流——**这就是 `adb forward` 真正需要的 device-level 原语**（host 自己 listen + 每连接 open 一条 tcp: 流）。
- **上游拒绝 PR #208 的理由 = "不愿暴露内部 API"** → 这正是 fork 存在的根本原因（待 workflow 确认 issue 原文）。

### xdb 当前的 forward：仍依赖外部 adb server（要被取代的东西）
- `xdb-core/src/relay.rs:305-342` `spawn_relay_protocol`：直连 `adb_server_addr()`，发 `{:04x}host:transport:<serial>` + `shell:<cmd>` smartsocket 帧。
- `relay.rs:91` `adb.forward_tcp(...)`、`forward_remove` 等都走 adb server。
- **这正是"xdb 自己当权威 server"要替换掉的部分**——届时 forward = xdb 自己 listen + 持久连接 `open_session(tcp:)`，reverse = 接住 device-originated OPEN（Ask #2）。

**初步推论**（待 workflow 验证）：对"权威 server"目标，`ADBServerDevice::forward()`（`host:forward:` 代理）可能是**死重**——xdb 自己就是 server，不该再代理给另一个 server。真正需要的是 PR #208 的 `open_session(tcp:)` 原语 + host 侧 TcpListener 编排。

## B. async：fork 全同步，xdb 全 tokio，nusb 是被压抑的 async-native

### fork 100% 同步
- `adb_client/src` 零 `tokio` / `async fn` / `.await`（grep 确认）。
- 持久连接 = **一个 OS 线程**跑 reader_loop（`persistent.rs:84-88`）+ `std::sync::mpsc` channel；写是 stop-and-wait（阻塞等 OKAY）。

### nusb 0.2.3 是 async-native，但被 `.wait()` / `transfer_blocking` 压成同步
- `usb_transport.rs`：`nusb::list_devices().wait()`（`:73`）、`device.open().wait()`（`:227`）、`claim_interface(...).wait()`（`:232`）、`endpoint.transfer_blocking(chunk, timeout)`（`:193`、`:364`、`:203`）。
- 即：**async 的底层能力已经在库里，只是被同步外壳包住**。切 async 不需要换 USB 库。

### xdb 100% tokio async
- `xdb-core/Cargo.toml`：`tokio = { version = "1", features = ["full"] }`、`futures = "0.3"`。
- main.rs/core 全 async。它从 async 上下文调用**同步**的 `open_session`/read/write → **阻抗失配**（待 workflow 量化：是否阻塞 tokio worker / 是否需要 spawn_blocking / 是否每 device 一个专用 OS 线程）。

### 双同步消费者约束
- `adb_cli`（CLI）和 `pyadb_client`（Python 绑定）都是**同步**消费者。纯 async core 会强迫它们穿过 runtime。
- 成熟库的解法（待 workflow 调研）：sans-io（协议状态机与 I/O 解耦）/ 双门面（reqwest blocking 式）/ sync-core + async-facade。

## C. 对 MVP 的潜在影响（待 workflow 综合确认）
- async 决策可能**重排** 6-Ask 计划：若走 async，reader_loop 重设计（#2/#3）和 delayed_ack 窗口（#1）的实现形态都会变（单 reader future + per-session channel + 窗口化非阻塞写，天然是 async 的菜）。
- 存在"先做 async 地基 再做 6 Ask" vs "6 Ask 用同步做、async 永不/以后" 的岔路。
