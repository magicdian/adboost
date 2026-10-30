# Fork ↔ Upstream 拓扑与合并成本基线

> 调研时间：2026-06-05。基于 `git fetch` 上游 `cocool97/adb_client` main + PR #184/#198 head。
> 目的：在动 `persistent.rs` 大手术前，先把"未来还能不能合入上游"这个担忧量化。

## 决定性事实

### 1. fork = 上游 v3.2.2 的 squash 快照，git 层面与上游 histories unrelated

- fork root commit：`42a2c24 initialize project from adb_client v3.2.2@365ef22`（2026-06-02）。
- 上游 root commit：`7670c1f Initial commit`（2022-01-05）。
- `git merge-base HEAD refs/upstream/main` → **空**（无共同祖先）。
- 上游 main HEAD = `365ef22 chore: v3.2.2` —— 与 fork 的初始化基线**完全一致**。

**推论**：fork 的内容基线 = 上游 main @ 365ef22，但因为是 squash init，git 不认共同祖先。
所以"合入上游新提交"**从来就不是 `git merge`，而是手工 cherry-pick / patch apply**。这一点
独立于本次改动——无论我们改不改 `persistent.rs`，上游同步都得手工做。

### 2. fork 在基线上只加了 2 个小鹏 patch

```
8b24f89 import xdb usb extensions patch into fork   ← 引入 persistent.rs (780行) + session_stream.rs (153行)
1af81a5 refactor(usb): migrate USB transport from rusb to nusb  ← usb_transport.rs 重写
```
（其余 commit 是 trellis/spec/journal 杂务。）

### 3. `persistent.rs` 是小鹏独有文件，上游 main 不存在

- `git cat-file -e refs/upstream/main:adb_client/src/message_devices/usb/persistent.rs` → **不存在**。
- 它由 `8b24f89` 引入，是 xdb usb extensions patch 的产物。
- **6 个 Ask 的主战场全部在 `persistent.rs`（826 行）**。

**关键推论（反转了用户的担忧）**：
> 在 `persistent.rs` 上做任意大手术，**对未来合入上游的成本影响 = 0**——因为上游永远不碰这个文件，
> 不会产生冲突。真正的合并摩擦点在 **共享文件**（trait 定义、`commands/*`、`adb_session.rs`、
> `adb_message_device.rs`），改动这些才会与上游未来提交冲突。

## 上游两个 PR 的拓扑

| PR | 标题 | 状态 | 基线 | 触及 `persistent.rs`? | 触及共享文件? |
|----|------|------|------|----------------------|--------------|
| #184 | wip: multiplexed usb/tcp devices | draft (open) | v3.1.1 `2e3db33`（更老） | 否（其基线无此文件）| 是：新增 `adb_multiplexer.rs`，改 `adb_session.rs`/`adb_message_device.rs`/多个 `commands/*` |
| #198 | wip: v4.0.0 — improve ADBDeviceExt trait | open | `5180775`（v3.2.2 之后） | 否 | **是，重度**：`adb_device_ext.rs`/`adb_message_device_commands.rs`/`commands/{install,list,pull,push,shell,stat,uninstall}.rs`/`server_device/*`/tcp+usb device/pyadb |

- **PR #184** 是上游自己的平行多路复用尝试，与 fork 的 `persistent.rs` **设计竞争**。
  reviewer (salvatorebenedetto) 标注阻塞问题：TLS/STLS 升级后未调用 `set_authenticated()`
  → 后续消息路由到错误队列；Drop 可能挂死；PR 把 timeout + deps + multiplexer 混在一起。
  → 它新增 `adb_multiplexer.rs`（168 行）作为竞争设计，可作**参考**，但 fork 已有 `persistent.rs`。
- **PR #198** 是真正的**未来合并摩擦点**：它重塑 `ADBDeviceExt` trait（dyn-兼容、`boxed()`），
  大改 fork 也持有的共享文件。本次 server 工作若大改这些共享文件的签名，会与 v4.0.0 正面冲突。

## 对本任务的指导

1. **优先在 `persistent.rs`（独有文件）里实现能力**，少碰共享 trait/commands → 上游合并成本最低。
2. 若必须改共享文件，**先评估 PR #198 的新 trait 形状**，决定"预先对齐 v4.0.0 trait"还是"留在 v3.2.2"。
3. PR #184 的 `adb_multiplexer.rs` 作为多路复用设计的**第二意见**参考（尤其它的 bug 是我们要避开的坑）。

## 物料

- `research/_material/pr-184.diff`、`pr-198.diff`：两个 PR 相对各自 merge-base 的完整 diff。
- `research/_material/pr-184-adb_multiplexer.rs`、`pr-184-adb_session.rs`：竞争多路复用设计源码。
- 本地 refs：`refs/upstream/main`、`refs/upstream/pr-184`、`refs/upstream/pr-198`（已 fetch）。
