# adboost server capability + CLI start-server/kill-server

## Goal

将通用的 **USB-backed ADB server 前端**能力下沉到 `adb_client`(adboost)库:监听本机 TCP(默认 `:5037`),实现 adb smartsocket **host 协议**服务端,并把 post-transport 的 local service(`shell:` / `tcp:`)桥接到既有的 `PersistentUsbConnection` / `MultiplexedSession`。同步给 `adboost_cli` 增加 `start-server` / `kill-server` 子命令,让 CLI 能把自己作为 adb server 跑起来,对外给原生 `adb`、`scrcpy` 等标准客户端提供服务(绕开 Google 官方 adb server 对 USB 的独占)。

需求来源:`/private/tmp/adboost-server-feature-request.md`(xdb 团队提交)。

## What I already know (codebase-verified)

- **"下半身"已具备**:`PersistentUsbConnection`(`adb_client/src/message_devices/usb/persistent.rs`)单次 claim + reader/writer 后台任务按 local_id 解复用;`open_session(&ADBLocalCommand) -> MultiplexedSession`;`MultiplexedSession::into_split()` 返回 `SessionReadHalf`/`SessionWriteHalf`(均 `AsyncRead`/`AsyncWrite`)。需求文档的接缝设计与现状完全吻合。
- **设备枚举**:`find_all_connected_adb_devices() -> Result<Vec<ADBDeviceInfo>>` 是**同步函数**(无 `.await`),需求文档 §4.6 示例写的 `.await` 有误,实现时按同步处理(或包到 `spawn_blocking`)。`ADBDeviceInfo { vendor_id, product_id, device_description }` —— **没有 serial 字段**,host 协议需要 serial,这是个 gap(见 Open Questions)。
- **`ADBLocalCommand`** 已支持 `ShellCommand(String, Vec<String>)`(空 args ⇒ `shell:` v1)与 `TcpConnect(u16)`(⇒ `tcp:<port>`),正是桥接所需。
- **命名冲突 ⚠️**:`adb_client` 已存在 `pub mod server`,内含 `ADBServer`——它是"连接到**外部** adb daemon 的 TCP **客户端**"(全程 outbound,见 lib.rs:22)。需求文档示例里的 `adb_client::server::AdbServerFrontend` 会与之**撞名**。新 server 前端模块必须用不同模块名。
- **USB feature**:USB 相关能力在 `usb` feature 下(`nusb`)。新 server 前端依赖 USB 默认 backend,但 host 协议纯函数层不依赖 USB。
- **CLI 结构**:`MainCommand` enum(`adboost_cli/src/models/opts.rs`)。现有 `host kill` 语义是"让**外部** adb daemon 退出"(`ADBServer::kill`),与新需求的 `kill-server`(停止 adboost 自身 server)**语义不同**,不可复用同一命令。
- **CLI 现状无 daemon 化**:CLI 全部是 one-shot 子命令,没有任何长驻进程 / PID 文件 / detach 机制。`start-server` 引入"长驻进程"这一全新形态。

## Assumptions (temporary)

- 落地范围以 xdb 需求文档 §6 的 Phase 1–3 为主(协议纯函数 → DeviceBackend trait + 默认 USB backend → AdbServerFrontend accept 循环 + 桥接);Phase 4(能力协商配置化)可作为 stretch。
- 默认只通告诚实最小 features 集 `cmd,stat_v2,fixed_push_mkdir,apex`(不含 `shell_v2`/`sync_v2`),强制客户端走 v1 路径。
- 新增能力 feature-gated(如 `server`),默认 off,不影响纯 client 用户。

## Decisions (resolved in brainstorm)

- **D1(落地范围)**:**Phase 1–4 全部**,并允许对现有代码做架构重构(用户明确授权)。
- **D2(CLI 进程模型)**:**后台 daemon + PID 文件**。`start-server` detach 成后台进程并写 PID 文件;`kill-server` 读 PID 文件停止它。
- **D3(架构重构 — 解决 `server` 命名冲突)**:现有 `adb_client::server`(`ADBServer`,内部方法 `proxy_connection`,本质是"代理到**外部** adb daemon 的 client")+ `server_device`(`ADBServerDevice`)**合并并重命名**为 `adb_client::proxy`:
  - `ADBServer` → `ADBProxyServer`
  - `ADBServerDevice` → `ADBProxyDevice`
  - 类型名一并改(Proxy*),`server_device` 并入 `proxy` 模块。
  - 腾出的 `adb_client::server` 路径给**新的真正 server 前端**:`{AdbServerFrontend, DeviceBackend, UsbDeviceBackend, ServerCapabilities, protocol::*}`。
  - 角色三正交:`proxy`(代理到外部 daemon 的 client)/ `usb`+`tcp`(直连设备 client)/ `server`(adboost 自己作为 adb server)。
  - 命名依据:内部方法已叫 `proxy_connection`;`relay` 被排除(xdb §5 已用作 QNX/Hyp 专有概念,会撞车)。
