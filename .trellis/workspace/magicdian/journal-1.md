# Journal - magicdian (Part 1)

> AI development session journal
> Started: 2026-06-02

---



## Session 1: Import xdb USB extensions patch into fork

**Date**: 2026-06-02
**Task**: Import xdb USB extensions patch into fork
**Branch**: `main`

### Summary

Imported the functional hunks of 0001-xdb-usb-extensions.patch (authored against adb_client v3.2.1) into the xp_adb_client fork. Skipped the Cargo.toml hunk to preserve workspace inheritance and version 3.2.2. Added new files session_stream.rs (ADBSessionStream) and usb/persistent.rs (PersistentUsbConnection + MultiplexedSession session multiplexing), made open_session pub, added ADBUSBDevice::inner_mut and ADBLocalCommand::TcpConnect (hand-ported for v3.2.1->v3.2.2 enum drift). Quality gate GREEN: build/test/clippy pass for adb_client --features usb, dependents unaffected. Captured the upstream-patch-import procedure as a new backend spec.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `8b24f89` | (see git log) |
| `0af5888` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 2: Bootstrap backend coding guidelines

**Date**: 2026-06-02
**Task**: Bootstrap backend coding guidelines
**Branch**: `main`

### Summary

Filled the five placeholder backend spec files from real adb_client/adb_cli/pyadb_client source (verified file:line citations): directory-structure (workspace + mod.rs convention + models/commands/*_commands split + ADBDeviceExt layering), error-handling (RustADBError thiserror + Result alias, CLI ADBCliError classification, PyO3 anyhow mapping, panic policy, persistent.rs lock().unwrap() flagged as tech debt), logging-guidelines (log facade, log::<level>! style, level conventions), database-guidelines (repurposed as Persistence & External State since there is no DB), quality-guidelines (clippy pedantic, MSRV 1.88, feature flags + CI feature gap, inline test style, quality gate). Updated index with pre-dev checklist. Completed and archived 00-bootstrap-guidelines.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `b074d8e` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 3: Migrate USB transport from rusb to nusb

**Date**: 2026-06-03
**Task**: Migrate USB transport from rusb to nusb
**Branch**: `main`

### Summary

Migrated adb_client USB layer from rusb (libusb/vendored-C) to pure-Rust nusb. Thin adapter keeps USBTransport public API stable; new_from_device now takes nusb::DeviceInfo. Per-call timeout via Endpoint::transfer_blocking; TransferError::Cancelled mapped to new RustADBError::UsbTimeout and matched structurally in the persistent reader loop (replacing fragile error-string matching). IN/OUT endpoints behind two separate Arc<Mutex> locks (USBTransport stays Clone per ADBMessageTransport bound) so reader never blocks writer. Preserved header/payload accumulate loop, zero-length-packet write, CRC32 integrity. Verified: adb_client/adb_cli build+clippy+test green; pyadb_client green under Python 3.10/3.12 (3.9 failure was a pre-existing abi3-py310 env issue). 4 new unit tests. Remaining: Windows WinUSB on-device manual test (USB I/O not CI-testable).

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `1af81a5` | (see git log) |
| `3336689` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 4: persistent.rs server capabilities: 6 Asks (delayed_ack, device-OPEN, raw channel, SYNC mux, shell-v2, honest banner)

**Date**: 2026-06-05
**Task**: persistent.rs server capabilities: 6 Asks (delayed_ack, device-OPEN, raw channel, SYNC mux, shell-v2, honest banner)
**Branch**: `feat/persistent-server-capabilities`

### Summary

Two-round read-only research (16 agents) verified all 6 capability Asks against source + AOSP wire protocol, mapped fork-vs-upstream topology (persistent.rs is XPENG-only = zero merge cost) and async strategy (sync core now, sans-io async later). Implemented all 6 Asks in persistent.rs via 5 trellis-implement/trellis-check rounds: honest DeviceFeatureSet banner, device-OPEN routing + raw subscribe_raw/send_raw (one reader_loop redesign), delayed_ack windowed FlowControl (32MiB, OKAY-payload i32-LE delta, opener starts at 0), SYNC v1 open_sync_session, shell-v2 exit code. lock().unwrap() 9->0, 55+4 tests, clippy pedantic clean. Spec updated with delayed_ack contract + sans-io pattern + single-reader constraint.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `8e91437` | (see git log) |
| `c55edad` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 5: Relicense fork to Apache-2.0 with upstream MIT attribution

**Date**: 2026-06-08
**Task**: Relicense fork to Apache-2.0 with upstream MIT attribution
**Branch**: `main`

### Summary

Courteous relicensing of the adb_client v3.2.2 fork. Replaced LICENSE with Apache-2.0 full text; added NOTICE embedding the upstream MIT text verbatim (c) 2023-2024 Corentin LIAUD plus acknowledgements and nusb-migration note. Set workspace SPDX to 'Apache-2.0 AND MIT', added jdjingdian as author, pointed repository/homepage to the fork, aligned adb_cli RPM metadata license. Rewrote README as a grateful placeholder and fixed broken LICENSE-MIT badge links in subcrate READMEs. Verified MIT 2 compliance and that cargo metadata --no-deps accepts the SPDX expression. Deliberately did NOT rename crates to adboost (deferred to the planned refactor task).

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `64d7186` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 6: Fix delayed_ack/CNXN version contradiction (Android 16 USB hang)

**Date**: 2026-06-12
**Task**: Fix delayed_ack/CNXN version contradiction (Android 16 USB hang)
**Branch**: `main`

### Summary

External bug report: PersistentUsbConnection advertised delayed_ack while connecting CNXN at legacy version 0x0100_0000, but AOSP requires >= A_VERSION_SKIP_CHECKSUM (0x0100_0001) for windowed flow control. Android 16 adbd ignored the windowed OPEN -> open_session timed out after 10s. Fix (A)+(B): CNXN now connects at A_VERSION_SKIP_CHECKSUM iff features.delayed_ack; do_connect/do_auth return (device_version, banner) and negotiation is gated through new pure negotiate_delayed_ack() helper on device_version >= threshold. Added 5 sans-io regression tests. All quality gates green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `46d674f` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 7: Fix #2: magic-only message integrity (skip vestigial data_check at skip-checksum version)

**Date**: 2026-06-12
**Task**: Fix #2: magic-only message integrity (skip vestigial data_check at skip-checksum version)
**Branch**: `main`

### Summary

External bug report #2: CRITICAL regression from 46d674f. Bumping USB CNXN to A_VERSION_SKIP_CHECKSUM (0x01000001) for delayed_ack activated the peer's skip-checksum mode (data_check sent as 0), but check_message_integrity() still compared data_crc32 -> every payload-bearing inbound frame failed 'Invalid integrity ... got 0', killing CNXN for all delayed_ack devices. Deep analysis (parallel code map + AOSP source verification + latent-bug hunt) confirmed AOSP never validates data_check on receive in any version and found the same defect would kill live-session WRTE/windowed-OKAY, AUTH, and reverse/OPEN frames (whole connection dies, not just CNXN). Fix: magic-only integrity check (AOSP-faithful), runs for every frame incl. zero-payload (closing a pre-existing magic-skip gap). One core change covers all receive paths; no version flag needed. Send path unchanged. Added 4 sans-io regression tests + new adb-wire-protocol-contract.md spec documenting the version/delayed_ack/data_check coupling behind both regressions. All quality gates green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `09ca21e` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 8: Audit magic-only decision + harden USB receive path (data_length bound, reader fault-tolerance, bug #3 OPEN-rejection)

**Date**: 2026-06-12
**Task**: Audit magic-only decision + harden USB receive path (data_length bound, reader fault-tolerance, bug #3 OPEN-rejection)
**Branch**: `main`

### Summary

User asked to confirm the magic-only fix was optimal/maintainable and whether latent issues remain. Ran an adversarial audit (3-way debate: defend magic-only / attack-prefer-version-aware / AOSP-faithful judge) + latent-bug hunt. Verdict: magic-only is the AOSP-faithful, lowest-maintenance optimal choice (AOSP check_header never validates data_check in any version; version-aware would add Arc<AtomicBool> hot-path state for a vestigial byte-sum -> rejected). Audit surfaced a CRITICAL pre-existing OOM (unbounded vec![0; data_length] before any check) and downstream reported bug #3 (windowed OPEN 10s hang). Root-caused bug #3 against AOSP source: report hypothesis 2a confirmed (adbd rejection A_CLSE(arg0=0,arg1=local_id) routed to data channel, open_session only awaited ack channel -> silent timeout); 2e refuted (adbd writes header/payload as separate bulk writes). Fixed: A) MAX_PAYLOAD bound (relocated to always-compiled module, shared pure helper) on USB+TCP before alloc; B) reader fault-tolerance (only InvalidIntegrity recoverable -- post-payload-read & frame-aligned; ConversionError/bound-error/IO stay fatal -- trellis-check caught the implementer wrongly marking ConversionError recoverable, a real desync bug in the fix); C) open_session biased select over ack_rx+data_rx for fast-fail on CLSE + read_exact non-discard guard. Honest: bug #3 is hang->fast-fail, not windowed-OPEN-forced-success (needs real-device usbmon capture). Spec adb-wire-protocol-contract.md extended with check_header two clauses, CLSE-routing, reader resync invariant. +4 tests. All gates green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `6fec37e` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 9: Bug #3 TRUE root cause: CNXN banner trailing NUL corrupted last feature (delayed_ack) — device-verified fix

**Date**: 2026-06-12
**Task**: Bug #3 TRUE root cause: CNXN banner trailing NUL corrupted last feature (delayed_ack) — device-verified fix
**Branch**: `main`

### Summary

User connected a real Android-16 device (adb/xdb servers killed) to capture ground truth for bug #3. Built a throwaway /tmp diagnostic harness using the public subscribe_raw frame-tee + open_session(shell:getprop). Capture settled it decisively: windowed OPEN got CLSE(arg0=0, arg1=local_id) in ~1.8ms (hypothesis 2a confirmed, 2e refuted -- frame teed cleanly); classic mode succeeded. AOSP source research found the ONLY immediate-CLSE-on-OPEN path (adb.cpp:507 SupportsDelayedAck() != bool(arg1)) and that SupportsDelayedAck() keys purely on the host CNXN banner features= list (not the protocol version). Deeper AOSP dig found the real bug: adbd's StringToFeatureSet splits the feature CSV on ',' WITHOUT trimming and never strips the CNXN banner's trailing NUL, so to_banner_string()'s trailing \0 corrupted the LAST token ('delayed_ack\0' != 'delayed_ack') -> SupportsDelayedAck() false -> windowed OPEN(arg1=32MiB) rejected. shell_v2 (first token) masked it. Proved by temporarily removing the NUL on-device: windowed OPEN then succeeded with a 4-byte windowed OKAY grant [00,00,00,02]=32MiB. Fixed to_banner_string() (drop \0), corrected the false 'matches real adb' doc/comment, updated 3 banner tests + added regression lock, re-verified end-to-end on-device with the committed code (open_session SUCCEEDED ~13ms). Removed the diag harness. spec adb-wire-protocol-contract.md: new no-NUL-banner contract + bug #3 marked RESOLVED. Downstream may now drop the delayed_ack=false workaround. All gates green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `a0e39da` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 10: Library log->tracing migration (subtask A): emit-only, per-session local_id spans, RUST_LOG activation

**Date**: 2026-06-12
**Task**: Library log->tracing migration (subtask A): emit-only, per-session local_id spans, RUST_LOG activation
**Branch**: `main`

### Summary

Brainstormed a capability feature (parent task) to close gaps exposed by bugs #1/#2/#3: (1) adb_cli closed-loop validation, (2) controllable library observability. Decisions: rebrand adb_cli->adboost_cli + full async migration + persistent USB exerciser (subtask B); library log->tracing migration (subtask A). 'adboost' bare name reserved for the library's future. Split into 2 subtasks. Completed subtask A: mechanical rewrite of 69 log:: -> tracing:: sites; Cargo drop log, add tracing+log feature (backward compat for env_logger consumers); hot-path spans (do_connect/do_auth/reader_loop/writer_loop/open_session/open_shell_v2/open_sync_session) carrying local_id so RUST_LOG=[session{local_id=N}]=trace narrows to one session; library stays pure emitter (tracing-subscriber optional behind off-by-default tracing-init feature gating init_tracing_from_env with try_init). trellis-check caught a real async-span footgun (sync span.enter() held across .await in open_session -> span leaks across tasks) and fixed it to #[instrument(fields(local_id))] + Span::record. spec logging-guidelines.md rewritten for tracing incl. the async-span rule. All gates green (build/clippy/test x3 feature combos, fmt). Subtask B (adboost_cli) is next, depends on A.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `e4ed77d` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 11: adboost_cli rebrand + async migration + persistent USB exerciser (subtask B) — real-device closed-loop verified

**Date**: 2026-06-12
**Task**: adboost_cli rebrand + async migration + persistent USB exerciser (subtask B) — real-device closed-loop verified
**Branch**: `main`

### Summary

Completed subtask B (capability work). Rebranded adb_cli -> adboost_cli (git mv), migrated sync->async against the local workspace adb_client, added back to workspace members + release CI. ADBDeviceExt is now async (AFIT + trait_variant) and NOT dyn-compatible (boxed()/Box<dyn> removed), so the CLI's Box<dyn> dispatch was generic-ized to async fn run_command<D: ADBDeviceExt>. Byte streams bridged to tokio::io/tokio::fs. Dropped env_logger; CLI installs its own tracing-subscriber (RUST_LOG then --debug); library stays emit-only. Added a 'persistent' exerciser subcommand driving PersistentUsbConnection end-to-end with --no-delayed-ack (classic vs windowed, the bug-#3 control in one flag) + a negotiation self-check printing the first SESSION frame after OPEN (OKAY=accepted/CLSE=rejected) — formalizes the throwaway /tmp harness. Found+fixed a real library bug: adb_client's nusb dep lacked the 'tokio' feature, so real USB connect() panicked ('Awaiting blocking syscall without an async runtime'). trellis-check caught two rename-completeness misses (root README + rust-release.yml release pipeline still referencing -p adb_cli) and fixed them. Real-device verified on Android 16 (0e8d:201c): windowed -> delayed_ack negotiated=true, first frame OKAY payload_len=4, getprop->d02; classic -> negotiated=false, OKAY, getprop->d02. First-frame self-check initially mis-reported a buffered CNXN; fixed the subscribe_raw filter to session frames (Okay/Clse/Write) and re-verified on-device. spec directory-structure.md updated. Both subtasks (A tracing migration, B CLI) and the parent task are now complete + archived. All gates green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `19aa24a` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 12: adboost ADB server capability + CLI server start/kill daemon

**Date**: 2026-06-12
**Task**: adboost ADB server capability + CLI server start/kill daemon
**Branch**: `feat/adboost-server-capability`

### Summary

Implemented USB-backed ADB server in adb_client (feature 'server', phases 1-4): host-protocol pure fns, DeviceBackend trait + UsbDeviceBackend (nusb hotplug), AdbServerFrontend accept loop + shell:/tcp: bridge, ServerCapabilities. Architecture refactor: renamed misleading server/server_device modules -> proxy (ADBProxyServer/ADBProxyDevice) to free the 'server' name. Added USB serial addressing. CLI gained 'server start/kill' as a re-exec detached daemon with PID file + signal shutdown. Validated end-to-end against two real devices (XPENG d02, Qualcomm SA8155P): adb devices/devices-l/shell all work. 110 unit + 4 doctests, clippy pedantic clean. Follow-up: host:forward family + pyadb/examples migration to proxy API.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `6ebdfec` | (see git log) |
| `9b23064` | (see git log) |
| `68a80c1` | (see git log) |
| `0b24d8e` | (see git log) |
| `00d72b2` | (see git log) |
| `5efe2f6` | (see git log) |
| `0c5a86c` | (see git log) |
| `65c9736` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete

---

## 2026-06-12 — adboost server capability expansion (P1-P4) + CLI self-test harness

**Task**: `06-12-adboost-server-capability-expansion-forward-sync-shell-v2-reverse-cli-self-test-harness`

### Summary

Expanded the adboost ADB server frontend per the follow-up capabilities FR, and
added an interactive device-backed self-test CLI. Six logical PRs, all validated
against two real devices (XPENG d02 + Qualcomm SA8155P) and the official `adb`
client:

- **PR1** — `DeviceBackend` trait extended with backward-compatible defaulted
  methods (`capabilities()`, `open_sync_session`, `open_shell_v2`); honest
  capability negotiation: `host:features` advertises `sync_v2`/`shell_v2` only
  when the backend reports it implements them. (trait_variant 0.1.2 quirk: default
  bodies must be wrapped in `async move {}` since `make(Send)` rewrites
  `async fn` → `fn -> impl Future`.)
- **PR2** — `host:forward` family (forward/killforward/killforward-all/
  list-forward), AOSP-exact framing (two bare OKAYs; `tcp:0` → `%04x`+decimal
  port; list-forward = single OKAY + body; single FAIL on error). New
  `server/forward.rs` (pure parse + ForwardRegistry). Three entry points: `host:`,
  `host-serial:`, post-transport (our ProxyDevice path).
- **PR3** — `sync:` + `shell,v2` bridged **verbatim** (server is a byte pipe;
  client/device speak the sub-protocol end-to-end). New `ADBLocalCommand::Raw`
  pass-through. Gated on the negotiated banner.
- **PR4** — reverse: honest staged degradation (explicit FAIL "reverse not
  supported by this server", never advertised). End-to-end deferred: needs a
  host-side acceptor API for device-initiated OPENs that doesn't exist + would
  need unvalidated acceptor-role flow control. Rationale in
  research/p4-reverse-staging-decision.md.
- **PR5** — `adboost_cli selftest` + gtest-style reporter. Two channels: usb_direct
  (PersistentUsbConnection — multiplexes + clean CLSE on drop) and through_server
  (in-process frontend on ephemeral port + ADBProxyDevice client). Official-adb
  parity auto-detected. tcpip pre-wired SKIPPED. Multi-device by serial.
- **PR6** — interactive phase: USB replug + reboot recovery (120s timeout,
  excludes tcpip). README + server capability-matrix docs.

### Key finding (hardware)

The non-persistent `ADBUSBDevice` does not cleanly tear down adbd's USB endpoint
on drop, so rapid re-open reads stale frames (even across separate CLI processes).
usb_direct therefore uses `PersistentUsbConnection` (CLSE on drop + multiplexing).
USB single-exclusive-claim also forced phase ordering: all usb_direct first, then
the server's cached claims. Live result: 18 passed / 3 skipped on both devices.

### Testing

- [OK] adb_client: 135 unit + 4 doctests (default + `--features server`)
- [OK] adboost_cli: 16 unit
- [OK] clippy pedantic clean: default, `--features server`, `--features usb`
- [OK] live `adboost_cli selftest`: 18 passed / 3 skipped on 2 real devices + official-adb parity

### Notes

- Pre-existing fmt drift in persistent.rs / proxy/commands/devices.rs / daemon.rs /
  main.rs left untouched (not this task's to sweep); all new files are fmt-clean.

### Status

[OK] **Completed** — pending user acceptance.

---

## 2026-06-13 — P4 reverse end-to-end (subtask 06-13-reverse-acceptor)

**Task**: `06-13-reverse-acceptor` (subtask of the 06-12 capability expansion).

### Summary

Replaced the staged reverse FAIL with a real, device-validated end-to-end
implementation. `adb reverse` now works through adboost's server: iperf3 reverse
measured **335 Mbits/sec** (sender) / **322 Mbits/sec** (receiver) on a real
device — on par with official adb.

- **accept_device_open** (acceptor role) in PersistentUsbConnection: mirror of
  open_session minus OPEN send/await; AOSP-verified arg order (OKAY arg0=our_id,
  arg1=device_id); windowing keyed on connection-level delayed_ack; send-window
  seeded from the OPEN arg1 grant.
- **incoming_opens** changed to `&self` (Mutex<Option>) so an Arc-shared backend
  can pump device-initiated OPENs.
- **DeviceBackend** reverse API (open_reverse/reverse_remove/reverse_remove_all/
  list_reverse) + `BackendCapabilities::reverse` + **ReversePolicy** (library-
  configurable security: RejectUnconfigured default / AllowAll / Custom; CLI uses
  RejectUnconfigured). UsbDeviceBackend owns the reverse data path: lazy per-serial
  ReverseState (rules + inbound-open pump), accept → dial host → bridge.
- **frontend** routes reverse:forward/killforward*/list-forward to the backend,
  AOSP double-OKAY framing (native adb stays in sync), single OKAY+body for list.
- **selftest**: reverse_echo (always) + reverse_iperf3 (auto when device has it),
  both PASS on both real devices.

### Two protocol bugs found + fixed on the device (and captured in the spec)

1. **read_exact over-read**: a bulk IN completion can return MORE than the
   requested field (max_packet_size alignment + controller coalescing under
   sustained device→host throughput). The old fatal "frame desync" guard tore
   down the whole multiplexed connection on the first large reverse WRTE. Fixed
   with a residual carry-buffer (`fill_and_carry`, unit-tested). The prior spec
   assumption ("adbd writes header/payload separately so never over-delivers")
   was WRONG on the IN path — spec corrected.
2. **reader frame reads were select!ed against control_rx** → not cancel-safe; a
   Register/Unregister mid-frame corrupted an in-flight WRTE, stalling one of two
   concurrent device→host streams. Fixed: drain control between frames (non-
   cancelling), re-drain before classify to keep register-before-route. Spec
   updated with the atomic-frame-read contract.
   (+ defensive `read_residual.clear()` on connect/disconnect so a fresh CNXN
   never consumes a stale frame.)

### Testing

- adb_client 154 unit + 4 doctests (default & server); 86 (usb, incl. fill_and_carry).
- selftest on 2 real devices: reverse_echo + reverse_iperf3 PASS; 0 failures.
  (usb_direct flaky-skips on the Qualcomm device — pre-existing PersistentUsbConnection
  rapid-reclaim timing, not the reverse work; XPENG passes it.)
- fmt clean; clippy pedantic clean on default / server / usb.

### Status

[OK] **Completed** — reverse end-to-end working, pending user acceptance.


## Session 13: adboost server P1-P4 (forward/sync/shell-v2/reverse) + interactive self-test harness

**Date**: 2026-06-13
**Task**: adboost server P1-P4 (forward/sync/shell-v2/reverse) + interactive self-test harness
**Branch**: `main`

### Summary

Expanded the adboost ADB server frontend per the follow-up capabilities FR: P1 host:forward family (AOSP-exact framing), P2 sync: + P3 shell,v2 bridged verbatim with honest capability negotiation, and P4 reverse end-to-end (device-initiated OPEN acceptor + ReversePolicy + per-serial pump). Added an interactive device-backed self-test CLI (adboost_cli selftest, gtest-style) covering USB-direct + through-server channels, forward, reverse (echo + iperf3), official-adb parity, multi-device, USB replug/reboot recovery. Validated on two real devices incl. iperf3 reverse at 335 Mbits/sec. Fixed two device-found USB protocol bugs (bulk IN over-read carry-over; non-cancel-safe reader frame reads) and corrected the wire-protocol spec contract. 154 unit + 4 doctests, clippy pedantic clean on default/server/usb.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `866dac4` | (see git log) |
| `f5ef847` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 14: Export composable usb::ReverseEngine for external DeviceBackend impls

**Date**: 2026-06-13
**Task**: Export composable usb::ReverseEngine for external DeviceBackend impls
**Branch**: `main`

### Summary

Promoted the reverse data path from a server-private state machine to a public, per-connection usb::ReverseEngine (new + open/remove/remove_all/list) so any acts-as-a-server backend (xdb) delegates reverse in four lines, symmetric with sync/shell_v2. PR1: moved ReversePolicy to usb:: (server:: re-export) and extracted the shared half-close bridge as pub usb::bridge_tcp_session. PR2+3: ReverseEngine body (absorbs run_reverse_command; pump dials via bridge_tcp_session; rule/policy split into connection-free RuleSet for hardware-free tests), UsbDeviceBackend delegates, server/reverse.rs deleted, no_run doctest. Documented the key contract in spec: reverse data-plane belongs to whoever is the device's server — proxy-style backends forward the reverse: command instead of using the engine, to avoid racing for single-consumer incoming_opens. 158 tests + 5 doctests + clippy (default/usb/server) green; no wire-protocol change.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `3e96e47` | (see git log) |
| `ec22bd2` | (see git log) |
| `c19e7c6` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 15: Fix tport:any error wording for multi-device (no -s)

**Date**: 2026-06-15
**Task**: Fix tport:any error wording for multi-device (no -s)
**Branch**: `main`

### Summary

Fixed select_tport collapsing all failures to 'device not found'; multi-device adb shell with no -s now reports AOSP 'more than one device'. Each selector branch carries its own correct reason. Added 5 frontend unit tests + a device-backed selftest parity case, and a new backend spec documenting the transport-selection error-wording contract.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `087ee85` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 16: Fix reboot-recovery selftest + through-server shell exit code

**Date**: 2026-06-15
**Task**: Fix reboot-recovery selftest + through-server shell exit code
**Branch**: `main`

### Summary

Interactive selftest surfaced two defects. (1) interactive.reboot_recovery failed with 'session channel closed': reboot was issued over shell:reboot and the read hit BrokenPipe when the device tore the stream down. Added PersistentUsbConnection::reboot using the dedicated reboot: service (open_session confirms OKAY, no EOF read). (2) through_server.shell_exit_code was SKIPPED. Diagnostics revealed the proxy's host:features (sent after host:transport:<serial> on the same connection) was rejected by the server frontend as 'service not supported', forcing fallback to v1 shell with no exit codes. Fixed the frontend to answer post-transport host:features/host:version from negotiated caps. Also hardened the proxy shell-v2 decoder to return the captured exit code on trailing EOF (Ok(exit) not Ok(None)) and extracted it into a unit-tested free function. reboot_recovery verified PASS on hardware; shell_exit_code fix verified by diagnostics + new server unit test. 168 adb_client tests + 21 cli tests green, clippy clean.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `c7a09d1` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete

---

## 2026-06-15 — tcpip mainline parity (PR1–5 + PR4a/b split)

Task: `06-15-tcpip-mainline-parity-...`. Closes the tcpip gap vs official adb and
fixes the reported `error: unknown host service: connect:127.0.0.1:8885`.

Shipped (all green: 192 lib + 5 doc + 21 cli tests, clippy clean, all feature combos build):
- **PR1**: `tcpip(port)->Result<String>` + `usb()` on `ADBDeviceExt`; impl for
  `ADBMessageDevice` (direct USB/TCP) + `ADBProxyDevice` (folded in existing
  proxy cmds). `commands/tcpip.rs` reads the WRTE ack; proxy reads the streamed
  tail via new `TCPProxyTransport::read_raw_to_end`.
- **PR2**: CLI `tcpip`/`usb`/`remount`/`disable-verity`/`enable-verity` on
  usb/tcp/local (extracted `run_control_command` to stay under clippy line cap).
- **PR3**: server `map_local_service` bridges control services verbatim via
  `is_control_service` (tcpip/usb/root/reboot/remount/verity) — they're shaped
  like `shell:` v1 so `bridge_tcp_session` already handles them; no new path.
- **PR4a**: renamed `UsbDeviceBackend`→`DefaultDeviceBackend` (deprecated alias),
  added TCP device registry + `host:connect`/`disconnect` arms + unified device
  table (`merge_device_sets`). **Fixes the user's bug.**
- **PR5**: `host:wait-for-*-device` (poll, 60s bound) + `host:reconnect-offline`.
- selftest: `official_adb_connect_routing` parity case (non-destructive,
  loopback:1) guards the connect bug at runtime; tcpip SKIP message updated.

### Non-obvious finding (recorded in server-host-protocol.md)
`MultiplexedSession` + the 3140-line `PersistentUsbConnection` multiplexer are
hard-typed to `USBTransport`. So "register a TCP device" (cheap) and "bridge a
client shell THROUGH to a TCP device" (deep refactor) are different jobs — hence
the PR4/PR4a+PR4b split. PR4a lists/selects TCP devices; `open_local_service`
against a TCP serial returns a stable "not yet supported".

### Deferred
- **PR4b** (task #6, blocked-by #4): generalize the multiplexer over
  `ADBMessageTransport` for TCP shell/sync bridging. HIGH RISK — must preserve the
  3 device-verified wire regressions. Not started.
- PR6 (pair/mDNS) + PR7 (keygen/sideload/...) out of this task's scope.

### Verification gap (honest)
`connect` happy-path (real TCP handshake) + shell-over-TCP aren't unit-tested
(need hardware / PR4b). Covered by the connect-routing parity case + pure-helper
tests. No git commit made (awaiting user acceptance).

## 2026-06-15 (cont.) — PR4b: transport-generic multiplexer

Generalized `PersistentUsbConnection` → `PersistentConnection<T: ADBMessageTransport = USBTransport>`
(alias kept) so the server bridges shell/sync/tcp THROUGH to `host:connect`d TCP
devices. Sessions stay non-generic (channel-only), so `open_*` return types are
unchanged. USB ctors in `impl PersistentConnection<USBTransport>`; `new_from_tcp_addr`
in the TCP impl. Backend TCP registry now holds `Arc<PersistentConnection<TcpTransport>>`;
TCP serials route to it in all three open methods (guard removed).

Done via trellis-implement → trellis-check sub-agents.
- **trellis-check caught a serious latent bug**: the STLS branch had a post-upgrade
  double-read (`finish_after_stls`) that would HANG `host:connect` to any TLS device,
  because `TcpTransport::upgrade_connection()` already consumes the post-STLS CNXN
  (matches `ADBMessageDevice::connect`). Fixed: return `(A_VERSION_LEGACY, "")` right
  after `upgrade_connection()`. Recorded the gotcha in adb-wire-protocol-contract.md.
- 3 regression-locked behaviors confirmed byte-equivalent (delayed_ack/version/data_check,
  CNXN no-NUL banner, CLSE-on-data fast-fail). All their unit tests pass.
- Green: adb_client 195 lib + 5 doc, adboost_cli 21; clippy pedantic 0 warnings;
  fmt clean; default + no-default + usb-only builds all pass.

### Verification gap (hardware)
The live TLS `host:connect` handshake (new STLS branch) and the interactive
tcpip→connect→shell-through→usb end-to-end are NOT run (need a wireless device +
TLS adbd). Ordering now mirrors the device-verified direct-TCP path. Plain
(non-TLS) `host:connect` is fully exercised in logic.

PR1–5 + PR4a/b all complete. Awaiting user acceptance, then commit.

## 2026-06-15 — ACCEPTED (real-device selftest)

User ran `adboost_cli selftest` on 2 real devices (XPENG d02, SA8155P-ADP).
28/29 passed. The single FAILED — `tcpip.shell_through_tcp_device` — is an
ENVIRONMENT limitation, not a code defect: the device's adb-over-tcp IP
(172.20.1.45, its ethernet internal IP) is not routable from the host, so
`host:connect` times out. Validating it needs device+host on the same reachable
network (WiFi / USB-tethering), unavailable here.

Crucially, the `host:connect` ROUTING itself IS verified on hardware:
`parity.official_adb_connect_routing` PASSED (real `adb` client → adboost server),
which is the exact path of the originally-reported `unknown host service:
connect:` bug. Also green on real devices: through_server shell/shell_v2/
push-pull/forward/reverse/iperf3 (both devices), interactive usb_replug +
reboot_recovery.

User accepts: tcpip mainline parity complete; the TCP-link end-to-end is
deferred to an environment that has a reachable wireless link. Per user
decision, the interactive case stays FAILED-on-unreachable (no skip softening).


## Session 17: tcpip mainline parity (PR1-5 + PR4a/b)

**Date**: 2026-06-15
**Task**: tcpip mainline parity (PR1-5 + PR4a/b)
**Branch**: `feat/tcpip-mainline-parity`

### Summary

Closed the tcpip gap vs official adb and fixed 'unknown host service: connect:'. Added tcpip/usb to ADBDeviceExt (direct+proxy) + CLI verbs; server bridges device control services (tcpip/usb/root/reboot/remount/verity); added host:connect/disconnect, wait-for-*, reconnect-offline with a unified USB+TCP device table; renamed UsbDeviceBackend->DefaultDeviceBackend; generalized the persistent multiplexer to PersistentConnection<T> so the server bridges shell/sync/tcp through to host:connect'd TCP devices (3 wire regressions preserved; STLS double-read bug caught+fixed in review). Real-device selftest 28/29; the one FAILED is an env-only unreachable TCP link, host:connect routing itself verified green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `c6447d7` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 18: Rename library crate adb_client -> adboost (main line)

**Date**: 2026-06-15
**Task**: Rename library crate adb_client -> adboost (main line)
**Branch**: `main`

### Summary

Completed the long-deferred library crate rename adb_client -> adboost across the two active workspace members (library + adboost_cli). Decision context: the fork is fully detached from upstream cocool97/adb_client (async rewrite + reshaped API: proxy/ rename, new server/ module, nusb), so upstream patch-pulling is already manual-only and the old crate name no longer served as a compat anchor. git mv adb_client/ -> adboost/ (129 renames, history preserved); name='adboost'. Updated workspace root (members, header comment, dropped the now-meaningless [patch.crates-io] adb_client stanza), adboost_cli's 16 .rs files + path dep, library handshake host identity (adb_client@ -> adboost@ in adb_rsa_key.rs — note: already-authorized devices may re-prompt for USB debugging), rustdoc/log-target/docs.rs-badge docs, benches, release CI (cargo publish -p adboost), and 8 backend spec docs (retired the 'reserved for future rename' note). Three reference classes handled distinctly: crate-name -> adboost; cocool97/adb_client attribution links PRESERVED (courtesy); bug-report links RETARGETED to magicdian/adboost/issues so our issues don't disturb upstream. pyadb_client/ and examples/ intentionally untouched (off main line, already broken vs current API; path deps dangle by design, separate future migration). Verified green: workspace build, feature combos (server/usb/mdns), lib(4 doc)+cli(21) tests, doctests(5 all-features), clippy pedantic 0 warnings, bench compile, fmt. Both acceptance greps empty. trellis-implement + trellis-check both passed with no fixes.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `0a55c91` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 19: Expose TCP connection building blocks for external backends + fix two latent TCP-path bugs

**Date**: 2026-06-22
**Task**: Expose TCP connection building blocks for external backends + fix two latent TCP-path bugs
**Branch**: `main`

### Summary

调用方 xdb 需在自定义 device backend 中持有持久化 TCP 连接，但 TcpTransport 不可命名。暴露 TcpTransport + 公开别名 PersistentTcpConnection + TcpConnectOptions（标准 Default + 链式定制）并从 crate root re-export（a3b1a91, semver minor）。打通后真机 selftest 逐层暴露两个 pre-existing bug：(1) host-serial:<serial>:<sub> 用 split_once(':') 切分，对 ip:port serial 失败——改为锚定已知 sub-service 切分（a80dfd0）；(2) transport-generic reader 只认 USB 的 UsbTimeout，TCP 的 IOError(TimedOut) 被误判致命拆连接——在 ADBMessageTransport trait 层统一为非门控的 ReadTimeout，移除 UsbTimeout（4951301, breaking）。三处均全特性组合门禁绿 + 真机 selftest tcpip.shell_through_tcp_device 验证通过。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `a3b1a91` | (see git log) |
| `a80dfd0` | (see git log) |
| `4951301` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 20: Fix TCP_NODELAY on TcpTransport connect (bug 1 of TCP shell report)

**Date**: 2026-06-22
**Task**: Fix TCP_NODELAY on TcpTransport connect (bug 1 of TCP shell report)
**Branch**: `main`

### Summary

External bug report (TCP/IP host:connect shell). Verified both reported bugs in source. Bug 1 (fixed): TcpTransport::connect never set TCP_NODELAY, so interactive adb shell over host:connect lagged a keystroke-RTT each (Nagle). Added set_nodelay(true) after socket build (also covers TLS upgrade, same socket) + hermetic loopback unit test. Bug 2 (per-device shell_v2 over-advertising via global capabilities()) confirmed real but DEFERRED to a separate task+brainstorm; report's premise that device_features() already exposes the device's banner features is WRONG — that field is what adboost advertises TO the device; the device banner is parsed only for delayed_ack and the full set is discarded, so a real fix needs new banner-feature plumbing. Testbed: hypervisor Yocto-Linux stripped adbd reached via Android tcp:6665 forward — one backend genuinely fronts two devices with different caps.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `e90ab60` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 21: Per-device capability negotiation (bug 2 of TCP shell report)

**Date**: 2026-06-22
**Task**: Per-device capability negotiation (bug 2 of TCP shell report)
**Branch**: `main`

### Summary

Fixed bug 2 from the external TCP/IP host:connect shell report: global DeviceBackend::capabilities() over-advertised shell_v2/sync_v2 to feature-less devices, so a stripped adbd (empty features= banner, reached via adb forward tcp:N tcp:6665 + adb connect) CLSE'd every shell,v2 OPEN. Made negotiation per-device, sourced from each device's CNXN banner. New: DeviceFeatureSet::from_banner parser (round-trip test caught a real host::features= placement bug), PersistentConnection::peer_features() storing the DEVICE's advertised set (distinct from device_features() = what we advertise — the conflation the report made), DeviceEntry.capabilities: Option<DeviceFeatureSet>, DeviceBackend::device_capabilities(serial,timeout) with default None, DefaultDeviceBackend cache+query (timeout on the on-demand single-device call, not list_devices). Frontend: post-transport + host-serial host:features reply server_caps ∩ device_caps (client picks v1 gracefully); defense-in-depth gate fallback FAILs shell,v2/sync cleanly. Banner mapping: shell_v2⟸shell_v2, sync_v2⟸stat_v2. BREAKING: DeviceEntry public field + new trait method (default impl). All hermetic unit tests incl. e2e mixed full/stripped device; no special hardware. Spec updated (server-host-protocol.md two-axis contract). Corrected the report's wrong premise that device_features() already held the device banner.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `67cc53e` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 22: SEG A nodelay miss — client-facing frontend sockets (TCP shell lag follow-up)

**Date**: 2026-06-22
**Task**: SEG A nodelay miss — client-facing frontend sockets (TCP shell lag follow-up)
**Branch**: `main`

### Summary

Follow-up to the reported still-present nodelay lag. Confirmed the earlier fix (e90ab60) only set TCP_NODELAY on the device-facing socket (SEG B); the client-facing socket (SEG A: adb client → :5037 frontend) was never set, so interactive shell echo was Nagle-delayed an RTT per keystroke on BOTH IP-direct and forwarded-port paths (SEG A is shared by all devices). The reporter's diagnosis was correct and precise. Fixed by setting TCP_NODELAY right after each client accept() in frontend.rs — the main :5037 accept loop and the host:forward listener accept — via a shared enable_client_nodelay helper. Set at accept (not in bridge_tcp_session, which the reverse host-dial path reuses and already sets nodelay; and the main socket needs nodelay during the pre-bridge host-protocol handshake). set_nodelay failure is logged-and-tolerated (live accepted socket; latency not correctness), mirroring reverse_engine.rs. Hermetic loopback test asserts the server-side accepted socket has nodelay()==true. trellis-implement + trellis-check sub-agents; clippy/fmt/test all green (219 tests), default build unaffected (server-gated). xdb cannot fix this — socket is entirely inside the frontend.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `fd5e624` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 23: TcpTransport split read/write halves — fix interactive shell ~2s lag

**Date**: 2026-06-22
**Task**: TcpTransport split read/write halves — fix interactive shell ~2s lag
**Branch**: `fix/tcp-transport-split-read-write-halves`

### Summary

Root-caused TCP adb shell ~2s/key lag to reader holding a shared Arc<Mutex<socket>> across its 1s read timeout, serializing the writer. Fixed via tokio::io::split into independent read/write half locks (aligning TCP to USB's separate-endpoint design); CurrentConnection now impls AsyncRead/AsyncWrite delegation (no unsafe). TLS upgrade unsplit/re-split preserves STLS timing. set_nodelay kept. Added loopback regression test + spec invariant.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `1e28628` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 24: Transport cancel-safety bug class: shared FrameReadBuffer + frame-atomic write timeout + hardening

**Date**: 2026-06-22
**Task**: Transport cancel-safety bug class: shared FrameReadBuffer + frame-atomic write timeout + hardening
**Branch**: `main`

### Summary

Investigated a TCP read cancel-safety report; a 6-lens adversarial review found it was one of a recurring class ('the async/TCP path lacks a USB robustness guarantee'). Class A (1aac71c): shared sans-io FrameReadBuffer enforcing 'a timeout is never observed mid-frame' — fixes TCP read partial-byte desync (the reported ifconfig disconnect), TCP write truncation poisoning, and USB multi-transfer-field partial loss. Hardening: recv_file short/empty-frame panic guard (23c2078), proxy framebuffer (5bd58ae) and LIST/RECV (f45e91d) unbounded wire-length allocs, is_alive() reflects writer (584dd75). Then fixed a regression the Class A writer teardown introduced — saturating reverse_iperf3 tore down on normal backpressure — with frame-atomic write timeout / Scheme B (ea88205): start-gate WriteTimeout is recoverable, only mid-frame truncation is fatal. Plus selftest reboot_recovery now runs last (bfcd337). All verified by user; quality gate green (fmt, clippy pedantic default+usb, full workspace tests).

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `1aac71c` | (see git log) |
| `23c2078` | (see git log) |
| `5bd58ae` | (see git log) |
| `f45e91d` | (see git log) |
| `584dd75` | (see git log) |
| `ea88205` | (see git log) |
| `bfcd337` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 25: 断开自动释放 forward/reverse 规则（OnDisconnect 策略 + ForwardHandle）

**Date**: 2026-06-23
**Task**: 断开自动释放 forward/reverse 规则（OnDisconnect 策略 + ForwardHandle）
**Branch**: `main`

### Summary

为 adboost server 增加 transport 断开时自动释放 forward/reverse 规则的能力，默认对齐标准 adb（断开即释放），可 opt-out。新增 OnDisconnect 策略（ReleaseAll/Retain/Notify，仿 ReversePolicy）、DeviceBackend::subscribe_lifecycle 事件流（独立于 track-devices，由 nusb hotplug diff + TCP disconnect 驱动）、release_reverse（不重开死连接）、ForwardHandle 主动清理 API（release/release_all，统一管 forward+reverse）、ForwardRegistry::remove_by_serial。frontend 订阅事件按策略释放。真机端到端验证通过（新增 selftest case_usb_forward_release_on_unplug：官方 adb 注册 forward→拔 USB→断言 list 自动清空），并修复既有 selftest 跨-case USB 重新枚举时序脆弱性（open_device_with_retry）。契约写入 server-host-protocol.md。质量门全绿：252 lib + 21 cli 测试通过。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `82006cc` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 26: host-usb/host-local + transport-usb/local for adb -d/-e

**Date**: 2026-06-23
**Task**: host-usb/host-local + transport-usb/local for adb -d/-e
**Branch**: `feat/host-usb-local-transport-kind`

### Summary

Implemented transport-kind selection so adb -d/-e work against the adboost server frontend, aligned with native adb. Added TransportKind{Usb,Local} + DeviceEntry.kind:Option<_> (#[non_exhaustive], with_kind builder), DefaultDeviceBackend tags USB/Local. One shared resolve_single_by_kind funnels all selection paths with byte-exact AOSP per-kind error wording (verified against adb 35.0.2). Stripped host-usb:/host-local: prefixes (reusing dispatch_host_serial), added transport-usb/local arms, kind-filtered wait-for. Updated server-host-protocol.md in lockstep; 18 new tests; fmt+clippy(pedantic)+270 tests green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `bbc2b3e` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 27: Fix adb -d/-e: select_tport kind tokens (tport:usb/local)

**Date**: 2026-06-23
**Task**: Fix adb -d/-e: select_tport kind tokens (tport:usb/local)
**Branch**: `main`

### Summary

Follow-up to the host-usb/transport-kind task: adb -d/-e still failed 'device not found' because modern adb's phase-2 switch is host:tport:usb/local (not transport-usb), and select_tport parsed usb/local as a serial. Extracted pure pick_single_by_kind helper (resolve_single_by_kind now wraps it), added usb/local kind-token branch to select_tport reusing the already-fetched device slice, kept serial:<s> resolving by serial. Verified end-to-end with real adb client against in-process server (adb -d shell/getprop/get-state all work). 7 new tests, spec updated lockstep, fmt/clippy/tests green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `43217d2` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 28: adb root reconnect handshake + unroot + USB re-enumeration retry

**Date**: 2026-06-23
**Task**: adb root reconnect handshake + unroot + USB re-enumeration retry
**Branch**: `main`

### Summary

Fixed 3 xdb-reported frontend/backend gaps. (1) unroot: mirrored root as a first-class capability across ADBLocalCommand/ADBDeviceExt/proxy/message/usb/tcp + is_control_service whitelist. (2) adb root reconnect handshake: routed host-transport-id:<N>:<sub> (top-level family prefix, corrected from xdb's host:transport-id:N: report), wait-for-* sub-services, and a new disconnect state in serve_wait_for pinned to the specific serial. (3) Real production bug: backend raced the not-ready USB endpoint after adbd restart/replug (IOKit 0xe00002ed NotResponding / 0xe00002c0 NoDevice) with zero retry — fixed via do_connect transient-retry (CNXN race, all consumers) + get_or_open/open_session_with_reopen bounded retry outside the conns lock (first-OPEN race + brief absence). Added a behavioral root_unroot selftest case running through the in-process server (exercises the real adb root path; production builds report Skipped) + a scripted-mock-transport contract test. Verified on real hardware. Transient WARN/ERROR log noise during re-enumeration documented as expected.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `19b86d4` | (see git log) |
| `f84039a` | (see git log) |
| `423efc9` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 29: adb root/unroot reconnect handshake: two-OKAY + event-driven disconnect + connect-layer re-enumeration recovery

**Date**: 2026-06-24
**Task**: adb root/unroot reconnect handshake: two-OKAY + event-driven disconnect + connect-layer re-enumeration recovery
**Branch**: `main`

### Summary

Fixed xdb-reported adb root/unroot reconnect handshake issues across two iterations, all verified on real MTK hardware. (1) wait-for: serve_wait_for now sends two OKAYs (was one -> protocol fault) and wait-for-disconnect is event-driven via a new LifecycleEvent::TransportReset fired on cached-connection reader death (DeathSignal AtomicBool+Notify), replacing the broken 60s presence poll (adbd restart != USB re-enumeration on MTK). (2) connect-layer re-enumeration recovery: real-hardware trace overturned an initial in-place-CNXN-budget approach -- a re-enumerated device gets a new IOKit registry id so the old transport endpoints are permanently dead; in-place retries spin uselessly. Reversed to: tiny in-place transient arm (CONNECT_TRANSIENT_MAX_ATTEMPTS=3) + outer get_or_open/retry_within owns recovery by rebuilding the transport within a 10s wall-clock budget, with a variant-family transient classifier (Unknown(_) catch-all) ending the per-IOKit-code whack-a-mole. Added examples/root_disconnect_probe.rs diagnostic harness that drove the data-first design. Known-acceptable residual (documented in spec): a back-to-back control service can return silently when adbd tears the stream down before its reply text; command still takes effect, native adb shows the same race.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `0977368` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete

---

## Session 30: SimulatedDevice software ADB test harness (Phases A/B/C)

**Date**: 2026-06-24
**Task**: 06-24-simulateddevice-software-adb-test-harness (parent) + sim-phase-a/b/c subtasks
**Branch**: `sim-harness-phase-a`

### Summary

Built a comprehensive, hardware-free protocol test harness so the bug classes that
kept escaping to xdb (delayed_ack negotiation, reconnect/re-enumeration, framing
desync, host-protocol parity) are now caught by adboost's own `cargo test`. Three
research agents first mined the escaped-bug history, an ~80-edge protocol-state
catalog, and the two recurring bug classes; the harness was scoped to that catalog.

Delivered in three independently-committed phases (parent + 3 subtasks):

- **Phase A** — `SimulatedDevice` (frame-level `ADBMessageTransport` adbd state
  machine: CNXN/AUTH/banner/delayed_ack + outbound queue, empty→ReadTimeout) +
  `DeviceProfile` (android_11/16, auth, featureless) + `Scenario` (fault injection)
  + `ChunkedTransport` (byte-level, reassembles via the shared `FrameReadBuffer`).
  Replaced the 4 fixed-script `ScriptedTransport` `do_connect` tests with stateful
  end-to-end equivalents through `PersistentConnection::new`. 18 tests; regressions
  B1 (android_11 classic flow control), B2 (data_check=0 accepted).
- **Phase B** — session state machine (OPEN/OKAY/WRTE/CLSE, double-OKAY, reject,
  echo, early-close) + `ChunkedTransport` byte faults. 15 tests; regressions B3a
  (early-CLSE fast-fail), B8 (half-open is_alive), B-recv (short SYNC frame no
  panic), B4/B5/B7/B9 (cancel-safety: split/coalesce/truncation/backpressure).
- **Phase C** — `SimDeviceBackend` + `SimRegistry` over the `DeviceBackend` trait,
  driving the smartsocket frontend end-to-end. 8 tests: host:devices, -d/-e kind
  selection, per-device honest host:features (B-feat), the real shell: bridge
  round trip (closing the `MockBackend` `unimplemented!()` gap), wait-for-disconnect
  on a real connection death, back-to-back restart recovery via reopen.

### Main Changes

- New `message_devices/usb/sim/` module (mod/device/chunked/profile/scenario/state
  + 2 test files), gated `#[cfg(any(test, feature = "test-support"))]`.
- New `server/sim_backend.rs` (`SimDeviceBackend`/`SimRegistry`), same gating.
- New `test-support` cargo feature (implies `usb`) exposing the harness to external
  test crates (CLI selftest, xdb) that cannot see `cfg(test)` symbols.
- Gated observability seams: `PersistentConnection::delayed_ack_negotiated()` and a
  `pub(crate)` `frontend::handle_client` wrapper (test/test-support only).
- New `SimulatedDevice::kill` / `SimState::kill_reader` for the restart edge.

### Key Decisions / Lessons

- Two complementary mocks at two layers: a frame-level `SimulatedDevice` for the
  bulk of the protocol/state-machine edges, plus a byte-level `ChunkedTransport`
  for the sub-frame cancel-safety class (B4/B5/B7/B9) a whole-frame mock cannot
  reach. The byte-layer framing bugs already had `framed_read.rs` regression tests,
  so the sim's unique value is the *consumer-side* (reader/writer-loop) guarantees.
- An idle read must SLEEP its deadline before returning ReadTimeout, else the
  spawned reader loop busy-spins and starves the test task (virtual under
  `start_paused`).
- `do_connect` issues single-shot reads with a huge timeout: a timeout-aware
  `ChunkedTransport` (assemble-whole under a large deadline, partial under a short
  one) carries a clean handshake AND exercises the idle-timeout path on the same
  trickled stream.
- Honest boundary (documented in the module): the sim does NOT prove real IOKit
  codes/latency, TLS, or IOKit re-enumeration to a new registry id — only the
  reopen-layer *reaction*. Those stay hardware tests.

### Testing

- [OK] `cargo fmt --all --check`; `cargo clippy --all-targets -D warnings` across
  default / usb / server / test-support / server,test-support — all clean.
- [OK] 337 lib tests pass under `server,test-support` (41 sim across the 3 phases);
  default (58) + `adboost_cli` build unaffected.

### Status

[OK] **Completed** (3 commits: Phase A 4a61942, Phase B 49123b3, Phase C 274ca0e)

### Next Steps

- Optional: enable `test-support` from `adboost_cli` selftest / xdb to reuse the
  harness for their own regression suites.


## Session 30: backend hook: local-service reject reason

**Date**: 2026-06-24
**Task**: backend hook: local-service reject reason
**Branch**: `main`

### Summary

Evaluated xdb feature request and implemented it as a contract-layer fix: new defaulted DeviceBackend::local_service_reject_reason(serial, service, default_reason) -> Option<String> hook, consulted in serve_local_service before the hardcoded FAIL on a map_local_service rejection. Err->Err only (reason text, never routing/gating); one unified seam over all map rejections with per-service None fallback; default_reason passed for wrap-not-just-replace; default None => byte-identical for non-overriding backends. Added 3 round-trip tests (override+wrap, untargeted fallback, byte-identical default) and documented the seam+invariants in server-host-protocol.md. fmt/clippy clean, 340 tests + 6 doctests green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `6d5ee3e` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 31: Release USB claim on PersistentConnection half-death edge

**Date**: 2026-06-26
**Task**: Release USB claim on PersistentConnection half-death edge
**Branch**: `fix/persistent-release-claim-on-half-death`

### Summary

Fixed a downstream-reported (xdb) permanent USB DeviceBusy leak: when a PersistentConnection's reader died single-sided while the writer parked on recv(), the writer's transport clone was never dropped, pinning the shared nusb Interface claim until the last external Arc dropped. Bound resource release to the death edge instead of refcount-zero — each I/O loop now watches the shared DeathSignal and returns when the other half dies (writer races recv() vs closed.wait(); reader checks is_dead() at its idle ReadTimeout boundary, preserving frame-read cancel-safety). Graceful shutdown/close/Drop and flow control unchanged; transport-generic (USB+TCP). Regression-locked in the sim harness via a strong_count probe (3->1 on death). Full suite green (341 lib tests), clippy/fmt clean. adboost_cli selftest intentionally out of scope (cannot deterministically trigger reader-only death on real hardware).

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `0a16916` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 32: shell-v2 shared layer: writable/streaming/cancelable + PTY, USB+proxy symmetric

**Date**: 2026-06-26
**Task**: shell-v2 shared layer: writable/streaming/cancelable + PTY, USB+proxy symmetric
**Branch**: `main`

### Summary

Converged shell-v2 into one shared layer across USB and proxy. S1: extracted a single ShellChannel(0..5)+encode/decode codec into always-compiled message_devices/, deleted both duplicated enums/decoders. S2: replaced stringly-typed ShellCommand(String,Vec<String>)+hardcoded ,raw: with typed ShellV2Service{cmd,term,ShellPtyMode} (pty/raw mutually exclusive at type level), renders shell,v2[,TERM][,raw|pty]:cmd. S3: ShellV2Session<R,W> generic over split AsyncRead/AsyncWrite with read_frame/write_stdin/close_stdin/execute; USB binds via into_split, cancel=drop. S4: ADBProxyDevice::open_shell_v2_service owns the TcpStream (drop closes socket->EOF), symmetric. S5: sim shell-v2 frame producer (post_open_writes) + end-to-end streaming/mid-drop/split regressions. S6: selftest automated shell_v2_stdin cat round-trip + interactive PTY-HUP process-group case. S7: documented PTY-HUP verification (MTK 8676 operator gate). All quality gates green (352 default + 190 usb tests, clippy default+usb, fmt).

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `d19d252` | (see git log) |
| `0f14e92` | (see git log) |
| `eda01cc` | (see git log) |
| `4bc153e` | (see git log) |
| `a3bd4e2` | (see git log) |
| `227f049` | (see git log) |
| `b41aa75` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 33: Fix USB reader timeout dropping raced-completion bytes (shell-v2 PTY desync)

**Date**: 2026-06-27
**Task**: Fix USB reader timeout dropping raced-completion bytes (shell-v2 PTY desync)
**Branch**: `main`

### Summary

Root-caused an external bug report: a connection-fatal ConversionError desync under sustained shell-v2 PTY output. The USB reader's per-transfer timeout cancels the in-flight bulk-IN transfer and forces Cancelled status, but a transfer completing in the same instant is drained with real bytes intact; read_into_buffer ran the status->error map before reading actual_len, dropping those bytes -> offset shift -> bad command word -> whole PersistentUsbConnection torn down. Fix: classify completions on (status, byte_count) together via a new pure classify_read_completion (salvage bytes whenever present, ReadTimeout only when genuinely empty), removed the subsumed map_transfer_status, added 6 hardware-free unit tests, tightened framed_read.rs feed-layer invariant doc. Confirmed sim cannot reach this layer (it plugs in above the nusb cancel/drain race). Deferred the larger select!-without-cancel reader refactor as a follow-up. All 355 tests + clippy -D warnings green.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `a121461` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 34: Feasibility study: cancel-safe per-chunk reader select (NO-GO, design reserved)

**Date**: 2026-06-30
**Task**: Feasibility study: cancel-safe per-chunk reader select (NO-GO, design reserved)
**Branch**: `main`

### Summary

Evaluated whether to do the root-cause refactor deferred by the prior salvage fix: have the USB reader select! a cancel-safe per-chunk read primitive vs control/death, so a bulk transfer is never cancelled merely to poll. Ran two parallel trellis-research strands (nusb source cancel-safety; in-repo design/risk), synthesized go/no-go. Verdict NO-GO: the design is feasible and elegant (nusb next_complete is source-confirmed cancel-safe; minimal 2-method trait delta; death observation becomes strictly more prompt; does NOT reproduce the reverted whole-frame-select WRTE-corruption because the cancellation unit drops to a single transfer) — but it is a cleanup not a bug fix (race already correct post-salvage), with negligible/non-load-bearing benefit and real two-transport contract risk plus a next_complete-panics-on-empty footgun. Took the NO-GO branch: documented the current timeout-poll+salvage design as intentionally-correct in the wire-protocol contract spec, with a gotcha warning future contributors off the reverted naive select and reserving the validated chunk-select as an upgrade path gated on a real driver. No production code changed.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `229df75` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 35: Fix multi-device forward via device-pinned host-serial scoping (DeviceSelector)

**Date**: 2026-07-01
**Task**: Fix multi-device forward via device-pinned host-serial scoping (DeviceSelector)
**Branch**: `main`

### Summary

External bug: ADBProxyDevice::forward failed 'more than one device/emulator' with >=2 devices — it sent host:transport:<serial> then a bare host:forward, but host:forward is a HOST service the server does not bind to the selected transport (shell/sync work because they are device services). Root-cause fix at the contract layer: new DeviceSelector{TransportId|Serial|Any} as the single source of transport_id->serial->any precedence, rendering transport_switch_command() (device services) and host_prefix() (device-pinned host services). Moved Forward/KillForward/KillForwardAll from ADBLocalCommand to ADBHostCommand with named {selector,local,remote} fields, emitting host-serial:<s>:forward:<l>;<r>. Kept killforward-all GLOBAL (AOSP is process-global; verified via research subagent against android-14 source). reverse unchanged (genuine device service). Wire-string unit tests are the regression net (NOT sim — own frontend tolerates the hack, which is why the bug escaped). selftest uses asymmetric ports + scoped killforward. clippy/fmt/365 lib+22 CLI tests green. Also PROVEN a separate CLI arg-order swap bug (local_commands.rs:35 passes local/remote reversed vs reverse arm) — filed as follow-up task, not fixed here (one bug/one commit). Spec + memory (forward-is-device-pinned-host-service) recorded.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `b44bf4a` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 36: Fix CLI forward arg-order swap (local/remote reversed to library)

**Date**: 2026-07-01
**Task**: Fix CLI forward arg-order swap (local/remote reversed to library)
**Branch**: `main`

### Summary

Follow-up to the multi-device forward fix. CLI forward handler passed (local, remote) into the remote-first library forward(remote, local), so 'adb forward tcp:1111 tcp:2222' emitted host:forward:tcp:2222;tcp:1111 (ports swapped); adjacent reverse arm was already correct, only forward slipped (shipped 19aa24a), single-port selftest masked it. Investigated both consumers before deciding: research subagent confirmed native adb CLI is 'forward LOCAL REMOTE' (local-first) / 'reverse REMOTE LOCAL' (remote-first), mirrors sharing one wire mapping — adboost_cli clap defs already match. Read xdb source (/Volumes/MagicWork/.../xpeng-debug-bridge): xdb calls library forward() DIRECTLY and correctly (forward_tcp -> forward(remote,local), with a comment) and pins adboost by git rev — so flipping the library signature would silently break xdb. Chose option 1 (fix at the call site, library API unchanged). Routed the arm through a pure forward_library_args(local,remote)->(remote,local) helper so the CLI-library order cross is named, documented, and locked by a unit test with asymmetric ports (handler needs a live device, not unit-testable inline). clippy pedantic + fmt + 23 CLI tests + library forward/reverse tests all green. trellis-implement + trellis-check subagents both PASS; check independently validated the helper is warranted (not over-engineered) given a positional swap already shipped.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `6fcb0b7` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 37: Support vsock socket-spec in forward

**Date**: 2026-07-09
**Task**: Support vsock socket-spec in forward
**Branch**: `main`

### Summary

Added typed LocalSocketSpec/RemoteSocketSpec enums to the forward system, enabling vsock remote endpoints (adb forward tcp:X vsock:CID:PORT). Extensible design — future specs need only a new enum variant + parse arm.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `8550a7b` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 38: 支持裸 host:get-state/get-serialno（transport-any 单设备数据查询）

**Date**: 2026-08-06
**Task**: 支持裸 host:get-state/get-serialno（transport-any 单设备数据查询）
**Branch**: `main`

### Summary

修复外部下游 xdb 报告：adboost server 前端缺失裸 host:get-state/get-serialno（AOSP adb root/unroot 前调 adb_get_state()），落入兜底 arm 返回 unknown host service 中止整流程。将两个裸单设备数据查询并入 host_data_query_payload（单一数据查询分派点），复用 resolve_single_by_kind(None) transport-any 语义，与 host-serial:<serial>:<sub> 字节一致，0/多设备回 AOSP 措辞。明确不含 host:get-devpath（DeviceEntry 无 devpath 字段，诚实能力原则）。新增 round-trip 单测（single/zero/multi + 裸vs带前缀字节一致）与官方 adb parity case（裸 adb -P get-state）端到端锁定回归。单测 397 全绿、clippy clean；8155 实机 selftest 14/17 通过，parity.official_adb_get_state 通过；root_unroot_cycle 唯一失败系本机 Lemon 官方 adb server 抢占 USB claim 竞态（环境问题）。下游 xdb 需 bump rev 到 9498fea8242c4dce9983064dd1b9a607b478aba2 重编后做真机验证。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `9498fea` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 39: adb-server frontend: host:track-devices-l + unknown-service WARN logging

**Date**: 2026-09-03
**Task**: adb-server frontend: host:track-devices-l + unknown-service WARN logging
**Branch**: `main`

### Summary

Implemented the xdb-reported AS blank-device-list fix: host:track-devices-l as a DeviceListFormat-parameterized streaming service sharing the single format_devices renderer (stream-vs-one-shot byte parity locked), and a warn_unsupported_service funnel covering all five unknown-service FAIL paths (FAIL wording unchanged, original request string threaded through pinned dispatchers). Added 7 frontend unit tests, a raw-smartsocket protocol_cases selftest module (automated track_devices_family + interactive track_devices_l_hotplug), live-verified against real hardware (old build FAIL reproduced on 5037, new build OKAY+long format on 5038, full selftest green incl. hotplug streaming). Spec: server-host-protocol.md gained the device-list family contract + unknown-service WARN funnel sections; P1 proto variants documented as must-ship-with-feature-flag.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `46df633` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 40: AS acceptance round: host-features + exec:/JDWP bridging

**Date**: 2026-09-03
**Task**: AS acceptance round: host-features + exec:/JDWP bridging
**Branch**: `main`

### Summary

Acceptance-driven round: ran adboost itself on :5037 with the USB claim against real Android Studio. Device visibility confirmed (track-devices-l + host-features from 46df633 working). AS install then failed on the exec: service gap (NOT track-jdwp) — WARN funnel caught the full picture: exec:×15 (deployer agent + streaming install-write), track-jdwp×534 (debug monitor). AOSP-verified all are adbd-side services → bridged verbatim: exec: (two-axis shell_v2 gate) and the JDWP family (track-jdwp/track-app/jdwp/jdwp:<pid>, no gate). Also implemented host:host-features (adblib's FIRST query, server-level, zero-device-safe) and aligned bare host:features to AOSP per-transport semantics; corrected client-side HostFeatures docs (per-transport form, do not re-render). Full acceptance passed: AS sees device, app install succeeded, zero unknown-service WARNs post-fix (only documented-benign re-enumeration noise). 9 new unit tests + 4 parity cases (host-features/features/exec-out/jdwp); spec gained the features-duality section and the exec:/JDWP section.

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `2695f12` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete
