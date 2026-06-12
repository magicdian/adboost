# Workspace Index - magicdian

> Journal tracking for AI development sessions.

---

## Current Status

<!-- @@@auto:current-status -->
- **Active File**: `journal-1.md`
- **Total Sessions**: 10
- **Last Active**: 2026-06-12
<!-- @@@/auto:current-status -->

---

## Active Documents

<!-- @@@auto:active-documents -->
| File | Lines | Status |
|------|-------|--------|
| `journal-1.md` | ~340 | Active |
<!-- @@@/auto:active-documents -->

---

## Session History

<!-- @@@auto:session-history -->
| # | Date | Title | Commits | Branch |
|---|------|-------|---------|--------|
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