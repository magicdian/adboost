# Fix host-serial parsing for TCP/IP serials containing a colon (ip:port)

## Goal

adboost server frontend 解析 `host-serial:<serial>:<sub>` 请求时，用 `split_once(':')` 从**第一个**
冒号切分。USB serial 不含冒号没问题，但 TCP/IP 设备的 serial 是 `ip:port`（如 `172.20.1.45:5555`，
本身含冒号），导致 serial 被切成 `172.20.1.45`、sub 被切成 `5555:features`，落入未知分支并返回
`unknown host-serial sub-service: 5555:features`。修复解析逻辑，让含冒号的 TCP/IP serial 能被正确识别。

## Bug 复现与证据

- **现象**：`adboost_cli` selftest 用例 `tcpip.shell_through_tcp_device` 失败：
  `adb: unknown host-serial sub-service: 5555:features`。
- **触发链**：用例执行 `adb -P <port> -s 172.20.1.45:5555 shell echo <marker>`；官方 adb 客户端在
  `-s <serial>` 下先发 `host-serial:172.20.1.45:5555:features` 探测 feature → 命中解析 bug。
- **根因位置**：`adboost/src/server/frontend.rs:206`
  ```rust
  if let Some((serial, sub)) = rest.split_once(':') {  // 从第一个冒号切，对 ip:port serial 错误
  ```
  `rest = "172.20.1.45:5555:features"` → `split_once(':')` → serial=`172.20.1.45`, sub=`5555:features`
  → `dispatch_host_serial` 的 `match sub` 落到 `other =>`（frontend.rs:369-375）→ FAIL。
- **与 expose-tcp 任务无关**：该 selftest 用例由 commit `c6447d7` 引入（早于本次工作）；解析 bug
  也一直存在。`expose-tcp-persistent-connection-building-blocks` 任务未触碰 `frontend.rs`
  （`git diff HEAD -- frontend.rs` 为空），只是让 TCP 路径可用从而暴露了它。

## 难点：sub-service 自身可能含冒号

不能简单改成 `rsplit_once(':')`，因为部分 sub-service 本身含冒号：
- `forward:tcp:0;tcp:7777` / `killforward:tcp:7777`（frontend.rs:352 `forward:`/`killforward:` 前缀分支）
- `transport` / `tport`（frontend.rs:363，不含冒号，但属于已知 sub-service）

已知 sub-service 集合（见 `dispatch_host_serial` frontend.rs:327-376）：
`get-state` / `get-serialno` / `features` / `list-forward` / `killforward-all` /
`forward:*` / `killforward:*` / `transport*` / `tport`。

因此正确切分点 = "serial 与已知 sub-service 之间的那个冒号"，需按已知 sub-service 模式来定位，
而非盲目取第一个或最后一个冒号。

## Requirements

- [R1] `host-serial:<serial>:<sub>` 解析在 serial 含冒号（`ip:port`）时正确切出完整 serial 和完整 sub。
- [R2] 保持对所有既有 sub-service 的兼容：`get-state`/`get-serialno`/`features`/`list-forward`/
       `killforward-all`/`forward:...`/`killforward:...`/`transport*`/`tport`，含 `forward:` 这种
       自身带冒号的 sub。
- [R3] 不回归 USB serial（不含冒号）路径。
- [R4] 与 AOSP `host-serial:` 解析语义对齐（参照 server-host-protocol spec）。

## Open Questions

- [Q1] 切分策略：(A) 按已知 sub-service 后缀集合从右匹配定位切分点；(B) 已知 sub-service 改为前缀/精确
       匹配后，对 serial 做"剩余即 serial"提取；(C) 其它。倾向 (A)/(B) 类——以"已知 sub-service"为锚，
       而非冒号位置。具体由 implement 阶段结合 spec 与 AOSP 行为敲定。

## Acceptance Criteria

- [ ] `host-serial:172.20.1.45:5555:features` 正确路由到 features 分支并回 OKAY+payload。
- [ ] `host-serial:172.20.1.45:5555:get-state` / `:transport` 正确路由。
- [ ] `host-serial:172.20.1.45:5555:forward:tcp:0;tcp:7777` 正确路由（serial 与 forward 子参数都不被截断）。
- [ ] USB serial 路径（如 `host-serial:dev1:get-state`）无回归。
- [ ] 新增单元测试覆盖含冒号 serial 的各 sub-service（frontend.rs 现有 round_trip 测试风格，见 :1335 起）。
- [ ] selftest `tcpip.shell_through_tcp_device` 通过（需真机，作为手动验收项）。
- [ ] `cargo clippy --all-targets -- -D warnings` + `cargo test` 全绿。

## Definition of Done

- 解析修复 + 回归单测（含冒号 serial × 各 sub-service + USB serial 不回归）。
- Lint / typecheck / test 绿。
- 若有协议解析约定值得沉淀 → 更新 server-host-protocol spec。

## Out of Scope

- expose-tcp-persistent-connection 任务的任何改动（独立任务，独立 commit）。
- 重构整个 host service dispatch（只修 host-serial serial/sub 切分这一处）。

## Technical Notes

- 关键文件：
  - `adboost/src/server/frontend.rs:205-213`（`host-serial:` 前缀剥离 + 切分 bug 点）
  - `adboost/src/server/frontend.rs:318-378`（`dispatch_host_serial` 的 sub-service match）
  - `adboost/src/server/frontend.rs:1335+`（现有 host-serial round_trip 单测，新增测试参照此）
  - `adboost_cli/src/selftest/parity.rs:180`（失败用例 `case_official_adb_shell_through_tcp_device`）
- spec：`.trellis/spec/backend/server-host-protocol.md`（host 协议 / transport 选择 parity，含 AOSP 错误措辞）。
- 注意 `forward:`/`killforward:` sub 自身含冒号；`transport`/`tport` 是 transport 选择路径
  （返回 `HostOutcome::TransportSelected`，不能误判）。
