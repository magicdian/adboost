# adboost server capability expansion + CLI self-test harness

## Goal

扩展 adboost `server` 前端（adboost 作为 ADB server，监听 :5037 服务外部 adb/scrcpy 客户端）的能力，
补齐 `/private/tmp/adboost-followup-capabilities.md` 列出的退化项与待补能力：
**P1 forward / P2 push-pull / P3 shell-v2 / P4 reverse 全部实现**。库侧补齐后同步在 `adboost_cli` 体现。

同时在 `adboost_cli` 新增一套**交互式 self-test**（`adboost_cli selftest` 子命令）：基于实际连接的 ADB 设备运行，
先跑自动化（非交互）测试覆盖 shell/pull/push/forward，并对 **USB 直连** 与 **经 adboost server**（进程内起
frontend + 库内 ProxyDevice 客户端）两条通道分别验证；再进入交互式测试（USB 拔插重连、设备重启恢复）。
测试结果以 **gtest 风格**在控制台逐用例输出（`[ RUN ] / [ OK ] / [ FAILED ] / [ SKIPPED ]` + 汇总）。

## What I already know（来自代码勘察，rev 5845a05）

- **P2/P3 底层积木已存在**：`PersistentUsbConnection::open_sync_session()`（`persistent.rs:1083`，返回
  `SyncSession`，已实现 SYNC v1 push/pull）与 `open_shell_v2()`（`persistent.rs:1099`，返回 `ShellV2Session`，
  已实现 v2 内层 framing 解码）。P2/P3 = backend 新方法 + frontend 放行 + 桥接，不是从零写协议。
- **P4 reverse 只有低层积木**：`incoming_opens()`（`persistent.rs:469`，设备发起的 OPEN 队列）、
  `subscribe_raw()` / `send_raw()`（raw 中继原语）。无成品 reverse 编排——需 frontend 主机侧 listener +
  设备 stream 双向编排，改动最大、放最后一个 PR。
- **frontend 退化点**：`forward:`/`killforward` → `protocol::fail("forward not supported yet")`
  （`frontend.rs:229-236`）；`serve_local_service` 对 `sync:` / `shell,` / `reverse:` / `jdwp` /
  `localabstract:` 预先 FAIL（`frontend.rs:434-446`）。
- **protocol.rs 已有 `okay_twice()`**（双 OKAY，forward 成功语义）与 `transport_id_*` helpers。
- **backend.rs**：`DeviceBackend` trait 当前仅 `list_devices` / `subscribe_changes` / `open_local_service`。
  按 FR 用「带默认实现的新方法」扩展（`open_sync_session` / `open_shell_v2` / reverse 相关），保持向后兼容。
- **capabilities.rs**：诚实最小集默认 `cmd,stat_v2,fixed_push_mkdir,apex`；有 `with_shell_v2()` / `with_feature()`。
  未实现前严禁通告 `shell_v2` / `sync_v2`——实现后才随 backend 协商通告。
- **ProxyDevice 客户端**：`ADBProxyDevice` 有 inherent `forward()/reverse()`（`proxy/device_commands/`），
  发 `host:forward:` 等 host 协议，正好用于「经 server」通道验证 P1。`ADBDeviceExt` 暴露 shell/pull/push/
  shell_command/reboot/list/stat 等（`adb_device_ext.rs`）。
- **CLI**：`server start/kill` daemon 已存在（`daemon.rs`）。`models/opts.rs::MainCommand` 列子命令；
  `models/local.rs` 已有 `ForwardCommand`/`ReverseCommand`。二进制名 `adboost_cli`。

## Requirements

### 库侧（adb_client）
1. **P1 forward**（纯 frontend）：放行 `host:forward:` / `host:killforward` / `host:killforward-all` /
   `host:list-forward`（含 `host-serial:<serial>:forward:` 变体）。主机侧 `TcpListener::bind` 本地端口，
   每个入连 `backend.open_local_service(serial, TcpConnect(remote_port))` 桥接。成功回双 OKAY（`okay_twice()`），
   port-0 自动分配回 `%04x`+实际端口。维护 forward 规则注册表（按 local port）支持 kill/list。
2. **P2 push/pull**（backend 新方法 + frontend 放行 + 桥接）：`DeviceBackend::open_sync_session(serial)`
   默认返回 unsupported，`UsbDeviceBackend` 覆盖转发 `PersistentUsbConnection::open_sync_session()`。
   frontend 放行 `sync:`，拿到 `SyncSession` 后桥接 SYNC 子协议到客户端 socket。实现后 `sync_v2` 能力声明
   按 backend 是否实现协商（诚实 banner）。
