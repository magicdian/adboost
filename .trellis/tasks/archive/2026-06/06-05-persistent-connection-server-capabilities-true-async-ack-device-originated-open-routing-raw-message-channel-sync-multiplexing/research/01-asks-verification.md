# 6 个能力 Ask 的对抗式验证（带行号证据）

> 来源：只读调研 workflow `wz95n7zg8`（6 个 Explore agent 逐条对抗式验证 `persistent.rs` 当前源码）+ 我本人通读 `persistent.rs` 全文交叉核对。
> 行号已对当前源码复核（需求文档里的行号是旧快照，部分已漂移）。

## 裁决总表

| Ask | 裁决 | 严重度 | 主战场（全部 XPENG-only，零上游合并成本）| 一句话建议 |
|-----|------|--------|------|-----------|
| **#6** 诚实 banner | confirmed | high | `persistent.rs:127` banner 硬编码 | 改为可配置 `FeatureSet`，只宣告已实现的——**最先做**，它是所有人契约的闸门 |
| **#2** reader_loop 丢弃 device-OPEN | confirmed | **critical** | `reader_loop:238-264` + 单 reader `:84-88` | 加 `arg1==0` 检测 → `pending_opens` 队列 + `accept_device_connection()`，解锁 reverse/scrcpy |
| **#1** delayed_ack 流控（stop-and-wait）| confirmed | high | 写半 `:615-666`/`:744-802` + 读半 `:547-612`/`:673-741` | 要么摘掉 banner 里的 `delayed_ack`（诚实、便宜），要么真正做窗口；**在 #4 之前定下** |
| **#4** SYNC 未在持久连接多路复用 | partially | high | API 缺口；demux 本身已通用 `:248-257` | 加 `open_sync_session()` 薄封装；真正阻塞是 API 缺失 + 接口排他 claim，不是 demux |
| **#3** 无裸 message 通道 | partially | high | `write_message` 已 pub；reader 独占 IN 端点 | 在 reader_loop 内加 `subscribe_raw` tee（**不要**开第二个 reader）|
| **#5** shell,v2 内层帧解码缺失 | confirmed | high | `shell_exec:345-366`、`MultiplexedSession::read:673-741` | 在 `MultiplexedSession` 之上加 `ShellV2Session` 层；参考实现已存在于 `adb_server_device_commands.rs:189-205` |

---

## Ask #1 — delayed_ack 流控是假的（CONFIRMED, high）

**证据**：
- `persistent.rs:127` banner 宣告 `delayed_ack`（连同 4 个 `sendrecv_v2_*`），但代码未实现。
- `SessionWriteHalf::write`（`:615-666`）严格 stop-and-wait：发一个 WRTE（≤65536）后 `ack_rx.recv_timeout` 阻塞等一个 OKAY（`:641-649`）。`MultiplexedSession::write`（`:744-802`）同样。
- 读半 `:547-612` / `:673-741`：每收一个 WRTE 立即合成一个**无字节计数**的 OKAY（`:573-584` / `:701-714`）。
- **OKAY 的 arg0/arg1 被完全忽略**（`:651-652`、`:783-784` 只看 command type）→ 无任何窗口跟踪。
- 初始 OKAY（`:323-330`）也无 arg0 字节计数 → 初始信用窗口从未建立。
- **我额外发现的隐患**：reader_loop 用 `try_send` 投递（`:251`、`:255`）到 64 槽 sync_channel（`SESSION_CHANNEL_SIZE=64`，`persistent.rs:31`）。真窗口化下，满 channel 会**静默丢弃** WRTE/OKAY → 数据损坏。重设计必须解决。

**问题**：吞吐被钉在 ~1 RTT / 64KB（10ms RTT ≈ 6.5 MB/s）。更严重——xdb 当 server 宣告 delayed_ack 后，Android 11+ adbd 会**激进 pipeline 多个在途 WRTE**，而本库单在途 + 单 ack 槽语义会错配窗口 → stall/deadlock/丢包。

**推荐 API**（来自调研）：per-session `FlowControl { available_bytes, bytes_sent, bytes_acked }`；OPEN 响应取初始 window；发送前查 `bytes_sent - bytes_acked < available_bytes`；收 OKAY 时解析 arg0 推进窗口；写半把 WRTE 投递给一个 sender task，不在每次 `write()` 阻塞。

**⚠️ 实现前需对 AOSP 源码定真**（调研里有矛盾，必须确认）：
1. `delayed_ack` 恰恰是**放松**"一 WRTE 一 OKAY"硬规则的特性——classic 模式才是严格 stop-and-wait。两种模式要按协商结果分别处理。
2. OKAY 携带的字节数是**累计**还是**增量**？携带在 arg0 还是 payload？需查 AOSP `packages/modules/adb/types.h` / `adb.cpp` 的 `apacket` + `A_OKAY` 处理（`send_ready` / `update_ack`）。不要凭调研二手描述拍板。

---

## Ask #2 — reader_loop 丢弃 device-originated OPEN（CONFIRMED, **CRITICAL**）

**证据**：`reader_loop`（`:210-267`）只按 `arg1`（接收方 local_id）路由到 `sessions` map（`:248-257`）。device 主动发起的 OPEN 是 `arg0=device_local_id, arg1=0`，`arg1=0` 永远不在 session map 里 → 落到 else 分支（`:258-264`）**直接 drop**。

**问题**：这是 6 个 Ask 里**唯一的活跃正确性 bug**。`reverse:`（含默认模式 scrcpy 的 `reverse:localabstract:scrcpy`）依赖 device 主动 OPEN 一条流回 host，当前架构无任何路径接住。

