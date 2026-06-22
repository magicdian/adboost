# Frame-atomic write timeout (Scheme B): fix reverse_iperf3 teardown regression

## Goal

Commit `1aac71c` (#2 writer-loop teardown) made **any** write error fatal — it
poisons the write half and breaks `writer_loop`. That broke the saturating
`through_server.reverse_iperf3` selftest (`iperf3: error - control socket has
closed unexpectedly`): a normal backpressure write timeout (the OUT buffer is
momentarily full, **0 bytes of the frame actually committed**, fully recoverable)
is now misclassified as a fatal truncation and tears the whole connection down.

Adopt **Scheme B (frame-atomic writes)**: the per-write timeout gates only the
*start* of a frame. Once any byte of a frame has been committed to the transport,
finish the frame under a looser completion deadline. Only a genuine partial write
(frame started but not finished) or a real IO error is fatal. This makes
`writer_loop`'s "timeout ⇒ keep looping" safe again **while preserving** the #2
desync protection (a truly truncated frame still poisons + tears down).

## What I already know (verified)

- **Regression mechanism**: USB-backed device; iperf3 reverse data plane rides the
  backend persistent connection's `writer_loop`. USB write timeout →
  `transfer_with_timeout` Cancelled → `map_transfer_status` → currently
  `ReadTimeout` → my new `writer_loop` treats `Err(_)` as fatal → `break` → connection
  torn down → iperf3 control socket closes. `reverse_echo` (low traffic) passes;
  only the saturating `reverse_iperf3` trips it. Confirmed regression, not a device issue.
- **Cancel-safety fact that makes Scheme B clean**: a single `AsyncWrite::poll_write`
  is cancel-safe — it returns `Ready(Ok(n))` (n bytes committed) or `Pending` (0 bytes
  moved). `write_all` is NOT cancel-safe only because it loops across polls and stores
  progress in the future. So writing a frame as a manual loop over single `write()`
  calls lets us know exactly whether the frame has started.
- **USB**: each `max_packet_size` chunk is an atomic nusb transfer (Cancelled = whole
  transfer voided, no partial). "Frame started" = first chunk transfer succeeded. Same
  model; but `map_transfer_status` blanket-maps `Cancelled → ReadTimeout` and is shared
  with the read path — the write path must NOT reuse that mapping verbatim.
- **Error enum**: only `RustADBError::ReadTimeout` exists (`error.rs:102`, non-feature-gated).
  `adb_cli_error.rs` classifies it via an **exhaustive match** (no `_` arm) — a new
  variant requires a classification arm there (compiler-enforced).
- **Spec constraints** (`error-handling.md`): on read-deadline elapse, transports MUST
  return the transport-neutral `ReadTimeout`; never reintroduce string-matching on
  timeout text, never re-gate the timeout concept on a transport feature. The symmetric
  write-side signal should follow the same discipline.

## The model (three outcomes, each correct)

| Situation | Bytes on wire | Result | writer_loop |
|-----------|---------------|--------|-------------|
| Timeout before the frame's first byte is committed (backpressure) | 0 | recoverable `WriteTimeout` | **continue** |
| Frame started, then IO error or completion-deadline exceeded | partial (truncated) | **fatal**: poison write half | **break** + teardown |
| Frame fully written | all | `Ok(())` | continue |

This is the exact mirror of the read side's "`ReadTimeout` only at a frame boundary".

## Requirements

- A write timeout that fires with **zero bytes of the current frame committed** MUST be
  recoverable: return a neutral `WriteTimeout`, do NOT poison the transport, and
  `writer_loop` MUST keep looping (the next frame writes cleanly).
- Once any byte of a frame is committed, the frame MUST be driven to completion under a
  (looser) completion deadline; the start-gate timeout MUST NOT cancel mid-frame.
- A genuine partial write (frame started but not finished: IO error, or completion
  deadline exceeded) MUST remain fatal — poison the write half and tear the connection
  down (preserves #2 desync protection).
- Apply symmetrically to TCP and USB write paths; `writer_loop` classifies
  `WriteTimeout ⇒ continue`, everything else ⇒ `break` (mirror of `classify_read_result`).
- The USB write path must not inherit the read path's `Cancelled → ReadTimeout` mapping;
  a write-start timeout maps to `WriteTimeout`, a mid-frame timeout to the fatal path.
- Honor `error-handling.md`: structured (no string matching), not feature-gated; add the
  exhaustive CLI classification arm for any new variant.
- `#![forbid(unsafe_code)]`, clippy pedantic, MSRV 1.88.0, fmt — all stay green.

## Acceptance Criteria

- [ ] A transport-level test: a write that cannot make progress at the START of a frame
      yields the recoverable `WriteTimeout` and does NOT poison the write half (a
      subsequent write on a now-drained peer succeeds).
- [ ] A transport-level test: a write that fails AFTER the frame has started (mid-frame)
      poisons the write half — a subsequent write fails fast with `NotConnected`
      (preserves the existing `write_timeout_poisons_write_half` intent, retargeted).
- [ ] A `writer_loop`/persistent test (or a focused unit on the classify step): a
      recoverable `WriteTimeout` keeps the writer running; a fatal write error stops it.
- [ ] `through_server.reverse_iperf3` passes again on real hardware (manual selftest;
      this is the real-world regression signal — note it cannot run in CI).
- [ ] Existing tests still pass, including the Class A read cancel-safety locks.
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings` (default + `--features usb`),
      `cargo test` green.

## Definition of Done

- TCP + USB write paths frame-atomic; `writer_loop` safe on recoverable write timeout.
- #2 desync protection preserved (mid-frame truncation still fatal).
- Tests added; quality gate green; one coherent commit.
- Doc comments explain the start-gate vs completion-deadline split (mirror read side).

## Decision (ADR-lite)

**Context**: Scheme B needs (1) a bound on finishing an already-started frame so a
silently-stalled peer can't block the writer forever, and (2) a representation for
the recoverable "couldn't start the frame" timeout that `writer_loop` can match.

**Decision**:
1. **Two-tier deadline.** The configured per-write timeout (default 2s) gates only
   the *start* of a frame (zero bytes committed) → recoverable `WriteTimeout`. Once
   any byte is committed, the frame must finish within a **looser fixed completion
   deadline of 10s**; exceeding it (or any IO error) is fatal (truncation → poison +
   teardown). 10s ≫ any real drain on local adbd/USB, so normal backpressure never
   trips it; only a genuinely wedged peer does.
2. **New `RustADBError::WriteTimeout` variant** (non-feature-gated, symmetric to
   `ReadTimeout`). `writer_loop` matches it explicitly for `continue`; everything
   else is fatal. Add the exhaustive classification arm in `adb_cli_error.rs`.

**Consequences**:
- `write_timeout` semantics change from "whole-frame budget" to "frame-start budget";
  small frames are unaffected, a large frame under backpressure may now take up to the
  10s completion deadline instead of failing at 2s. This is the correct mirror of the
  read side ("timeout only at a frame boundary") and is documented in code + commit.
- New enum variant is a minor public-API addition; the exhaustive CLI match makes the
  classification a compile-time obligation (no silent `_`).
- Adds a `WRITE_COMPLETION_TIMEOUT` constant (10s) alongside `DEFAULT_WRITE_TIMEOUT`.

## Out of Scope

- The read path (Class A) — unchanged; this is the write-side symmetry.
- Removing/altering the 1s reader timeout or the fatal-vs-recoverable READ classification.
- Reworking `map_transfer_status` for the read path (only the write path stops reusing it).

## Technical Notes

- Files: `tcp/tcp_transport.rs` (`write_all_timeout` ~175-194, `write_message_with_timeout`
  ~314-350), `usb/usb_transport.rs` (`write_bulk_data` ~140-173, `map_transfer_status`
  ~283), `usb/persistent.rs` (`writer_loop` ~1236-1268, `classify_read_result` ~1194 as the
  template), `error.rs` (~102), `adb_cli/src/models/adb_cli_error.rs` (exhaustive arm).
- Regression introduced in `1aac71c`; original passing baseline `f5ef847`
  (iperf3-validated reverse).
- Relates to memory [[tcp-async-path-missing-usb-guarantees]] — same "make the timeout
  invariant explicit and symmetric across transports" theme.