- **D4(serial gap)**:自行解决 —— `nusb` DeviceInfo 自带 `serial_number()`,扩展 `ADBDeviceInfo` 增加 serial 字段即可,无需额外协议往返。
- **D5(兼容)**:一次性切换,**不保留 deprecated 旧路径别名**(`server` 路径要复用给新前端,无法同时保留旧 `server::ADBServer` 别名;adboost 仍 WIP,README 已声明 larger refactor planned)。xdb 侧同步改 import。

## Decisions (continued)

- **D6(track-devices)**:`DeviceBackend::subscribe_changes` 暴露 trait 方法。默认 `UsbDeviceBackend` 用 **nusb hotplug 事件流**(比轮询优雅,nusb 0.2.3 支持),轮询作为兜底。
- **D7(host:kill)**:**可配置,默认拒绝**(server 生命周期内独占 `:5037`)。`ServerCapabilities`/config 提供开关允许接管语义。
- **D8(features 集)**:默认**诚实最小集** `cmd,stat_v2,fixed_push_mkdir,apex`,不通告 shell_v2/sync_v2。Phase 4 `ServerCapabilities` 允许 backend 实现 v2 后显式 `.with_shell_v2()` 开启。(注:协议调研 agent 建议通告 shell_v2,与需求文档 §4.3 冲突;以需求文档为准。)
- **D9(daemon 进程模型)**:避免 fork-after-tokio 死锁 —— `start-server` 用 **re-exec 自身 + 隐藏 flag 的 detached child**(`std::process::Command` + Unix `setsid`/Windows detached flags),而非 double-fork。PID 文件位置:`$XDG_RUNTIME_DIR/adboost/server.pid` → `~/.android/adboost.pid` → temp 兜底。`kill-server` 读 PID + SIGTERM(Unix)/taskkill(Windows),stale PID 检测用 `kill(pid,0)`。server accept 循环用 `tokio::signal` 优雅停机。

## Research References

- 探勘 workflow 结果(6 agent):重命名影响面(165+ 引用)、API 面核实、USB serial 枚举、adb host 协议语义、daemon/PID 设计、CLI 结构。已综合入下方 Technical Approach。

## Confirmed Facts (codebase ground truth)

- `find_all_connected_adb_devices()` 是**同步**(`nusb::list_devices().wait()`);需求文档示例的 `.await` 有误。
- `ADBDeviceInfo` 当前**无 serial 字段**;nusb 0.2.3 `DeviceInfo::serial_number() -> Option<&str>` 可用(枚举时已缓存,无需 open device)。
- `USBTransport::new(vid,pid)` 仅按 vid:pid 取**第一个**匹配,多设备同 vid:pid 无法区分 → 需 serial 寻址。
- CNXN banner **不含** adb serial(只有 build 属性 + features);adb serial = USB iSerial,必须从 nusb 枚举取。
- `PersistentUsbConnection::open_session(&ADBLocalCommand) -> MultiplexedSession`、`into_split() -> (SessionReadHalf, SessionWriteHalf)`(均 AsyncRead+AsyncWrite)、`ADBLocalCommand::{ShellCommand(_, vec![])=shell: v1, TcpConnect(p)=tcp:p}` 全部已具备。

## Technical Approach

### A. 架构重构(腾出 `server` 命名)

将 `adb_client::server`(ADBServer,代理外部 daemon 的 client)+ `server_device`(ADBServerDevice)合并重命名为 `adb_client::proxy`:
- `ADBServer` → `ADBProxyServer`;`ADBServerDevice` → `ADBProxyDevice`;`TCPServerTransport` → `TCPProxyTransport`。
- `server/commands/*`(10 impl)、`server_device/commands/*`(19 impl)、`server/models/*` 全部迁入 `proxy` 模块。
- 同步更新:lib.rs 再导出、CLI(main.rs/handlers/models)、pyadb_client(PyADBServer/PyADBServerDevice 内部类型)、benches、emulator TryFrom、两个 README、所有 doc 链接。
- models(DeviceShort/WaitForDeviceTransport/MDNSBackend 等)继续从 `proxy` 再导出。

### B. 新 server 前端模块 `adb_client::server`(feature `server`)