3. **P3 shell-v2**（backend 新方法 + frontend 放行 + 桥接）：`DeviceBackend::open_shell_v2(serial, cmd)`
   默认 unsupported，`UsbDeviceBackend` 覆盖转发 `open_shell_v2()`。frontend 放行 `shell,v2`，桥接 v2 内层
   framing。实现后才通告 `shell_v2`。
4. **P4 reverse**（frontend + backend 协作，最复杂）：放行 `reverse:forward:` / `reverse:killforward*`。
   frontend 主机侧编排设备 stream（`incoming_opens`）与主机 listener。诚实分阶段：若编排过重，至少做到
   forward-style 注册表 + 设备侧 reverse 请求转发；端到端打通为目标，做不到则明确 FAIL（不半实现、不虚假通告）。
5. 能力声明随 backend 协商：frontend 放行 service 后，先查 backend 是否真正实现再决定 `host:features` 通告。

### CLI 同步（adboost_cli）
6. `server` daemon 默认 capabilities 在 backend 支持时通告新能力（`with_shell_v2()` 等）。
7. 既有 `host`/`local` 代理子命令若因库能力补齐而行为变化，保持一致。

### self-test harness（adboost_cli selftest）
8. 新增 `MainCommand::Selftest` 子命令，运行 `adboost_cli selftest`。
9. **设备探测**：无设备→报错退出提示先连设备；有设备→按数量决定单设备/多设备场景用例集。
10. **自动化阶段**（无需交互）覆盖：
    - **USB 直连通道**：shell（v1 + v2 exit code）、pull、push、list/stat、forward（起隧道→程序内连本地端口→经隧道读设备端 echo 服务验证，自动化，不靠人工 nc）。
    - **经 adboost server 通道**：进程内起 `AdbServerFrontend`（绑临时端口 127.0.0.1:0），库内 `ADBProxyDevice`
      连该端口跑同组 shell/pull/push/forward。
    - **多设备**：检测到 >1 设备时，对每个 serial 走 `-s <serial>` 等价路径（指定 serial 选择 transport）自动化验证选择逻辑。
    - **官方 adb parity（可选，自动探测）**：若系统有官方 `adb` 且能起 server，同组 proxy 命令对官方 server 跑一遍作基准，
      再对 adboost server 对比；缺失则整组 SKIPPED，不阻塞。
11. **交互式阶段**（自动化后进入，逐项提示用户）：
    - USB 拔插重连：提示拔→检测消失；提示插→检测重现且可重新 shell。
    - 设备重启恢复（放最后）：触发/提示 reboot，120s 超时内未重连成功视为 FAILED。**明确排除 tcpip 重启场景**
      （tcpip 重启本就可能需要重新建立连接）。
12. **tcpip 设备**：预埋用例位，检测到 tcpip 设备输出 **SKIPPED** + 原因（后续基于 android emulator 调试，当前无环境）。
13. **gtest 风格输出**：每用例 `[ RUN      ] suite.case` → `[       OK ] / [  FAILED  ] / [ SKIPPED ]`，
    末尾 `[==========] N tests, P passed, F failed, S skipped`。失败时打印原因，整体退出码反映成败。

## Acceptance Criteria

- [ ] frontend 不再对 forward/sync/shell,v2/reverse 直接 FAIL；放行后按 backend 能力路由。
- [ ] `DeviceBackend` 新增带默认实现的方法（sync/shell-v2/reverse 相关），现有 MockBackend 无需改动仍编译（向后兼容）。
- [ ] `UsbDeviceBackend` 覆盖新方法，转发到既有 `PersistentUsbConnection` 能力。
- [ ] `host:features` 仅在 backend 真正实现对应能力时通告 `shell_v2`/`sync_v2`（诚实 banner，有单测断言）。
- [ ] P1 forward：单测覆盖 host-protocol arm（注册表 add/remove/list、双 OKAY、port-0 分配的 pure helpers）。
- [ ] P2/P3 桥接逻辑有可单测的纯函数/MockBackend 覆盖；真实设备路径由 selftest 覆盖。
- [ ] P4 reverse：端到端打通则有用例；若分阶段降级，明确 FAIL 文案且不虚假通告，且 PRD/文档记录边界。
- [ ] `adboost_cli selftest`：无设备时报错退出；有设备时按 gtest 风格逐用例输出，退出码反映成败。
- [ ] selftest 自动化阶段覆盖 USB 直连 + 经 server 两通道的 shell/pull/push/forward；多设备走 -s；tcpip→SKIPPED；
      官方 parity 自动探测缺失→SKIPPED。
