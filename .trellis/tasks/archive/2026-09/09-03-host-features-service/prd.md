# PRD: adb-server frontend — `host:host-features`（server 级）+ 裸 `host:features` 语义对齐

- **Source**: xdb feature request §5（二期发现，2026-09-03 晚，针对 rev `2371022` 实测）
- **Severity**: High —— `track-devices-l` 落地后 AS 仍看不到设备；**上一任务上线的 WARN 日志漏斗**
  抓到根因：AS 每 ~1s 无限重试 `host:host-features` → `FAIL unknown host service` →
  adblib 从未走到 track-devices-l。
- **Scope**: `adboost` crate server frontend（`frontend.rs`）+ `adboost_cli` selftest。

---

## 1. Background

adblib 设备跟踪的决策链（AS 2026.1.4，反编译确认）：

```
SessionDeviceTracker.pickBestFormat()
  → AdbHostServices.hostFeatures() → wire `host:host-features`   ← 本任务缺口
  → 含 devicetracker_proto_format ? proto-binary : track-devices-l  ← 已实现（46df633）
```

AOSP 语义（`adb.cpp::handle_host_request`）——两个 features 查询语义不同：

| wire 服务 | CLI | 语义 | AOSP 行为 |
|---|---|---|---|
| `host:host-features` | `adb host-features` | **server 级** | `FeatureSetToString(supported_features())`（real adb 额外附 `libusb`、`push_sync`） |
| `host:features`（裸，pre-transport） | `adb features` | **per-transport** | `acquire_one_transport` 后返回 `t->features()`；0/多设备 → AOSP FAIL 措辞 |

adboost 现状：裸 `host:features` 实现为 **server 级**（语义错位，恰是 adblib 不用的形态），
缺 adblib 实际使用的 `host-features`。与 track-devices-l 同类：缺 arm → FAIL → adblib 无降级。

## 2. Requirements

### R1 — `host:host-features`（P0，验收关键）

新增该 host 数据查询：回复 `OKAY` + `%04x`framed + server feature 集
（`caps.features_csv()`，serve 时已与 backend 能力协商过的诚实集合）。

- **诚实原则**：不得包含未实现的 `devicetracker_proto_format`（P1 proto 变体上线前）；
  也不伪造 real adb 附加的 `libusb` / `push_sync`（adboost 不经 libusb、push 走 v2 通道）。
- 0 设备时也必须 OKAY（server 级查询，不解析 transport）。
- 放入 `host_data_query_payload`（一次性 `OKAY`+framed 数据查询的单一汇聚点）。
- **post-transport 一致性**：transport 选定后客户端再发 `host:host-features` 也应作答
  （与 `host:version` 同类的 server 级查询，镜像 `handle_client_impl` 的 post-transport 路由）。

### R2 — 裸 `host:features` 对齐 AOSP per-transport 语义（报告的可选校正，采纳）

采纳报告首选方案：裸 pre-transport `host:features` 改为 transport-any 单设备解析——

| 场景 | 现行为 | 新行为（= real adb） |
|---|---|---|
| 单设备 | OKAY + **server** features | OKAY + **该设备** features（`device_features_csv`，与 pinned `host-serial:<s>:features` 字节一致） |
| 0 设备 | OKAY + server features | `FAIL no devices/emulators found` |
| 多设备 | OKAY + server features | `FAIL more than one device/emulator` |

实现即 `get-state`/`get-serialno` 裸形态的既有模式（`resolve_single_serial()` →
`device_features_csv(serial)`）。**不变**：post-transport `host:features`（已 per-device）、
`host-serial:<s>:features`、`host-usb:/host-local:` phase-1 features。

### R3 — Non-goals

- 不实现 `devicetracker_proto_format` / proto 变体（仍为 P1）。
- 不添加 `libusb` / `push_sync`（诚实能力原则；spec 记录差异）。
- 不改 `host:version`。

## 3. Tests

### Unit（`frontend.rs`，`--features server,usb`）

- `host_features_replies_server_feature_set`：`host:host-features` 0 设备也 OKAY + 诚实 csv
  （改造原 `host_features_is_honest_minimal`）。
- 裸 `host:features`：单设备 → per-device csv；0 设备 → AOSP FAIL；多设备 → AOSP FAIL；
  裸 vs `host-serial:<s>:features` 字节一致（镜像 bare_get_*_matches_pinned_* 系列）。

### Real-device（`adboost_cli selftest`，parity.rs 沿用官方 CLI 驱动）

- `case_official_adb_host_features`：`adb -P <port> host-features` → 成功且含 feature token，
  `unknown host service` 判 REGRESSION。每 run 一次（server 级、非破坏性）。
- `case_official_adb_features`：`adb -P <port> features`（裸，无 `-s`）→ 单设备场景含
  `cmd`（per-device 交集后仍存活的 always-safe token）；多设备场景打印 AOSP 歧义错误。

### 协议级现场验收（无需 AS）

in-process/独立端口 server 上探测：`host:host-features` → OKAY + csv（无
`devicetracker_proto_format`）→ 证明 adblib 决策链首环接通（下一环 track-devices-l 已有）。

## 4. Acceptance criteria

1. `host:host-features` 回 OKAY + 诚实 server features（0 设备亦然）；日志不再出现
   `host:host-features` 的 unknown WARN。
2. 裸 `host:features` 行为与 real adb 一致（per-transport + AOSP FAIL 措辞 + 与 pinned 形式字节一致）。
3. `fmt` / `clippy`（默认 + `server,usb`）/ 全量测试绿；真机 selftest 新 parity 用例通过。
4. spec（`server-host-protocol.md`）两轴 features 章节更新：`host-features` 行、裸 features
   语义修正、adblib 决策链 gotcha、libusb/push_sync 差异记录。

---

## R3（验收驱动追加）：AS 安装/调试服务 —— `exec:` + JDWP 家族

用户以「adboost 拿 USB + 监听 5037，AS 能看到设备、能安装应用、能调试」为本轮验收标准。
真机验收中 WARN 漏斗抓到第三批缺口（均为 adbd 侧服务，AOSP `daemon/services.cpp` 实锤）：

| 缺口 | 日志证据 | 影响 |
|---|---|---|
| `exec:` | ×15（`exec:cmd package install-write … -` + `exec:/data/local/tmp/.studio/bin/installer …`） | **AS 安装失败**（用户报错实锤） |
| `track-jdwp` | ×534（AS 每秒重试） | AS 调试器进程监控 |
| `jdwp`（裸，`adb jdwp` CLI 用） | 探针发现 | CLI parity / 单次列表 |

实现：`map_local_service` 新增两族 arm——`exec:` verbatim 透传（两轴 `shell_v2` 门控，与
`shell,v2` 同判）；`track-jdwp`/`track-app`/`jdwp`/`jdwp:<pid>` verbatim 透传（adbd 核心
服务，无门控）。`serve_local_service` 的 caps 预查询扩展到 `exec:`。测试：3 个 exec 门控
单测 + jdwp 家族单测 + `case_official_adb_exec_out` / `case_official_adb_jdwp` parity 用例。
真机验证：`adb exec-out` / `adb jdwp` / `adb devices` 全部通过 adboost-5037 正常工作。

验收状态：设备识别 ✅（AS 认到 d03）；安装/调试 —— 待用户在 AS 中重试（服务已补齐）。
