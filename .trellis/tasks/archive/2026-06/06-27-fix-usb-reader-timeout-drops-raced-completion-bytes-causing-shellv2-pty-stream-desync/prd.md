# Fix: USB reader timeout drops raced-completion bytes → shell-v2 PTY stream desync

## Goal

On the USB direct path, a `shell,v2,...,pty:` session streaming sustained
multi-line output (e.g. `ping -c 5 -W 2`) intermittently makes the
connection-wide reader task die with `Conversion error`, tearing down the entire
shared `PersistentUsbConnection` (every multiplexed session dies). Root cause:
the per-transfer read timeout can race a real bulk-IN completion and **discard
the bytes that completion carried** before they reach `FrameReadBuffer`, so the
next read resumes one chunk late and parses a header out of mid-payload bytes →
`ConversionError` → fatal reader exit. Fix the read feed path to be **lossless on
timeout**: salvage any bytes a cancelled/timed-out completion carries before
treating it as a `ReadTimeout`.

## What I already know (from source trace @ 0e54c2a)

- The reader loop reads each frame with a **1s** per-transfer timeout:
  `persistent.rs:1462` `read_message_with_timeout(Duration::from_secs(1))`.
- `transfer_with_timeout` (`usb_transport.rs:304`): on `tokio::time::timeout`
  elapsing, it calls `endpoint.cancel_all()`, drains the now-cancelled transfer
  via `next_complete().await`, then **forces** `status: Err(TransferError::Cancelled)`
  while keeping `..completion` (i.e. the drained completion's `buffer` /
  `actual_len` are preserved in the struct).
- `read_into_buffer` (`usb_transport.rs:536`): calls
  `map_transfer_status(completion.status)?` **before** reading
  `completion.buffer[..actual_len]`. On the forced `Cancelled`, `map_transfer_status`
  returns `Err(ReadTimeout)` and the `?` **returns early** — so
  `buffer.push(received)` never runs. Any bytes the drained completion carried
  are dropped.
- The race: when the 1s timer fires at the same instant a bulk-IN completion
  lands with `actual_len > 0`, those real wire bytes are lost. `FrameReadBuffer`
  (`framed_read.rs`) is itself sound — its cancel-safety invariant
  (`framed_read.rs:13-24`) holds *only if* the cancelled transfer carried zero
  bytes; the feed layer violates that precondition.
- Symptom fingerprint matches exactly: `ConversionError` (offset shift from lost
  bytes), **zero** `InvalidIntegrity` (no bit-flip / aligned-frame corruption).
- Why only PTY `ping -W 2`: it emits output ~once/second, aligning with the
  reader's 1s timeout → repeatedly completes at the timeout boundary, maximizing
  the race window. Sub-ms bursts (getprop/echo) never reach the boundary.
- `delayed_ack=true` is a red herring (report flagged it as a suspect but could
  not prove it); the bug is independent of windowing.

## Bug provenance

- `read_into_buffer` / `FrameReadBuffer` introduced in `1aac71c`
  ("make framed transport reads/writes cancel-safe via shared FrameReadBuffer").
  That commit fixed the TCP `read_exact`-drop desync and made the *buffer*
  lossless, but left the USB *feed* path's timeout-salvage gap: it returns on
  `map_transfer_status(...)?` before pushing the drained completion's bytes.
- Recurring bug class: [[tcp-async-path-missing-usb-guarantees]] mirrored — a
  robustness property assumed everywhere is missing at one transport seam.

## Requirements (evolving)

- USB read path MUST push any bytes a completion carries (`actual_len > 0`) into
  `FrameReadBuffer` **before** deciding the read is a timeout — losslessly.
- A genuinely empty timed-out completion (`actual_len == 0`, forced `Cancelled`)
  MUST still surface as `RustADBError::ReadTimeout` (the reader-loop idle signal —
  contract in `adb_message_transport.rs:35-52`).
- A real transfer error (Disconnected, etc.) on an empty completion MUST still be
  fatal/propagated unchanged.
- The salvage decision MUST be a pure function of `(status, actual_len)` so it is
  unit-testable without USB hardware.
- The `framed_read.rs` module invariant doc MUST be tightened to state that the
  feed layer is required to salvage a timed-out completion's bytes.

## Acceptance Criteria (evolving)

- [ ] Salvage decision extracted into a pure function (input: transfer `status` +
      received-byte slice/len; output: salvage-bytes / `Ok(())` / `ReadTimeout` /
      propagate-error) so it is unit-testable without `nusb` or hardware.
