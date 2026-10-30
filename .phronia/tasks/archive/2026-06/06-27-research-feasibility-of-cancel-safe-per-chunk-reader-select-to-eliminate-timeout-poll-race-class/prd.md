# Research: feasibility of a cancel-safe per-chunk reader `select!` to eliminate the timeout-poll race class

## Type

**Research-only.** Produces a design feasibility document under `research/`. NO
implementation code is written in this task. The output is a go / no-go decision
plus, if go, a design sketch that a future implementation task would consume.

## Goal

The USB persistent reader loop polls with a 1s per-transfer timeout purely to
return to its control/death observation point between frames. That timeout
cancels idle bulk-IN transfers, which created the (now-fixed) raced-bytes desync
(task `06-27-fix-usb-reader-timeout-drops-raced-completion-bytes`). The salvage
fix made the race **harmless** (no bytes lost, no desync) but did not make it
**impossible**. The hypothesized root-cause refactor: have the reader `select!`
over a *cancel-safe* "read one chunk" primitive vs `control_rx.recv()`, so a bulk
transfer is NEVER cancelled merely to poll — eliminating the race class at the
root and removing the 1s idle cancel/resubmit churn.

This task determines whether that refactor is **feasible, safe, and worth it** —
explicitly accounting for the fact that a *naive* version of it was already tried
and reverted (see Constraints).

## Why this is research, not implementation

The change would touch the USB+TCP shared `ADBMessageTransport` contract and the
death-signal observation model — high blast radius. A naive prior attempt
introduced a production bug. Before committing engineering effort we need
evidence that a *correct* design exists and that its benefit justifies the risk.

## Hard constraints / prior art (MUST be honored by any proposed design)

- **The naive `select!(read_whole_frame, control_rx)` was already tried and
  reverted.** `persistent.rs:1254-1266` documents it: a Register/Unregister
  arriving mid-read cancelled and **corrupted a large in-flight WRTE** (one of two
  concurrent device→host bulk streams silently stalled at 0 bytes). A multi-byte
  ADB frame read (header + `data_length` payload spanning many bulk transfers) is
  NOT cancel-safe. Any proposed design that re-introduces whole-frame cancellation
  is an automatic NO.
- The salvage fix already makes the existing design **correct**. This refactor is
  therefore NOT a bug fix — it is a race-class elimination + minor efficiency gain.
  The bar for "worth it" is correspondingly high (project philosophy:
  [[user-maintainer-profile]] standard-defaults + opt-in; [[prefer-root-cause-fix-at-contract-layer]]
  one-bug-one-commit, no speculative refactors).
- The reader currently observes the death edge ONLY at the idle timeout boundary
  (`persistent.rs:1291`), and the comment argues that is "the only place it is
  safe to observe the death edge from the read path" because a non-cancel-safe
  in-flight frame read has just completed/timed out. Any new design must preserve
  an equally-safe death-observation point.
- TCP transport shares the same `ADBMessageTransport` trait and the same reader
  loop. A trait change is a two-transport change.

## Research questions (the deliverable answers these)

1. Does `nusb` 0.2.x actually expose a cancel-safe "await the next chunk
   completion WITHOUT cancelling it" primitive? `endpoint.next_complete()` is
   documented cancel-safe — can the reader hold a submitted transfer across a
   `select!` and, when `control_rx` fires instead, return to await the SAME
   pending transfer next iteration (no cancel, no resubmit)? Confirm against nusb
   docs/source the queue-model semantics (`submit` once, `next_complete()` may be
   awaited repeatedly; what happens to a still-pending transfer across selects).
2. Can the chunk-level select be made cancel-safe where the whole-frame select was
   NOT? i.e. is the unit-of-cancellation a single in-flight bulk transfer (whose
   bytes the `FrameReadBuffer` already retains losslessly) rather than a
   multi-transfer frame? Does pushing chunk salvage into the buffer (already done)
   make a chunk-boundary select lossless even though a frame-boundary select was
   not?
3. What is the minimal `ADBMessageTransport` surface change? Sketch the trait
   method(s) needed (e.g. a `poll`-style or future-returning "read available
   chunk" that the reader can `select!`). Does it compose with the TCP transport
   (which builds a fresh reader on TLS upgrade) without regressions?
4. Where does the death edge get observed in the new model, and is it provably as
   safe as today's idle-boundary observation? (writer-died → reader must exit
   promptly to release the claim — currently `closed.is_dead()` at the timeout.)
