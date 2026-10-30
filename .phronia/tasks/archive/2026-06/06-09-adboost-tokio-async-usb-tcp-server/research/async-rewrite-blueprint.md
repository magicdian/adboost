# adboost async 重构实现蓝图（agent 注入用）

精炼自调研报告（`research/recon-report.json` 全文）+ 蓝图（`research/tokio-rewrite-plan.md`）。决策见 `prd.md`。

## 不可违背的原则

1. **sans-io 核心冻结**：以下纯逻辑 async 后**一字不改**通过，是回归锚：
   - `flow_control.rs` 整模块 + 16 测试
   - `persistent.rs:79` `classify_message`、`:109` `banner_advertises_delayed_ack`、`:1234` `apply_ack`、`:1386-1549` 测试
   - `sync_session.rs` `encode_sync_header`/`SyncResponse::classify` + 4 测试
   - `shell_v2_session.rs` `decode_frame_header`/`ShellChannel::try_from` + 9 测试
   - `usb_transport.rs` `aligned_request_len`/`map_transfer_status`/`find_endpoints` + 测试
   - 协议层 `adb_transport_message.rs`/`message_commands.rs`/`models/*`/`error.rs`/`utils.rs`
2. **core 不持 runtime**：不 `#[tokio::main]`、不藏全局 runtime；reader/writer task 由消费者 runtime 承载。
3. **reader 绝不阻塞**（`persistent.rs:461` 硬不变量）：reader 路由用 `try_send`，满则丢 + `log::warn`，禁用 `send().await`/`blocking_send`。
4. **持锁/记账绝不跨外部 await**。

## nusb 0.2.3 实际 API（已核实）

队列模型，**非** `transfer().await`：
- `endpoint.submit(buf)` 提交 transfer；`endpoint.next_complete().await` 等完成（**cancel-safe**，可进 `select!`）。
- `transfer_blocking(buf, timeout)` = `submit + wait_next_complete(timeout)` 同步封装（assert `pending()==0`）。
- 高层 `EndpointRead`/`EndpointWrite`（`nusb::io`，内建 transfer 队列 + `with_read_timeout`/`with_num_transfers` + `until_short_packet`）。
- 超时：`next_complete()` 无内建 timeout → 包 `tokio::time::timeout`。`TransferError::Cancelled` 仍映射 `RustADBError::UsbTimeout`（保 `map_transfer_status` 语义）。
- **结论**：USB async 原生，**不需 spawn_blocking**。

## writer-task 模型（P1-①，核心架构）

当前 9 处共享 `writer.lock()`（`persistent.rs:347/660/708/957/1071/1179/1372`）→ 全部收敛为入队。
- 单 writer task 独占 OUT 端点，串行 `submit + next_complete().await`。
- reader task **从不写 OUT**（只 read IN + 路由）→ OUT 物理单写者，结构强制。
- **写完成语义**：OKAY/CLSE = `try_send` fire-and-forget（不需结果）；WRTE = 入队附 `oneshot::Sender`，writer task 发完回传 `Result`，`windowed_write` `.await` 回执后再 `record_sent`/报 `BrokenPipe`。

## teardown（P0-②）

- graceful：`async fn close(self)` 发 CLSE + 等确认。
- Drop 兜底：CLSE 入队 writer task（fire-and-forget）；reader `handle.join()` → `handle.abort()`。
- 三处 Drop：`PersistentUsbConnection:809`、`MultiplexedSession:1362`、`SessionCleanup:948/957-959`。

## 取消安全（P1-③）

- 窗口记账：`read_with_ack` 唯一 await 是 `data_rx.recv().await`（tokio mpsc recv **cancel-safe**，取消不取走消息）；返回后 OKAY 走 `okay_tx.try_send()` 同步无 await → recv→OKAY 之间无 await 点，窗口额度不丢。**OKAY 仍在读侧发，不改协议时序。**
- ❌ 否决"reader 收到 WRTE 即回 OKAY"：data channel 有界满则丢 WRTE（`:541`），提前 ack 掩盖数据丢失。
- 部分帧（跨多 message 的 SYNC/shell-v2 组装）中途取消 → 标记流损坏 + close（发 CLSE），不回滚。

## trait 染色（P0-③）

- 三 trait AFIT：`ADBTransport`(connect/disconnect)、`ADBMessageTransport`(read/write_message[_with_timeout])、`ADBDeviceExt`(16 方法)。
- `trait_variant::make` 生成 Send 变体（多线程 runtime 要 future: Send）+ 保 dyn/`boxed()` 对象安全（`adb_cli` 依赖 `Box<dyn ADBDeviceExt>`，但本轮 adb_cli 移出 workspace，仍应保对象安全为后续铺路）。
- `shell/exec/pull/push`（`adb_device_ext.rs:23-34,58-61`）参数破坏性改 `&mut (dyn AsyncRead+Unpin+Send)` / `Pin<Box<dyn AsyncWrite+Send>>`。
- 构造器 `new()` 调 `connect()` → 直接 async 化构造器签名（`async fn new`），不引入 builder。

## 阻塞点清单（按子系统，含 file:line）

详见 `recon-report.json` 的 `rawFindings[].blocking_points`。要点：
- **usb_transport.rs**：`:73/227/232` `.wait()`→`.await`；`:193/203/364` `transfer_blocking`→`submit+next_complete().await`+timeout。
- **persistent.rs**：`:213` thread::spawn→tokio::spawn；`:481` read 1s timeout→`tokio::time::timeout`；`:203/326/624-625` sync_channel→tokio mpsc；`:665/1154/1193` `recv_timeout`→`tokio::time::timeout`；`:1052` `data_rx.recv()`→`.await`；`:514-519/577` `sessions/raw_subscribers.lock()`→`tokio::sync` 锁或 task 私有；`:406` `thread::sleep`→`tokio::time::sleep`。
- **sync_session.rs**：`push<R:Read>`/`pull<W:Write>`→`async` + `AsyncRead/Write+Unpin`；内部 `read_exact`/`copy_payload`/`write_frame_header` 全 async。
- **shell_v2_session.rs**：`execute`/`drain_payload`/`read_exact_or_eof`→async。
- **tcp_transport.rs**：`Arc<Mutex<CurrentConnection>>`→`tokio::sync::Mutex`+`tokio::net::TcpStream`；TLS `StreamOwned`→`tokio_rustls::TlsStream`（async 握手，无 ownership swap）。
- **server**：`tcp_server_transport.rs:170` connect→`tokio::net`；`adb_server_device_commands.rs:284` `thread::spawn`+`try_clone()` relay→`tokio::spawn`+`tokio::io::split()`。

## 依赖

加：`tokio`(rt/net/io-util/sync/time/macros)、`tokio-util`(compat/AsyncRead 适配)、`tokio-rustls`(替 rustls StreamOwned)。`error.rs` 加 `tokio::task::JoinError` + timeout/cancel 变体。

## 测试策略

- sans-io 单测保留不动（回归锚）。
- async 集成测试（`#[tokio::test]`）：内存 mock transport（impl async transfer 喂字节序列），覆盖 reader 路由、背压 timeout、AsyncRead/Write 往返、writer-task WRTE oneshot 回执。
- teardown 闸门：kill-mid-stream / drop-without-close / 并发 drop → 无泄漏无 panic。
- 取消安全：mid-read_with_ack 取消不丢窗口额度；mid-frame 取消正确 close。
- 门：`cargo clippy --all-targets --all-features -- -D warnings`（pedantic）+ fmt + MSRV 1.88。
