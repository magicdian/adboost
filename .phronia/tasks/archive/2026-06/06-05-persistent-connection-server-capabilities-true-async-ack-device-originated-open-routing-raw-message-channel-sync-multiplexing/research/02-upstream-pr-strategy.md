# 上游 PR 调研与合并策略（#184 / #198）

> 来源：调研 workflow `wz95n7zg8` 的 2 个 PR 分析 agent + `00-fork-upstream-topology.md` 的 git 考古。
> 物料：`research/_material/pr-184.diff`、`pr-198.diff`、`pr-184-adb_multiplexer.rs`、`pr-184-adb_session.rs`。

## 统御性事实

fork 是上游 v3.2.2@365ef22 的 **squash 快照**，与上游 git history 无关 → "合入上游" = **手工 patch，永远不是 git merge**。
对每个改动只问一句：**它碰的代码上游也有吗？** 6 个 Ask 全在 `persistent.rs`（上游无此文件、且不实现任何共享 trait）→ **6 个 Ask 全部零上游合并成本**。唯一摩擦面是共享 `ADBDeviceExt` trait 及其 impl。

---

## PR #184 — "wip: multiplexed usb/tcp devices"（draft，基于更老的 v3.1.1）→ **IGNORE（0% 采纳）**

上游自己的平行多路复用尝试，与 fork 的 `persistent.rs` **设计竞争**，但基线更老、且带 3 个 CRITICAL bug。

**3 个 CRITICAL bug（我们要避开的坑）**：
1. **TLS/STLS 路由 bug**（diff `:149-162`）：STLS 升级后 `connect()` 返回 `Ok(())` 却没调 `set_authenticated()` → reader 把消息路由到 `unauthenticated_data` 队列而非 session 队列 → 首个 `open_session()` 永久阻塞等错队列的 OKAY。
2. **`Arc::into_inner()` 静默失败**（`adb_multiplexer.rs:160-164`）：`disconnect()` 用 `Arc::into_inner(handle)`，只要有任何 session clone 存活就返回 `None` → reader 线程 `join()` 永不执行 → **线程永久泄漏**。
3. **无 reader 关闭机制**（`adb_multiplexer.rs:114-140`）：reader 跑 `loop{read_message()?}` 无 atomic shutdown flag → 断连时无法优雅退出。（fork 的 `persistent.rs` 有 `shutdown: AtomicBool`，更优。）
- 附加：50ms sleep 轮询（`:89`）而非阻塞 channel，CPU 浪费、多 session 扩展性差；捆绑无关依赖升级（chrono/rcgen/regex/rustls）膨胀审计面。

**可借鉴的设计教训（只参考，不 port 代码）**：
- 多路复用作为 **middleware**（包在 transport 外）而非 fork 的 `PersistentUsbConnection` 单例——更干净，但要大重构。
- `ADBSession<T>` 做成 **Clone** 以支持并发读写——fork 没有 split() 之外的等价物（fork 用 `into_split()` 解决）。
- 从 `adb_message_device.rs` **抽出**多路复用逻辑，让 device 专注 ADB 协议状态（CNXN/AUTH/STLS）。
- 显式 `RustADBError::Timeout` 错误类型，而非字符串匹配——更干净的 API（fork 已用结构化 `UsbTimeout`，方向一致）。

**结论**：fork 已有 `persistent.rs`（实战验证）。采纳 #184 会造成**两个多路复用器互相破坏 session map**。0% 采纳，仅作架构第二意见。

---

## PR #198 — "wip: v4.0.0 — improve ADBDeviceExt trait" → **ADAPT（在 holding branch 上预先对齐 trait 方向）**

这是**真正的未来合并摩擦面**。把 `ADBDeviceExt` trait 改为 dyn-兼容/泛型（`boxed()`、`Box<dyn ADBDeviceExt>`），触及 fork 也持有的 18 个共享文件、约 60 个方法签名。

**碰撞面**：
- trait 定义 `adb_device_ext.rs:14-142`：`&dyn AsRef<T>` 参数 → 泛型。
- 4 个 device impl（`ADBMessageDevice<T>`/`ADBServerDevice`/`ADBTcpDevice`/`ADBUSBDevice`）+ 8 个 command 文件 ~60 签名必须重构。
- CLI `main.rs:151,203,213` 调 `.boxed()`，PR 移除它。
- path 命令（push/pull/list/stat/install/uninstall）从 `AsRef<str>` → `AsRef<Path>`，~30-50 调用点需包 `Path::new()`。
- Python 绑定 `pyadb_client/*` 更新签名转换。
- **`persistent.rs` 本身零冲突**（它不 impl `ADBDeviceExt`）。

**间接陷阱（关键）**：一旦 fork 后续加 `PersistentUsbDevice impl ADBDeviceExt`（把持久连接通过标准 device API 暴露），它**必须**匹配 v3.2.2 或 v4 签名二选一。若用今天的 v3.2.2 trait 实现，v4 后续 ship 时要吃 ~15 小时 / 18 文件冲突 **外加**重写自己的新 impl。

**采纳清单（ADOPT）**：
- 泛型参数核心思想：`&dyn AsRef<T>` → `<P: AsRef<Path>>`（类型安全 + 单态化）。
- path 处理标准化（`.to_string_lossy()` 防御性 UTF-8）。
- 所有权灵活性：`&mut dyn Read` → `R: Read`（调用方决定 boxing）。
- **线程 scope 安全（CRITICAL，立即采纳）**：`spawn()` → `scope()` 防数据竞争、强制借用生命周期——**修一个潜在 bug**。

**拒绝清单（REJECT）**：
- 167 行 `ADBDevice` enum wrapper（`adb_cli/models/adb_device.rs`）——CLI 保留 `.boxed()` 更简单。

**结论**：现在在 holding branch 上 cherry-pick PR #198 的**方向**（泛型 + scope 修复）。把"保证会发生的未来合并意外"转成**前置、可控**的工作，并让任何新 `PersistentUsbDevice` impl 一出生就 v4 兼容。

---

## 对本任务的净指导

| 改动 | 碰上游共享代码? | 合并摩擦 |
|------|----------------|---------|
| Ask #1-#6（全在 `persistent.rs`）| 否（上游无此文件）| **零** |
| #3 裸用 `write_message` | 已是 pub trait 方法，有先例 | 零 |
| **未来** `PersistentUsbDevice: ADBDeviceExt` impl | **是——这是陷阱** | 若不预先采纳 PR #198 则 high |

→ **优先在 `persistent.rs` 实现能力，尽量少碰共享 trait/commands**；把唯一上游敏感工作（`ADBDeviceExt` impl）推到最后，置于 PR #198 预采纳之后。