- `server/protocol/`(Phase 1,纯函数 + 单测):`parse_hex_len`、`encode_framed`、`transport_id_for`(排序后 1-based)、回包变体(`write_okay`/`write_okay_data`/`write_fail`/`write_okay_twice`/`write_okay_tport`)。data_length 上界校验(防 OOM)。
- `DeviceBackend` trait(Phase 2):`list_devices` / `subscribe_changes` / `open_local_service`。`DeviceEntry{serial,state,product,model,device}` + `DeviceState`。
- `UsbDeviceBackend`(Phase 2):薄封装 `PersistentUsbConnection`,按 serial 维护 `HashMap<String, Arc<PersistentUsbConnection>>`;`subscribe_changes` 用 nusb hotplug。
- `AdbServerFrontend` + builder(Phase 3):`TcpListener::bind` + accept 循环,每 client 一个 task;`handle_client` 状态机(host 服务 → transport 选择 → local service 桥接);`bridge_session` 用 `into_split()` 双向 copy。
- `ServerCapabilities`(Phase 4):可配置 features(默认诚实最小集)、version_hex、host:kill 接管策略开关、`.with_shell_v2()` 等。

### C. USB serial 寻址(支撑 B)

- `ADBDeviceInfo` 增 `serial: Option<String>` 字段 + 枚举时填充。
- `USBTransport::new_by_serial(serial)`、`PersistentUsbConnection::new_from_serial(serial, key)`。

### D. CLI daemon(feature 透传)

- 新增 `MainCommand::Server(ServerCommand<ServerManagementCommand>)`,`ServerManagementCommand::{Start{foreground,pid_file}, Kill{pid_file}}`。
- `start-server`:re-exec detached child(隐藏 `--internal-daemon` flag/env),写 PID 文件;`--foreground` 时前台阻塞。
- `kill-server`:读 PID + 信号停机。
- 与现有 `host kill`(停外部 daemon)严格区分。

## Requirements

- adb_client::proxy 重构完成,全工作区编译通过,行为不变。
- adb_client::server 前端模块(feature-gated `server`,默认 off):protocol 纯函数 + DeviceBackend + UsbDeviceBackend + AdbServerFrontend + ServerCapabilities(Phase 1-4)。
- ADBDeviceInfo serial 字段 + serial 寻址构造器。
- adboost_cli `server start` / `server kill` daemon 子命令。
- 端到端:原生 `adb devices` / `adb shell` / scrcpy 经 adboost server 连通 USB 设备。

## Acceptance Criteria

- [ ] proxy 重构后 `cargo build --workspace` + 现有测试全绿,无行为回归。
- [ ] host 协议纯函数有单元测试(三种 OKAY 变体、FAIL、transport_id 排序一致性、hex 解析、data_length 上界)。
- [ ] `cargo build -p adb_client --features server` 通过;不开 feature 时纯 client 用户不受影响。
- [ ] `adboost server start` 起服务(daemon + PID 文件),`adboost server kill` 停服务。
- [ ] 端到端:`adb devices` 列出设备、`adb shell` 可交互(真机验证)。
- [ ] clippy(pedantic)全绿;README/模块 doc 更新。

## Requirements (evolving)

(待 brainstorm 收敛)

## Acceptance Criteria (evolving)

- [ ] adboost 暴露 server 前端 API + 默认 USB backend,feature-gated。
- [ ] host 协议纯函数(`parse_hex_len` / `encode_framed` / `transport_id_for` + 回包变体)有单元测试覆盖。
- [ ] `adboost_cli start-server` 能在 `:5037` 起服务;原生 `adb devices` / `adb shell` 能连通(端到端验证)。
- [ ] `adboost_cli kill-server` 能停止该服务。
- [ ] `cargo build` / `clippy` / 现有测试全绿;纯 client 用户(不开 server feature)不受影响。

## Definition of Done

- Tests added/updated(协议纯函数单测 + 桥接/listener 可测部分)。
- Lint / typecheck / 现有测试全绿。
- README / 模块 doc 更新(新 server 能力 + CLI 用法)。
- 不破坏既有 `adb_client::server`(client)API。

## Out of Scope (explicit)

- xdb 小鹏定制:relay 到 QNX/Hypervisor、SSH 连接池、设备 probe(XOS 属性)、IPC 协议、刷机、双 transport 编排、connectivity monitor(需求文档 §5)。
- `sync:` / `shell,v2` / `reverse:` / `jdwp:` / `localabstract:` 等 local service —— 显式 `FAIL`。
- SYNC v2 + 压缩协商。

## Technical Notes

- 接缝核心:`DeviceBackend` trait(`list_devices` / `subscribe_changes` / `open_local_service`),adboost 自带 `UsbDeviceBackend` 薄封装既有 `PersistentUsbConnection`。
- 回包语义易踩坑点(需求文档 §3.1 / §4.2):三种 OKAY 变体(裸 OKAY / `tport` 8 字节 LE / forward 双 OKAY)、`FAIL`+`%04x`+reason、transport-id 单一真相来源(排序后 1-based)。
- 参考实现:xdb `sources/host/crates/xdb-core/src/server/adb_listener.rs`(~1430 行)。

## Research References

(若需调研 adb host 协议细节,补充于此)