**协议真相**：device-originated OPEN：`A_OPEN(device_local_id, 0, "<dest>")`；host 必须回 `A_OKAY(device_local_id, host_local_id)` 接受，或 `A_CLSE(0, device_local_id)` 拒绝。

**推荐 API**：reader_loop 检测 `command==Open`（其 arg1==0）→ 路由到 `pending_opens` 队列；暴露 `accept_device_connection(timeout) -> Result<(device_local_id, service_string, channels)>`（pull 模型）或注册回调（push 模型）——**brainstorm 决定**。注意 `pending_opens` 须 bounded + 定义溢出策略（单 reader 不能阻塞在满队列上，否则 stall 所有 session）。

---

## Ask #3 — 无裸 message 通道（PARTIALLY CONFIRMED, high）

**证据**：`write_message` 实际在 `adb_message_transport.rs:29-38` 上已是 **pub trait 方法**（`shell.rs:73,78,83` 已有裸用先例）。所以"裸写"机械上已可行；真正缺的是**裸 inbound 订阅** + 旁路 session 注册表的设计。

**关键架构约束**（红队反复警告）：**同一 bulk-IN 端点只能有一个 reader**。reader_loop 通过 `usb_transport.rs:310` 的 `connection.lock()` 独占持有 IN 端点，整个阻塞 transfer 期间持锁。开第二个 reader 会 deadlock。所以裸 inbound 必须**在现有 reader_loop 内 tee**，不能新开 reader。

**推荐 API**（三选一，调研倾向 Option 3）：
1. `send_raw(msg)`（已可行）；
2. crate 内 `RelaySession`（合成 session-local OPEN/OKAY 透传）；
3. `subscribe_raw(filter) -> Receiver<ADBTransportMessage>`，reader_loop tee 匹配消息给订阅者。

**与 #2 的耦合**：#2（device-OPEN 路由）与 #3（raw tee）都是改 reader_loop 的同一个 demux switch。应作为**一次 reader_loop 重设计**完成，而非两次触碰。

---

## Ask #4 — SYNC 未在持久连接多路复用（PARTIALLY CONFIRMED, high）

**证据**：`open_synchronization_session` 只在 `ADBMessageDevice`（`adb_message_device.rs:154`），不在持久多路复用器上。xdb 现在 push/pull 另开独占 `ADBUSBDevice`（xdb 侧 `xdb-core/src/transport/usb.rs:198-224`），会**二次 claim 接口**，与 `PersistentUsbConnection` 的排他 claim 冲突。但 `open_session`（`:275`）已对 `ADBLocalCommand` 通用，demux（`:248-257`）已按 local_id 通用——所以"部分确认"：阻塞是 API 缺失 + 接口排他 workaround，不是 demux 本身。

**推荐 API**：`open_sync_session()` 薄封装 `open_session(&ADBLocalCommand::Sync)`，SYNC 子命令（STAT/LIST/SEND/RECV/DATA/DONE/OKAY/FAIL，opcode 在 `message_commands.rs:35-46`）走共享 reader_loop。client push/pull 逻辑参考 `adb_session.rs`。

**SYNC v2**：`sendrecv_v2 + brotli/lz4/zstd` banner 已宣告但缺 codec 依赖（`Cargo.toml` 无 brotli/lz4/zstd）。**建议 v1-only，从 banner 摘掉 4 个 `sendrecv_v2_*`**。

---

## Ask #5 — shell,v2 内层帧解码缺失（CONFIRMED, high）

**证据**：`open_session` 可发 `shell,v2`，但 crate 未解码内层帧。`shell_exec`（`:345-366`）注释明说返回 `None` 退出码（`:361-365`）。`MultiplexedSession::read`（`:673-741`）把所有 WRTE payload 当裸字节。

**协议真相**：shell v2 内层帧 = `[1 字节 id][4 字节 LE 长度][payload]`。id：`0=stdin, 1=stdout, 2=stderr, 3=exit_status(1字节), 4=close_stdin, 5=window_size_change(8字节)`。exit code 在 id=3 的最后一帧。

**推荐 API**：`ShellV2Session` 包在 `MultiplexedSession` 之上（保持 `MultiplexedSession` 字节透明，给 tcp/sync 复用），解析 5 字节帧头，分离 stdout/stderr，返回 `exit_code: u8`。参考实现已存在于 `adb_server_device_commands.rs:189-205`。**无现存 `shell_exec` 调用方** → 低碰撞。

---

## Ask #6 — 诚实 banner / 可配置 feature 集（CONFIRMED, high）

**证据**：`persistent.rs:127` banner 硬编码 18 个 feature，含未实现的 `delayed_ack` + 4 个 `sendrecv_v2_*`（`Cargo.toml` 零压缩依赖）+ `shell_v2`（但 `:347` 发的是 v1）。

**问题**：当 client 连真 adbd 时 adbd 是 responder，影响有限；但 **xdb 当 server 后 banner 是对外宣告依据**，宣告未实现 feature 会让客户端按其行事而崩。#1 的 deadlock 风险、#5 的协议混淆风险**都源于 line 127 这条谎言**。

**推荐 API**：`DeviceFeatureSet { shell_v2, cmd, stat_v2, delayed_ack, ... }` + `banner()` 生成 + `new_with_features()` 注入。feature 名常量化避免字符串散落。

**最高杠杆**：把 banner 收紧到只宣告已实现的，**几小时工作量即可同时给 #1/#4/#5/#6 去风险**。
