# 综合：依赖图、上游策略、MVP 切分

> 来源：调研 workflow `wz95n7zg8` 的 lead-architect 综合 agent。决策级，供 brainstorm 收敛 prd。

## 执行摘要

- **本库宣告了一个 server 级别的能力集，却没实现。** CNXN banner（`persistent.rs:127`）宣告 18 个 feature（含 `delayed_ack`、4 个 `sendrecv_v2_*`、`shell_v2`），而代码零压缩依赖、stop-and-wait 写路径、无 shell-v2 解码。今天（client 角色）device 容忍；一旦 xdb 当 **server**，每条假宣告都变成连接它的客户端会据以行事的契约。
- **两个严重度层级**：唯一 **critical** 是 Ask #2（device-originated OPEN 被丢 → reverse/scrcpy 静默失效）；其余 **high** 是吞吐/契约债（#1、#6）或缺 API（#3、#4、#5），非当前 client 流的活跃损坏。
- **blast radius 压倒性地是 XPENG-only**：`persistent.rs` 上游不存在、不实现共享 trait → 6 个 Ask 全部零上游合并成本。摩擦面只在 PR #198 重塑的共享 `ADBDeviceExt` trait。
- **单 bulk-IN reader 是架构拱心石**：Ask #2/#3/#4/#5 全部经由唯一的 `reader_loop`（独占 IN 端点，`persistent.rs:84-88` + `usb_transport.rs:310` 持锁）。无法存在第二个 reader → 每个路由改动都是改**这一个 loop**，既是天然集成点，也是串行化约束。
- **上游裁决**：PR #184 **ignore**（竞争性、3 个 critical bug、更老基线）；PR #198 **pre-adopt 方向**（在 holding branch cherry-pick 泛型 + scope 修复，避免保证发生的未来冲突）。

## 依赖图

```
                 ┌─────────────────────────────────────────┐
                 │  #6 诚实 banner（地基 / 闸门）              │
                 │  FeatureSet 决定我们能宣告什么              │
                 └───────────────┬───────────────────────────┘
        宣告 delayed_ack? shell_v2? sendrecv_v2? │
              ┌──────────────────┼────────────────────────┐
              ▼                  ▼                         ▼
   ┌────────────────┐   ┌─────────────────┐     ┌──────────────────┐
   │ #1 流控          │   │ #5 shell-v2     │     │ 压缩 = 以后/永不   │
   │ 写/读半语义       │   │ 帧解码           │     └──────────────────┘
   └───────┬─────────┘   └─────────────────┘
           │ #4 SYNC 骑在写/读半语义上
           ▼
   ┌────────────────┐
   │ #4 SYNC 多路复用 │  (open_sync_session 复用同一 channels)
   └────────────────┘

   ┌──────────────────────────────────────────────────────────┐
   │  reader_loop（单 bulk-IN 拥有者）—— 共享编辑面             │
   │  #2 device-OPEN 路由（arg1==0）                            │
   │  #3 裸 subscribe tee                                       │
   │  #4 SYNC 帧（已按 arg1 路由，无新增编辑）                   │
   └──────────────────────────────────────────────────────────┘
```

**关键路径**：`#6（闸门）` → `#2（critical, reader_loop）` ‖ `#1（核心写/读语义）` → 然后 `#4`、`#5`、`#3`（#3 折进 #2 的 reader_loop 改动）。

**跨 Ask 耦合**：
- #1 重写写半（`:615-666`/`:744-802`）+ 读半（`:547-612`/`:673-741`）。**#4(SYNC) 直接骑在这些写/读半语义上**——#1 的 API 形状（阻塞 vs 窗口）必须在 #4 之上构建前定下，否则 #4 重写两次。
- #2 + #3 都改 reader_loop 同一 demux switch → 作为**一次 reader_loop 重设计**，避免两次触碰、避免触发反复警告的第二-reader deadlock。

## 上游策略（详见 `02-upstream-pr-strategy.md`）

- 6 个 Ask 全在 `persistent.rs` → **零上游合并成本**。
- PR #184 → **ignore**（仅借鉴架构教训：multiplexer-as-middleware、ADBSession-Clone、显式 Timeout 错误）。
- PR #198 → **adapt**：现在在 holding branch 预采纳泛型方向 + **立即采纳 `spawn()`→`scope()` 修复**（修潜在数据竞争）；拒绝 167 行 enum wrapper。任何未来 `PersistentUsbDevice: ADBDeviceExt` impl 一出生就 v4 兼容。

## 推荐 MVP 切分

