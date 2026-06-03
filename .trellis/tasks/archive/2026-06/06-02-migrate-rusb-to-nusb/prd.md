# Migrate USB transport from rusb to nusb

## Goal

将 `adb_client` crate 的 USB 传输层从 `rusb`（libusb 的 Rust 绑定，需 vendored C 编译）迁移到 `nusb`（纯 Rust，无 C 依赖）。目标是去掉 libusb/C 工具链依赖、获得纯 Rust 构建，同时**无缝平移现有全部 USB 功能**（设备枚举、打开、claim interface、bulk 读写、超时、persistent 多路复用连接）。

迁移须采用**兼容层 / adapter 适配器**策略，而非破坏性替换：保持现有公开 API（尤其 `USBTransport`、`new_from_device`）形态尽量稳定，以便未来从上游 `adb_client` 合入改动时摩擦最小。

## What I already know

### 现状（rusb 0.9.4, vendored）
- rusb 仅在 `adb_client` crate 声明（`adb_client/Cargo.toml:55`），feature gate `usb = ["dep:rusb"]`（`Cargo.toml:23`）。下游 `pyadb_client` / `adb_cli` / `examples/mdns` 只透传 `usb` feature。
- 引用 rusb 的源文件仅 3 个：
  - `adb_client/src/message_devices/usb/usb_transport.rs`（核心传输）
  - `adb_client/src/message_devices/usb/utils.rs`（枚举 + 设备识别 + 字符串描述符）
  - `adb_client/src/error.rs:81`（`UsbError(#[from] rusb::Error)`）
- **仅使用 bulk 传输**（`read_bulk`/`write_bulk`），无 control/interrupt。
- **未使用 hotplug**（一次性主动枚举 `devices().iter()`）。
- **未使用** `open_device_with_vid_pid`（手动枚举比对 vid/pid）。
- **纯同步**，无 async runtime。并发靠 `std::thread` + `std::sync::mpsc` + `Arc<Mutex>`（`persistent.rs`）。
- 读写是两个不同的 endpoint（IN/OUT 分离，`find_endpoints` 按 `Direction` 分类）。
- 超时重度依赖：每次 `read_bulk`/`write_bulk` 传 `Duration`；上层 100ms 排空（`persistent.rs:109`）、1s reader 轮询（`persistent.rs:210`）、10s auth/OKAY 等待。
- 超时判断靠**字符串匹配** `"timed out"` / `"Timeout"`（`persistent.rs:215`）—— reader loop 用它区分"正常超时继续循环" vs "断连退出"。
- 设备识别：`is_adb_device`（`utils.rs:80`）按 interface class/subclass/protocol：`(0xFF, 0x42, 0x01)` 或 `(0xDC, 0x02, 0x01)`。
- `USBTransport` 当前 `#[derive(Clone)]`，靠 clone `Arc<DeviceHandle>` 让 reader thread 与 writer 共享 handle。

### nusb 侧能力确认（0.2.3, 纯 Rust）
- `nusb::list_devices().wait()` → 按 `vendor_id()`/`product_id()` 过滤 → 平替枚举。
- `device_info.open().wait()` → `device.claim_interface(n).wait()`。
- **`endpoint.transfer_blocking(buf, timeout) -> Completion`**：原生带超时阻塞 bulk 传输，超时返回 `TransferError::Cancelled`，**无需 async runtime / block_on**。
- `endpoint.wait_next_complete(timeout)`、`io::EndpointRead/Write`（实现 `std::io::Read/Write`，`reader(n).with_read_timeout(d)`）。
- `Endpoint<EpType, Dir>` 是 `&mut self` 独占模型，**不可 Clone**；IN/OUT 是两个独立 Endpoint。
- Windows 用 WinUSB，Linux usbfs，macOS IOKit。
- 无 `read_*_string_ascii` 便捷方法，需用 `get_string_descriptor` 自行读取+解码。

### 平台验证
- 目标 ADB 设备在 Windows 上确认为 **WinUSB 设备**（设备管理器制造商 = "WinUSB 设备"）→ nusb 可直接打开，坑 6 已排除。

## Decisions (confirmed by user)

