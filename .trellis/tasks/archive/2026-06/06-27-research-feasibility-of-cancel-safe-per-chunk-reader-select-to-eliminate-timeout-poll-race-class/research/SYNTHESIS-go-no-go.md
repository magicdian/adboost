# Synthesis: go / no-go on the cancel-safe per-chunk reader `select!` refactor

- **Date**: 2026-06-27
- **Inputs**: [`nusb-cancel-safe-chunk-primitive.md`](nusb-cancel-safe-chunk-primitive.md) (external/nusb strand), [`in-repo-reader-refactor-design-risk.md`](in-repo-reader-refactor-design-risk.md) (in-repo design/risk strand)
- **Question**: should we now do the refactor where the reader `select!`s a cancel-safe "read one chunk" primitive vs `control_rx`/`closed`, eliminating the 1s timeout-poll that cancels idle transfers?

## Recommendation: **NO-GO now — but the design is validated and reserved.**

Not because the refactor is wrong or infeasible — both strands agree a **correct,
elegant design exists**. NO-GO because, measured against this project's bar
([[prefer-root-cause-fix-at-contract-layer]]: one-bug-one-commit, no speculative
refactors; [[user-maintainer-profile]]: standard-defaults + opt-in), the
**benefit does not justify the risk right now**, and there is no driver requiring it.

## What the research settled (so a future task does not re-litigate)

### Feasibility: YES, a correct design exists
- **nusb primitive is genuinely cancel-safe — confirmed against 0.2.3 SOURCE**, not
  just the doc: `next_complete()` is `poll_fn` holding no state; a submitted transfer
  lives in the backend `VecDeque` and is re-awaited intact after a dropped future;
  completion is latched in an `AtomicU8`, so no edge is lost. Holding ONE in-flight
  transfer across a `select!` is the documented "Optimized Streaming" pattern at N=1.
- **Minimal trait delta**: add `read_chunk` (cancel-safe, timeout-less, pushes into
  the transport-owned `FrameReadBuffer`) + `try_next_frame` (sans-io `try_parse`);
  demote `read_message_with_timeout` to a default layered over them. Both USB and TCP
  **already contain this exact split** — USB's `read_chunk` is today's
  `read_into_buffer` minus the timeout arm; TCP's is the timeout-less inner read. The
  `FrameReadBuffer` does NOT move (already on the transport). TLS-upgrade path
  untouched (it rebuilds a fresh reader before tasks spawn).
- **Death observation becomes strictly better**: add a third cancel-safe
  `() = closed.wait() => break` arm — the same pattern the writer loop already uses.
  Reaction to a single-sided death drops from "up to 1s" to "immediate", tightening
  the `DeviceBusy`-release window.

### Safety: it does NOT reproduce the reverted WRTE-corruption bug
The decisive distinction: the reverted naive attempt `select!`ed control against a
**whole-frame** read, so a control event cancelled a transfer *mid multi-transfer
frame* and discarded partial payload → desync. The chunk-select cancels (in fact,
merely abandons-and-re-awaits) at the **single-transfer** boundary, and every
received byte is already in the persistent `FrameReadBuffer` before the future
resolves. Cancellation unit drops from multi-transfer-frame (lossy) to
single-transfer-between-frames (lossless). The `framed_read.rs` byte-at-a-time test
is the pure-form proof.

## Why NO-GO anyway (the bar)

1. **It is not a bug fix.** The race is already *correct* post-salvage (shipped
   `a121461`). This refactor buys race-class *elimination* + a simpler loop — a
   cleanup, not a correctness fix.
2. **Benefit is negligible / non-load-bearing.** The 1 Hz idle cancel+resubmit churn
   is below any throughput-relevant rate (and only fires when idle, displacing no
   work). Control-apply latency improves from "≤ one frame" to "immediate", but
   **no real consumer needs sub-frame latency**: register-before-route correctness is
   enforced by `drain_control` running after each frame read, not by latency; Unregister
   is fire-and-forget; Subscribe has no ordering guarantee.
3. **The new risks are real and asymmetric.** A two-transport contract change (the
   exact [[tcp-async-path-missing-usb-guarantees]] bug class), new borrow/lock subtlety
   from holding a `&mut self` future across a `select!`, a mandatory (not optional)
   `closed.wait()` death arm whose omission re-opens the `DeviceBusy` leak, and the
   `pending()==0` panic footgun. Each is manageable, but together they are real risk
   budget spent on a non-bug.
4. **Regression-net coverage is partial.** The sim plugs in at the frame boundary; it
   *can* be extended to exercise a chunk-level select (not a hard blocker), but the
   single highest-value property — the kernel/nusb re-await-pending-across-select — is
   source-confirmed yet only *fully* verifiable on hardware. Spending risk on a non-bug
   whose core safety property the automated net cannot fully cover is exactly what the
   maintainer philosophy counsels against.

## The chosen higher-value action (NO-GO branch)

Per the PRD's NO-GO branch: **document the current design as intentionally-correct**
so a future contributor does not mistake the now-harmless race for an unfixed hazard
and re-open it (or re-attempt the reverted naive select). Specifically capture, in
the wire-protocol contract spec and/or the reader-loop source comment:
- salvage (lossless feed) + frame-boundary `drain_control` is a *coherent, deliberate*
  correct design, not a patch over a hazard;
- the naive `select!(whole_frame, control)` was tried and reverted (WRTE corruption);
- the asymmetric `is_dead()` (read path) vs `wait()` (writer path) death observation is
  deliberate and *why*;
- this validated chunk-select design is the reserved upgrade path **if a real driver
  appears** (e.g. a consumer that genuinely needs sub-frame control latency).

## The reserved GO trigger (when to revisit)

Flip to GO only when a concrete driver materializes — most plausibly: a consumer that
demonstrably needs sub-frame control-apply latency, or a measured problem with the 1 Hz
idle wake. At that point this document + the two strands are sufficient to seed the
implementation task directly (trait delta, death arm, panic guard, two-transport test
plan all specified).
