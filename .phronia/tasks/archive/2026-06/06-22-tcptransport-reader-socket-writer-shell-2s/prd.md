# PRD：TcpTransport 读写半边拆分，消除 reader 持 socket 锁阻塞 writer

## 背景 / 问题

业务方在 `5b39b2d` 复测：TCP/IP 设备的交互式 `adb shell` 每键回显恒定 ~2000ms（USB 对照仅 ~3ms）。
报告：`/private/tmp/adboost-bug-report-tcp-shell-lag-rootcause.md`（PTY 逐键计时，per-char `1153 2010 2008 2010 ...` 平坦 2s 台阶）。

此前三轮 `set_nodelay` 修复方向正确但**没命中本瓶颈**——Nagle 只叠加一个局域网 RTT（ms 级），绝不会产生恒定 2s 台阶。已逐行核对代码，真因属实：

### 根因（设计层，两层叠加）

1. **单 socket 单锁**：`TcpTransport` 是 `#[derive(Clone)]`，字段
   `current_connection: Option<Arc<Mutex<Option<CurrentConnection>>>>`
   （`tcp/tcp_transport.rs:101,106`）。reader_loop 与 writer_loop 各持一个
   `transport.clone()`（`usb/persistent.rs:489-490`），clone 共享**同一把 `Mutex`**。
2. **reader 持锁跨整个读超时 await**：`read_message_with_timeout`
   （`tcp_transport.rs:185-216`）先 `lock().await` 拿锁，再跨整个
   `read_exact_timeout(read_timeout)` await 持锁，直到函数尾才 drop guard。
   reader_loop 每轮 `read_message_with_timeout(1s)`（`usb/persistent.rs:1176`），
   这把锁几乎一直被 reader 占满，只在每个 1s 读超时到期的瞬间释放。
3. **writer 被串行化**：`write_message_with_timeout`（`tcp_transport.rs:238`）抢**同一把锁**，
   每次发送最坏等一个完整读超时窗口。交互式 shell 是「写一键→等回显」的乒乓，
   每个字符 writer 都要抢锁、撞上 reader 持锁读 → 恒定 ~2s 台阶。批量输出是连续 WRTE 流，
   锁竞争被摊销，所以不暴露。

### 为什么 USB 不受影响（对照即正解）

`usb/persistent.rs:485-488` 注释：reader/writer 各持 `transport.clone()`，
「Both share the same underlying `Arc<DeviceHandle>` but use the **separate endpoint
locks**, so reads never block writes」。USB 的 bulk-IN / bulk-OUT 是两个不同端点、
两把锁，读不阻塞写。**TCP 只有一个 socket、一把 Mutex，这个前提不成立。**

### 使用方（xdb）无法修复

锁、reader/writer 任务、读超时全在 adboost `TcpTransport` / `PersistentConnection` 内部，
注入的 `DeviceBackend` 触不到底层 socket 锁。只能在 adboost 修。

## 目标

把 `TcpTransport` 的全双工 socket 拆成**独立的读半边 / 写半边，各自一把锁**，使 reader 的
阻塞读不再阻塞 writer——与现有 USB「双端点双锁、读不阻塞写」语义对齐。交互式 shell 每键延迟从
~2s 降到与 USB 同量级（局域网 RTT + 处理，ms 级）。

## 非目标 / 明确不做

- **不回滚 `set_nodelay`**：它修的是另一个真实问题（Nagle/RTT），与本 bug 正交，保留。
- **不缩短 reader 读超时**（方向 2）：split 后读写各一把锁、互不阻塞，根因已消除；缩短超时是治标
  且引入忙轮询。已与维护者确认：纯 split，不动 1s 超时。
- 不改 `ADBMessageTransport` trait 契约（仍 `Clone` + `&mut self` 读写）、不改 USB 路径、
  不改 persistent reader/writer loop。

## 方案：`tokio::io::split` 统一拆分（已与维护者确认）

对 `CurrentConnection` enum 统一 split，Tcp / Tls 两臂走同一套代码（TLS 路径统一性优于
`TcpStream::into_split`，后者会让 TCP/TLS 两条路径分叉）。

### 数据结构