1. **兼容层策略 = 方案 1（薄适配层 + 保留同名公开 API）**（坑 2）：不做破坏性替换。`USBTransport` 内部字段换 nusb 类型，对外方法名/语义尽量不变。`new(vid, pid)` 不变；`new_from_device(rusb::Device<Context>)` 因 rusb 类型被移除而不可避免改签名 —— 改为吃 `nusb::DeviceInfo`，保留"从已枚举设备构造"语义。trait 双后端方案（2）因维护成本过高被否决。
2. **endpoint 所有权 = 方案 1（读写端点拆到不同所有者）**：`connect()` 后 IN endpoint 移交 reader thread，OUT endpoint 留 writer 侧（`Arc<Mutex>` 串行写）。`USBTransport` **不再 `#[derive(Clone)]`**（用户已接受）。仅 `persistent.rs:78` 的 reader `transport.clone()` 受影响 —— 改为显式读/写端点分发；非 persistent 路径 `adb_usb_device.rs` 不依赖 `Clone`，无影响。整个 transport 包 `Arc<Mutex>` 共享读写（方案 2）因 reader 长持锁阻塞 writer、引入死锁/饥饿风险被否决。
3. **平台**：Windows WinUSB 已验证可用。
4. **硬约束（用户）**：重构不得改变现有用户体验的功能逻辑 —— 对外行为（CNXN/AUTH 握手、shell、persistent 多 session 多路复用、超时行为、`find_all_connected_adb_devices`/`get_single_connected_adb_device` 结果）必须与迁移前完全等价。

## Open Questions

- （见下方逐条 brainstorm）

## Requirements (evolving)

- [ ] 用 nusb 替换 rusb 实现 `USBTransport` 的：枚举、打开、claim/release interface、find_endpoints、bulk 读、bulk 写（含零长包逻辑）。
- [ ] 保留 per-call 超时语义（用 `transfer_blocking(buf, timeout)`）。
- [ ] 修正超时判断：从字符串匹配改为对 nusb 错误类型/`TransferError::Cancelled` 的结构化匹配（`persistent.rs:215`）。
- [ ] `error.rs` 的 `UsbError` 改用 nusb 错误类型。
- [ ] `utils.rs` 枚举 + `is_adb_device` + 字符串描述符迁移。
- [ ] 重构 `USBTransport` 以适配 nusb endpoint 独占/不可 Clone 模型，同时保持 reader/writer 双所有者并发能力。
- [ ] 兼容层：保持公开 API 稳定。

## Acceptance Criteria (evolving)

- [ ] `cargo build -p adb_client --features usb` 通过，无 libusb/C 依赖。
- [ ] 真机（Windows WinUSB ADB 设备）上：枚举到设备、CNXN+AUTH 握手成功、shell 命令往返成功。
- [ ] persistent 多路复用连接（多 session）功能正常。
- [ ] reader loop 超时不再误判为断连。
- [ ] **行为等价**：对外公开 API（`USBTransport`、`ADBUSBDevice`、`find_all_connected_adb_devices`、`get_single_connected_adb_device`、`PersistentUsbConnection`）的行为与迁移前一致（仅 `new_from_device` 参数类型由 rusb 换 nusb 这一不可避免的 breaking）。
- [ ] `Cargo.toml` 中 rusb 依赖被移除，新增 nusb。

## Definition of Done

- 纯逻辑部分加单测（CI 可跑、不碰真实 USB）：错误类型映射、超时判断（`TransferError::Cancelled` → 继续循环）、消息拼接/零长包逻辑等。
- USB I/O 部分靠真机手测验证（Windows WinUSB ADB 设备）。
- Lint / typecheck green (`cargo clippy --features usb`, `cargo build --features usb`)
- 真机验证通过（shell / persistent 多 session / 枚举）
- spec 更新（迁移中发现的约定/坑点）

## Verification Strategy (confirmed by user = option 3)

1. **单测兜底**（CI 可跑）：重点覆盖 `persistent.rs:215` 的超时判断改写（字符串匹配 → nusb 错误类型匹配）、`error.rs` 的错误映射、`write_bulk_data` 的零长包分块逻辑。
2. **编译全绿**：`cargo build/clippy -p adb_client --features usb`。
3. **真机手测**（用户执行）：Windows WinUSB ADB 设备上验证枚举、CNXN+AUTH、shell 往返、persistent 多 session。

## Out of Scope

- 引入 async runtime / 把项目改造成 async（保持纯同步）。
- 实现 hotplug（当前未用）。
- control/interrupt 传输。
- 改动下游 `pyadb_client` / `adb_cli` 的对外行为。

## Technical Notes

- nusb 0.2.3 docs：`Endpoint::transfer_blocking`、`io::EndpointRead/Write`、`list_devices`。
- 关键风险点：超时字符串匹配失效、endpoint 独占模型对 `Clone` 的冲突、IN transfer 的 `requested_len` 须为 max_packet_size 整数倍。
