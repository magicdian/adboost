# ADB 线协议真相（为 Rust 重实现定真）

> 来源：调研 workflow `wz95n7zg8` 的 2 个协议 agent（WebSearch/WebFetch AOSP + Synacktiv）。
> ⚠️ 标注了**调研内部矛盾**之处——实现前必须回 AOSP 源码定真，不要凭二手描述拍板。

## 1. 24 字节消息头（little-endian）

```
offset size field          notes
0      4    command        u32 opcode
4      4    arg0           u32
8      4    arg1           u32
12     4    data_length    u32 payload 长度（可为 0）
16     4    data_crc32     u32 payload 字节简单求和 mod 2^32（非多项式 CRC）
20     4    magic          u32 = command XOR 0xffffffff
24+    N    payload
```

opcode：`A_CNXN=0x4E584E43, A_AUTH=0x48545541, A_OPEN=0x4E45504F, A_OKAY=0x59414B4F, A_WRTE=0x45545257, A_CLSE=0x45534C43, A_STLS=0x534C5453`。
（与 fork `adb_transport_message.rs` / `message_commands.rs` 一致——可交叉核对。）

## 2. OPEN/OKAY/WRTE/CLSE 握手与 id 分配

- `OPEN(local_id, 0, "<dest>\0")`：local_id 必须非零，arg1=0。
- `OKAY(local_id, remote_id, "")`（=READY）：双方 id 均非零，建立双向映射。
- `WRTE(local_id, remote_id, data)`：收到 READY 前不能发。
- `CLSE(local_id, remote_id, "")`：remote_id 必须非零；失败 OPEN 的拒绝 CLSE 中 local_id 可为 0。
- id 是**发送方相对**的：接收方收到 `OPEN(sender_local, 0, dest)`，回 `OKAY(sender_local, recipient_local, "")`。一旦建立，id 终生不变。
- 接收方对未知 remote_id 的 WRTE/OKAY/CLSE 应**忽略而非崩溃**（流已关闭的竞态）。

## 3. ⚠️ delayed_ack 流控（调研有矛盾，必须定真）

**调研给出的（部分自相矛盾的）说法**：
- 说法 A（一个 agent）："flow control 严格——sender 必须等 A_OKAY 才发下一个 A_WRTE，pipeline 有风险"。
  → 这其实是 **classic / pre-delayed_ack** 行为。
- 说法 B（另一个 agent，更可信）：`delayed_ack` 特性**放松**了"一 WRTE 一 OKAY"，允许 pipeline：
  - CNXN arg1 = maxdata（每流初始可用字节，fork 当前发 `1_048_576` = 1MB，`persistent.rs:132`）。
  - sender 维护 `available_window`，`total_unacked_bytes <= available_window` 时可连续 pipeline 多个 WRTE。
  - OKAY 更新流控窗口，**不**逐个确认 WRTE。

**两个必须回 AOSP 源码定真的点**（不要凭调研拍板）：
1. **classic vs delayed_ack 分支**：按 CNXN 协商结果决定行为。两端都宣告 `delayed_ack` 才启用窗口模式；否则退回严格 stop-and-wait。
2. **OKAY 字节计数语义**：是累计还是增量？放在 `arg0`、`arg1` 还是 payload？
   - 需查 AOSP `packages/modules/adb/`：`types.h`（`apacket`）、`adb.cpp` / `transport.cpp` 的 `A_OKAY` 处理、`send_ready()` 与 delayed-ack 的 `bytes` 字段。
   - fork 现状：CNXN arg0=`0x0100_0000`(version)、arg1=`1_048_576`(maxdata)（`persistent.rs:130-132`），但代码从不跟踪/强制 maxdata，OKAY 的 arg0 被忽略（`:651-652`、`:783-784`）。

**MAX_PAYLOAD 演进**：4K → 256K → 1M（随协议版本）。AUTH/CONNECT 包的 maxdata 为后向兼容限制在 4096。

## 4. SYNC 协议

- v1 子命令（4 字节 ASCII id + 4 字节 LE 长度）：`STAT/LIST/SEND/RECV/DATA/DONE/OKAY/FAIL`。DATA 块硬上限 **65536 字节**。
- v2：`STA2/LIS2`、`sendrecv_v2`，压缩 `brotli/lz4/zstd`。fork banner 宣告了但**无 codec 依赖**（`Cargo.toml` 无 brotli/lz4/zstd）。
- opcode 已在 fork `message_commands.rs:35-46`。

## 5. shell v2 内层帧

```
offset size field        notes
0      1    channel_id    u8
1      4    length        u32 LE
5      N    payload
```
channel_id：`0=stdin, 1=stdout, 2=stderr, 3=exit_status(payload 恰 1 字节), 4=close_stdin(len=0), 5=window_size_change(payload 8 字节 = rows_u32_LE + cols_u32_LE)`。
exit code 在 id=3 的最后一帧、CLSE 之前送达，即使成功（exit 0）也送。
fork 已有参考实现：`adb_server_device_commands.rs:189-205`（解析 id=1/2/3）。

## 6. device-originated OPEN / reverse / 默认 scrcpy

- device 主动：`A_OPEN(device_local_id, 0, "<dest>")`。host 回 `A_OKAY(device_local_id, host_local_id)` 接受，或 `A_CLSE(0, device_local_id)` 拒绝。
- reverse 建立：client 发 `reverse:forward:<remote>;<local>`（如 `reverse:forward:localabstract:scrcpy;tcp:0`），之后 **device** 对每条入站连接发 `A_OPEN` 回 host。
- dest 命名：`tcp:<port>` / `tcp::`(任意端口)、`localabstract:<name>`(Android 抽象命名空间 socket)、`localreserved:`、`localfilesystem:`、`vsock:<CID>:<port>`、`jdwp:<pid>`。
- **默认模式 scrcpy** 用 `reverse:localabstract:scrcpy` —— scrcpy server(device) 通过该抽象 socket 把视频流推回 host。这正是 Ask #2 必须接住 device-originated OPEN 的根本原因。

## 权威来源

- AOSP `packages/modules/adb` 的 `protocol.txt` / `OVERVIEW.TXT` / `SERVICES.TXT`（android-14.0.0_r3 tag）。
- Synacktiv "Diving into ADB protocol internals"（1-2 部）。
- fork 自身实现：`adb_transport_message.rs`、`message_commands.rs`、`adb_server_device_commands.rs`（shell v2 参考）。

> **实现前 TODO**：开 §3 的两个定真点必须用真实 AOSP 源码（android.googlesource.com）逐行核对，再敲定 `FlowControl` 的 OKAY 字节语义。这是 Ask #1 的最大不确定性。