`CurrentConnection` 需实现 `tokio::io::AsyncRead` + `AsyncWrite`（按 enum 委派给内层
`TcpStream` / `TlsStream`）。两个内层类型都 `Unpin`，故 `CurrentConnection` 也 `Unpin`，
poll 委派用 `self.get_mut()` + `Pin::new(inner)` 即可，**无需 `unsafe`**（库 `#![forbid(unsafe_code)]`）。

`TcpTransport` 字段由单一连接锁改为读/写两个半边锁：

```rust
// 之前：current_connection: Option<Arc<Mutex<Option<CurrentConnection>>>>
read_half:  Option<Arc<Mutex<Option<ReadHalf<CurrentConnection>>>>>,
write_half: Option<Arc<Mutex<Option<WriteHalf<CurrentConnection>>>>>,
```

`#[derive(Clone)]` 后 reader/writer 各持 clone，共享这两个 Arc；reader 只锁 `read_half`、
writer 只锁 `write_half`——两把独立锁，互不阻塞。半边包在 `Option<_>` 内，便于 TLS upgrade 时
`take()` 出来 unsplit。

### 各方法改动

- `connect`：建 `TcpStream` → `set_nodelay(true)`（保留）→ 包成 `CurrentConnection::Tcp`
  → `tokio::io::split` → 两个半边各自 `Arc<Mutex<Option<_>>>` 存入。
- `read_message_with_timeout`：锁 `read_half`，在 `ReadHalf`（AsyncRead）上 `read_exact` +
  `tokio::time::timeout`，超时仍返回 `RustADBError::ReadTimeout`（保持读超时契约，见 wire-protocol spec）。
- `write_message_with_timeout`：锁 `write_half`，在 `WriteHalf`（AsyncWrite）上 `write_all` + `flush`。
- `disconnect`：锁 `write_half`，对写半边 `shutdown()`。
- `upgrade_connection`（**关键，保持 STLS 时序契约**）：锁 read+write 两个半边 → `take()` 两半 →
  `ReadHalf::unsplit(write_half)` 还原 `CurrentConnection`（必须是 Tcp，否则报「cannot upgrade
  a TLS connection」并放回）→ 取出 `TcpStream` 做 TLS 握手 → 包成 `CurrentConnection::Tls` →
  重新 `split` → 放回两个半边 → **释放两把锁后**再 `read_message()` 消费 post-STLS CNXN
  （与现状一致，不新增第二次读；见 wire-protocol spec「STLS upgrade」节）。

### 不变量 / 必须保持

- 读超时契约：超时返回 `RustADBError::ReadTimeout`，不是 `IOError(TimedOut)`
  （`adb_message_transport.rs` + wire-protocol spec）。
- payload 越界保护：分配前 `payload_len_within_bound(data_length)` 检查不变。
- magic-only 完整性校验：`check_message_integrity` 每帧调用不变。
- STLS 时序：upgrade 内部消费 post-STLS CNXN，调用方不得再读（wire-protocol spec）。
- 无 `unsafe`、无 item 级 `#[allow]`、`clippy::pedantic` 干净（quality-guidelines）。

## 验收

1. **回归测试（核心）**：loopback 上构造 TcpTransport，clone 出 reader/writer 两份；
   reader 在无数据时阻塞于 `read_message_with_timeout(1s)`，并发地让 writer 发一帧 —— 断言写在
   远小于读超时（如 < 200ms）内完成，证明读不阻塞写。在旧单锁实现下此测试会超时/接近 1s。
2. 保留并通过现有 `connect_sets_tcp_nodelay`（nodelay 仍设置）。
3. TLS upgrade 路径：现有 STLS 行为不回归（依赖既有契约 + 测试）。
4. 质量门：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全绿；
   `cargo build -p adb_cli` 仍编过。

## 风险

- `CurrentConnection` 的 AsyncRead/AsyncWrite 委派需确认 `TlsStream<TcpStream>` 为 `Unpin`
  （tokio_rustls 在底层 IO Unpin 时成立）；若不成立则退而用 `pin-project`（无 unsafe）。
- unsplit 要求两半来自同一次 split；TLS upgrade 时务必先 `take()` 两半再 unsplit，放回后再释放锁。