5. Quantify the benefit: (a) eliminates the race class entirely vs merely
   harmless; (b) removes 1/sec idle cancel+resubmit churn — is that measurable or
   negligible? (c) reduces control-apply latency from "≤ one frame" to "immediate"
   — is there any real consumer that needs sub-frame control latency?
6. Enumerate the NEW risks: re-introducing WRTE corruption, half-open/death
   detection regressions, TCP-path divergence, increased reader complexity,
   testability (can the sim/ChunkedTransport cover the new select?).

## Acceptance Criteria (research deliverable)

- [ ] `research/cancel-safe-reader-select-feasibility.md` written, answering all 6
      research questions with citations (nusb docs/source, AOSP transport model
      if relevant, this repo's reader/transport source).
- [ ] Explicit **go / no-go recommendation** with reasoning tied to the
      risk/benefit bar above.
- [ ] If GO: a design sketch (trait delta, reader loop shape, death-observation
      point, test strategy) sufficient to seed a future implementation task — but
      NO implementation code in this task.
- [ ] If NO-GO: a recommendation to instead **document the current design as
      intentionally-correct** (salvage + frame-boundary drain-control), so the
      race class is not mistaken for an unfixed hazard by a future contributor.
- [ ] Honest treatment of the reverted prior attempt — the design must show WHY
      it does not reproduce the WRTE-corruption bug, or recommend no-go.

## Out of Scope

- Any implementation / code change (this is research-only).
- The already-shipped salvage fix (done, committed `a121461`).

## Technical Notes

- Reader loop + design rationale: `adboost/src/message_devices/usb/persistent.rs`
  (`reader_loop:1244`, `read_or_control:1443`, drain-control rationale `:1254-1266`,
  death observation `:1291`).
- Transport contract: `adboost/src/message_devices/adb_message_transport.rs`
  (the trait a change would touch).
- USB read primitive + the (now-fixed) salvage path:
  `adboost/src/message_devices/usb/usb_transport.rs`
  (`transfer_with_timeout:304`, `classify_read_completion`, `read_into_buffer`).
- Shared framing invariant: `adboost/src/message_devices/framed_read.rs`.
- Related memories: [[timeout-cancel-drops-raced-bytes]] (the bug this would
  structurally prevent), [[tcp-async-path-missing-usb-guarantees]] (two-transport
  contract care), [[sim-harness-regression-net]] (test-reachability of the layer),
  [[prefer-root-cause-fix-at-contract-layer]], [[user-maintainer-profile]].

## Research References

- [`research/SYNTHESIS-go-no-go.md`](research/SYNTHESIS-go-no-go.md) — **NO-GO now**: design is feasible & validated, but it's a cleanup not a bug fix; reserved upgrade path if a real driver appears.
- [`research/nusb-cancel-safe-chunk-primitive.md`](research/nusb-cancel-safe-chunk-primitive.md) — nusb `next_complete()` is source-confirmed cancel-safe (holds no state; pending transfer re-awaited intact); panics if `pending()==0` (load-bearing footgun).
- [`research/in-repo-reader-refactor-design-risk.md`](research/in-repo-reader-refactor-design-risk.md) — minimal 2-method trait delta (both transports already split); death obs becomes strictly safer; does NOT reproduce the reverted WRTE-corruption (cancellation unit drops to single-transfer); benefit negligible/non-load-bearing.

## Decision (ADR-lite)

**Context**: After the salvage fix (`a121461`) the timeout-poll race is harmless but
not impossible. Question: do the root-cause refactor (cancel-safe chunk `select!`) now?

**Decision**: **NO-GO now.** A correct, elegant design exists and is fully specified
(see synthesis), but it is a cleanup — not a bug fix — with negligible/non-load-bearing
benefit and real two-transport contract risk. Instead take the NO-GO branch: document
the current design (salvage + frame-boundary drain-control + asymmetric death
observation) as intentionally-correct so the now-harmless race is not re-opened, and
reserve the validated chunk-select design as the upgrade path for when a real driver
(e.g. a consumer needing sub-frame control latency) appears.

**Consequences**: No production code changes from this task. A small follow-up to
capture the "intentionally-correct" design rationale in spec/source is the concrete
next action (candidate for `trellis-update-spec`). The research is preserved so a
future GO can seed implementation directly.
</content>