- [ ] selftest 交互阶段：USB 拔插重连、reboot 恢复（120s 超时、排除 tcpip）。
- [ ] `cargo clippy`（pedantic）/ `cargo test` 全绿。

## Definition of Done

- 库侧新增能力有单元测试（host-protocol arm 用 MockBackend，pure helpers 直接测）。
- self-test harness 可在真实设备运行并输出 gtest 风格结果，退出码正确。
- 文档/README 更新（server 能力矩阵更新、selftest 用法）。
- 诚实 banner 原则：未实现/未协商的能力绝不在 `host:features` 通告。

## Technical Approach

- **向后兼容扩展 trait**：`DeviceBackend` 新方法全带默认实现（返回 unsupported 错误），避免破坏现有 backend。
- **frontend 放行 → backend 协商**：移除 `serve_local_service` 的硬拒绝，改为尝试调用 backend 新方法；backend
  返回 unsupported 则回落 FAIL。`host:features` 由 frontend 在构造时依据 backend 能力裁剪（或 capabilities 显式 opt-in）。
- **forward 注册表**：frontend 持有 `Mutex<HashMap<local_port, ForwardRule + JoinHandle>>`，listener 任务桥接。
- **selftest 架构**：`selftest` 模块内定义轻量 TestCase 抽象（name + async fn + 结果枚举 Ok/Fail/Skipped），
  runner 顺序执行并 gtest 风格打印。通道（USB 直连 / 经 server）作为 fixture 注入；多设备遍历 serial。
  交互阶段用 stdin 提示 + 超时（tokio timeout 120s）。

## Decision (ADR-lite)

- **Context**: server 前端下沉后退化 + 缺乏真实设备回归手段。
- **Decision**: 库侧 P1-P4 全做（reverse 诚实分阶段）；selftest 作为 `adboost_cli selftest` 子命令，进程内起
  frontend + 库内 ProxyDevice 验证经 server 通道，临时端口避开 :5037；tcpip 预埋 SKIPPED；官方 adb parity 自动探测可选。
- **Consequences**: 不干扰用户现有 adb 环境；reverse 可能分阶段；parity/tcpip 在缺环境时优雅降级。

## Out of Scope

- SYNC v2 + 压缩（brotli/lz4/zstd）—— 复用现有 SyncSession 仅 v1。
- ssh（非 adb 标准，xpeng 定制，永不下沉）。
- tcpip 实测（仅预埋 SKIPPED，待 android emulator 环境）。
- 接管 :5037 / kill 外部 adb daemon（selftest 用临时端口）。
- forward 端到端人工验证（改为程序内自动化验证）。

## Technical Notes

- FR 原文：`/private/tmp/adboost-followup-capabilities.md`
- 关键文件：`adb_client/src/server/{frontend,backend,capabilities,usb_backend,protocol}.rs`，
  `adb_client/src/message_devices/usb/{persistent,sync_session,shell_v2_session}.rs`，
  `adb_client/src/proxy/{adb_proxy_device,device_commands/forward,device_commands/reverse}.rs`，
  `adboost_cli/src/{main,daemon}.rs`、`adboost_cli/src/models/`、`adboost_cli/src/handlers/`。
- forward wire 格式：`ADBLocalCommand::Forward(remote, local)` → `host:forward:{local};{remote}`；
  ProxyDevice 先 `set_serial_transport()` 再发——实现 frontend 时需对照 host-serial 变体。

## Implementation Plan (small PRs)

- **PR1 — backend trait 扩展 + 能力协商骨架**：`DeviceBackend` 新增 `open_sync_session`/`open_shell_v2`
  （带默认 unsupported 实现）；`UsbDeviceBackend` 覆盖；capabilities 随 backend 协商；单测。
- **PR2 — P1 forward**（纯 frontend）：放行 + 注册表 + listener 桥接 + 双 OKAY + port-0；pure helper 单测。
- **PR3 — P2 sync + P3 shell-v2 桥接**：frontend 放行 + 桥接；诚实通告 sync_v2/shell_v2；单测。
- **PR4 — P4 reverse**：frontend 主机侧编排（incoming_opens + listener）；端到端或诚实分阶段降级。
- **PR5 — selftest harness（自动化阶段）**：`adboost_cli selftest` 子命令 + TestCase/runner + gtest 输出 +
  USB 直连/经 server 两通道 shell/pull/push/forward + 多设备 + tcpip SKIPPED + 官方 parity 探测。
- **PR6 — selftest 交互阶段 + 文档**：USB 拔插重连、reboot 恢复（120s 超时）；README/能力矩阵更新。
</content>