- [ ] Pure unit test: a completion with `actual_len > 0` + forced `Cancelled`
      status salvages the bytes and returns `Ok(())` (NOT `ReadTimeout`).
- [ ] Pure unit test: a completion with `actual_len == 0` + `Cancelled` returns
      `ReadTimeout`.
- [ ] Pure unit test: a non-timeout transfer error (e.g. `Disconnected`) on an
      empty completion still propagates as the transfer error (unchanged).
- [ ] `read_into_buffer` rewired to push salvaged bytes BEFORE the timeout
      decision, delegating the classification to the pure function.
- [ ] Existing read/timeout/cancel-safety tests still pass.
- [ ] `framed_read.rs` invariant doc updated to state the feed layer must salvage
      a timed-out completion's bytes.

## Regression-coverage decision (Q1)

The bug lives in `transfer_with_timeout` / `read_into_buffer` — **below** the
`ADBMessageTransport` frame interface where `SimulatedDevice` / `ChunkedTransport`
substitute. The sim's own honest-boundary doc (`sim/mod.rs:46-63`) states it does
NOT prove "the kernel, `nusb`, or `tokio` actually produces" the timeout/error
variants — it *emits* them at the frame layer. A sim regression therefore cannot
reach the nusb cancel/drain salvage race. Chosen coverage: **extract a pure
salvage function + exhaustive unit test** (the faithful, hardware-free coverage of
the actual defect). The real end-to-end race remains a manual hardware repro
(`ping -c100` PTY loop, per the bug report §7) — not automated.

## Definition of Done (team quality bar)

- Tests added (pure unit + sim regression).
- `cargo test` / lint / typecheck green.
- `framed_read.rs` doc updated to reflect the feed-layer salvage contract.
- One bug = one task = one commit ([[prefer-root-cause-fix-at-contract-layer]]).

## Out of Scope (explicit)

- **select-without-cancel reader refactor**: replacing the 1s timeout-poll with a
  `select!` over `endpoint.next_complete()` (nusb-documented cancel-safe) vs
  `control_rx.recv()`, so a bulk transfer is NEVER cancelled merely to poll
  control/death — eliminating the whole race class at the root. This is a larger
  architectural change (touches death-signal observation points, reader idle
  semantics) and should be evaluated as a separate follow-up task, not bundled
  into this P0 bug fix. Recorded here as a known future direction.
- Any change to TCP transport (its reader builds a fresh reader and is not
  subject to the nusb cancel/drain salvage path).
- delayed_ack / window accounting changes.

## Technical Notes

- Files: `adboost/src/message_devices/usb/usb_transport.rs`
  (`transfer_with_timeout:304`, `read_into_buffer:536`, `map_transfer_status:337`),
  `adboost/src/message_devices/framed_read.rs` (invariant doc),
  `adboost/src/message_devices/usb/persistent.rs` (reader loop 1s timeout:1462 —
  context only).
- External bug report: `/private/tmp/adboost-bug-report-shellv2-pty-conversion-error.md`.
- Related memories: [[tcp-async-path-missing-usb-guarantees]],
  [[prefer-root-cause-fix-at-contract-layer]], [[sim-harness-regression-net]].

## Decision (ADR-lite)

**Context**: The connection-fatal `ConversionError` desync is caused by the USB
read feed path dropping bytes when the 1s per-transfer timeout races a real
bulk-IN completion. `FrameReadBuffer` is sound; the gap is at the feed layer,
which returns on `map_transfer_status(...)?` before pushing the drained
completion's bytes.

**Decision**: Make the feed path lossless on timeout — salvage any bytes a
cancelled/timed-out completion carries before treating it as `ReadTimeout`, via a
pure, unit-testable classification function. Tighten the `framed_read.rs`
invariant doc to state the feed-layer salvage contract. Do NOT bundle the larger
select-without-cancel reader refactor (recorded as Out of Scope / future
direction).

**Consequences**: Eliminates the lost-bytes desync with a minimal,
contract-restoring change (one bug = one commit). The 1s timeout-poll architecture
remains (salvage is the safety belt); the race *class* is only fully eliminated by
the separate future refactor. Sim cannot cover this layer, so regression is a pure
unit test + a documented manual hardware repro.

## Open Questions

- (none — converged)
</content>
</invoke>
