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
