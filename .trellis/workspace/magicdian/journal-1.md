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
