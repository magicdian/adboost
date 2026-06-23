# Workspace Index - magicdian

> Journal tracking for AI development sessions.

---

## Current Status

<!-- @@@auto:current-status -->
- **Active File**: `journal-1.md`
- **Total Sessions**: 26
- **Last Active**: 2026-06-23
<!-- @@@/auto:current-status -->

---

## Active Documents

<!-- @@@auto:active-documents -->
| File | Lines | Status |
|------|-------|--------|
| `journal-1.md` | ~1097 | Active |
<!-- @@@/auto:active-documents -->

---

## Session History

<!-- @@@auto:session-history -->
| # | Date | Title | Commits | Branch |
|---|------|-------|---------|--------|
| 26 | 2026-06-23 | host-usb/host-local + transport-usb/local for adb -d/-e | `bbc2b3e` | `feat/host-usb-local-transport-kind` |
| 25 | 2026-06-23 | 断开自动释放 forward/reverse 规则（OnDisconnect 策略 + ForwardHandle） | `82006cc` | `main` |
| 24 | 2026-06-22 | Transport cancel-safety bug class: shared FrameReadBuffer + frame-atomic write timeout + hardening | `1aac71c`, `23c2078`, `5bd58ae`, `f45e91d`, `584dd75`, `ea88205`, `bfcd337` | `main` |
| 23 | 2026-06-22 | TcpTransport split read/write halves — fix interactive shell ~2s lag | `1e28628` | `fix/tcp-transport-split-read-write-halves` |
| 22 | 2026-06-22 | SEG A nodelay miss — client-facing frontend sockets (TCP shell lag follow-up) | `fd5e624` | `main` |
| 21 | 2026-06-22 | Per-device capability negotiation (bug 2 of TCP shell report) | `67cc53e` | `main` |
| 20 | 2026-06-22 | Fix TCP_NODELAY on TcpTransport connect (bug 1 of TCP shell report) | `e90ab60` | `main` |
| 19 | 2026-06-22 | Expose TCP connection building blocks for external backends + fix two latent TCP-path bugs | `a3b1a91`, `a80dfd0`, `4951301` | `main` |
| 18 | 2026-06-15 | Rename library crate adb_client -> adboost (main line) | `0a55c91` | `main` |
| 17 | 2026-06-15 | tcpip mainline parity (PR1-5 + PR4a/b) | `c6447d7` | `feat/tcpip-mainline-parity` |
| 16 | 2026-06-15 | Fix reboot-recovery selftest + through-server shell exit code | `c7a09d1` | `main` |
| 15 | 2026-06-15 | Fix tport:any error wording for multi-device (no -s) | `087ee85` | `main` |
| 14 | 2026-06-13 | Export composable usb::ReverseEngine for external DeviceBackend impls | `3e96e47`, `ec22bd2`, `c19e7c6` | `main` |
| 13 | 2026-06-13 | adboost server P1-P4 (forward/sync/shell-v2/reverse) + interactive self-test harness | `866dac4`, `f5ef847` | `main` |
| 12 | 2026-06-12 | adboost ADB server capability + CLI server start/kill daemon | `6ebdfec`, `9b23064`, `68a80c1`, `0b24d8e`, `00d72b2`, `5efe2f6`, `0c5a86c`, `65c9736` | `feat/adboost-server-capability` |
| 11 | 2026-06-12 | adboost_cli rebrand + async migration + persistent USB exerciser (subtask B) — real-device closed-loop verified | `19aa24a` | `main` |
| 10 | 2026-06-12 | Library log->tracing migration (subtask A): emit-only, per-session local_id spans, RUST_LOG activation | `e4ed77d` | `main` |
| 9 | 2026-06-12 | Bug #3 TRUE root cause: CNXN banner trailing NUL corrupted last feature (delayed_ack) — device-verified fix | `a0e39da` | `main` |
| 8 | 2026-06-12 | Audit magic-only decision + harden USB receive path (data_length bound, reader fault-tolerance, bug #3 OPEN-rejection) | `6fec37e` | `main` |
| 7 | 2026-06-12 | Fix #2: magic-only message integrity (skip vestigial data_check at skip-checksum version) | `09ca21e` | `main` |
| 6 | 2026-06-12 | Fix delayed_ack/CNXN version contradiction (Android 16 USB hang) | `46d674f` | `main` |
| 5 | 2026-06-08 | Relicense fork to Apache-2.0 with upstream MIT attribution | `64d7186` | `main` |
| 4 | 2026-06-05 | persistent.rs server capabilities: 6 Asks (delayed_ack, device-OPEN, raw channel, SYNC mux, shell-v2, honest banner) | `8e91437`, `c55edad` | `feat/persistent-server-capabilities` |
| 3 | 2026-06-03 | Migrate USB transport from rusb to nusb | `1af81a5`, `3336689` | `main` |
| 2 | 2026-06-02 | Bootstrap backend coding guidelines | `b074d8e` | `main` |
| 1 | 2026-06-02 | Import xdb USB extensions patch into fork | `8b24f89`, `0af5888` | `main` |
<!-- @@@/auto:session-history -->

---

## Notes

- Sessions are appended to journal files
- New journal file created when current exceeds 2000 lines
- Use `add_session.py` to record sessions