### MVP — "不撒谎、不静默丢包的诚实 server 地基"
1. **#6（子集）诚实 banner**：banner 收紧到只-已实现。摘掉 4 个 `sendrecv_v2_*`；`delayed_ack` 仅当 MVP 同时做 #1-Option-B 才保留，否则摘掉；`shell_v2` 仅当 #5 同期落地才保留。**一改同时中和 #1/#4/#5/#6 追溯到 line 127 的风险。最低成本、最高杠杆。**
2. **#2 critical 修复 + #3 裸 subscribe 作为一次 reader_loop 重设计**：`arg1==0`→`pending_opens` 路由 + `subscribe_raw` tee 一次编辑搞定（`persistent.rs:238-264`），bounded 队列 + 定义溢出策略。解锁 reverse/scrcpy + 提供中继原语，且绝不冒第二-reader 险。
3. **#4（v1 only）`open_sync_session()` 薄封装**：薄包 `open_session(&ADBLocalCommand::Sync)`，SYNC 帧走现有 demux。视 brainstorm Q4 决定是否退役 xdb 侧 fresh-`ADBUSBDevice` workaround。无压缩。

### Phase 2 — "真吞吐 + 完整 shell"
4. **#1（Option B）窗口化 delayed_ack 流控**：per-session `FlowControl`、解析 OKAY arg0、pipeline WRTE。落地后再把 `delayed_ack` 加回 banner。若 MVP 把 #4 建在 stop-and-wait 上，这里预算一次 #4 touch-up。
5. **#5 `ShellV2Session` 解码器**：port `adb_server_device_commands.rs:189-205` 的 5 字节帧解析；捕获 stderr + exit code。落地后把 `shell_v2` 加回 banner。低碰撞（无现存 `shell_exec` 调用方）。

### Phase 3 — "除非必要否则推迟"
6. **SYNC v2 + 压缩**（brotli/lz4/zstd 依赖 + codec 枚举）。仅当 server 用例确实需要压缩传输。
7. **`PersistentUsbDevice: ADBDeviceExt` impl** —— 在 PR #198 trait 方向预采纳之后再做，一出生即 v4 兼容、零 retrofit。

### 一句话 MVP 论点
先 ship **真相（诚实 banner）+ 正确性（#2 device OPEN）+ 触达（裸中继 #3、SYNC v1 #4）**；把 **性能（#1 窗口化）和丰富度（#5 shell-v2、压缩）**推到 Phase 2/3。每个 MVP 项都是 XPENG-only 零上游摩擦；唯一上游敏感工作（`ADBDeviceExt` impl）正确地推到 Phase 3、置于 PR #198 预采纳之后。

## 待 brainstorm 与用户敲定的设计岔路口

1. **delayed_ack：摘掉宣告 还是 真做窗口？（Ask #1，最大岔路）** A=诚实-最小（摘 banner，保留 stop-and-wait，~6.5MB/s@10msRTT，零 deadlock 险，便宜）；B=真 server（per-session 窗口，必需以兼容 Android 11+ adbd 激进 pipeline，改 public write API）。子问题：B 时**保留阻塞 Write + 新增窗口 API** 还是**替换**阻塞语义（sender-task 模型让 read 调用并发写，但是更大的 `MultiplexedSession` 重构）。
2. **裸中继 vs 煮熟-session 中继（Ask #3）**：send_raw / RelaySession / subscribe_raw(filter)。Option 3 是唯一不违反单-reader 约束的，但耦合 #2 的 reader_loop 重设计。决定：做**透传中继（scrcpy 式）还是煮熟多路复用 API**？这决定 #2 与 #3 是一次改动还是两次。
3. **device-originated 连接：队列-轮询 vs 回调（Ask #2）**：`accept_device_connection(timeout)`(pull) vs 注册回调(push)，哪个更贴 scrcpy reverse 的事件循环？bounded `pending_opens` 的溢出策略？
4. **SYNC v2 现在做 还是 v1-only？（Ask #4）**：建议 v1-only + 摘 4 个 `sendrecv_v2_*`。子问题：接口排他 workaround——SYNC 走持久 reader_loop 退役 fresh-device，还是保留 workaround？
5. **诚实 banner：静态-诚实 vs 可配置 FeatureSet（Ask #6）**：要不要 per-connection feature 协商（server 可能对不同 client 给不同 banner），还是一条诚实静态 banner 够用？
6. **shell-v2 分层（Ask #5）**：`ShellV2Session` wrapper（保持 `MultiplexedSession` 字节透明，更安全）vs 把解码塞进 `MultiplexedSession::read()`。建议 wrapper。
