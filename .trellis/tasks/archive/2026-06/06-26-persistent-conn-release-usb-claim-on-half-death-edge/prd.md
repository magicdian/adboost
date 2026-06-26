# PersistentConnection: release USB claim on the half-death edge

## Goal

When a `PersistentConnection`'s reader **or** writer task dies (fatal break), the
OTHER half must also stop promptly, so **both** `USBTransport` clones drop and the
nusb `Interface` claim is released **on the death edge** — independent of how many
external `Arc<PersistentConnection>` holders remain. Today the claim is only
released when the last `Arc` holder drops (refcount → 0), so a single-sided reader
death while the writer parks on `writer_rx.recv()` pins the USB interface claim
forever, and every subsequent `connect` on that device gets `DeviceBusy`
mis-reported as "another process already holds the ADB interface".

Root cause is a contract-layer split: the library already treats "either half died"
as "connection unusable" (`is_alive()` uses `&&`; `DeathSignal` fires on first
exit), but the resource release (drop the surviving half's transport clone) is
deferred to refcount-zero instead of bound to the same death edge.

## What I already know (source-verified, rev 70ab60d)

- `USBTransport` is `Clone` (`usb_transport.rs:88`); clones share
  `connection: Arc<Mutex<Connection>>` whose `Connection.interface: Option<Interface>`
  IS the claim. Claim releases only when the **last** clone drops.
- `persistent.rs:649-668`: reader/writer each `move` one clone into a `tokio::spawn`.
  Each closure fires `DeathSignal` after its loop returns
  (`reader_closed.fire()` / `writer_closed.fire()`).
- `writer_loop` (`:1475`) is `while let Some(frame) = writer_rx.recv().await`. The
  struct keeps a live `writer: WriterHandle` (an mpsc `Sender`), so `writer_rx`
  never closes while the connection is alive → writer **parks forever** when no
  writes happen. It only self-exits on a write error (`:1509`).
- `reader_loop` (`:1213`) reacts to a fatal read by `break` (`:1282`); a
  `ConversionError` is (correctly) fatal and unrecoverable (payload not yet drained).
  Its only non-frame checkpoint is the 1s idle `ReadTimeout` (`:1251`).
- Release exits today: `Drop` (`:2075`, only `abort()` site, needs refcount 0),
  `close(self)` (`:2045`, needs ownership), `shutdown(&self)` (`:2035`, flushes CLSE
  but does NOT abort the writer). Under shared `Arc`, all three are unreachable for a
  long-lived relay/proxy holder → claim pinned.
- `DeathSignal` (`:316`) is an `AtomicBool` + `Notify`: a never-lost one-way edge,
  already cloned into both tasks and the struct; `wait_closed`/`closed_signal` are
  built on it. Provenance of the split: spawn block introduced in `0977368`
  ("fix(server): make adb root/unroot reconnect handshake robust end-to-end").
- Transport-generic: the same code path serves USB and TCP (`PersistentTcpConnection`),
  so the fix benefits both with no transport-specific branching.
- Sim regression net exists (`usb/sim/`, `test-support` feature). `SimulatedDevice`
  is `Clone` sharing `Arc<Mutex<SimState>>`; `Scenario::with_death_after_reads(n)`
  makes the reader die after `n` idle reads (used by
  `reader_death_after_handshake_flips_is_alive_and_resolves_wait_closed`).

## Decision (ADR-lite)

**Context**: Need to release the OS claim on the death edge without requiring
external `Arc` holders to "know to let go". Two candidates:
- Report's Approach 1: cross-inject each task's `AbortHandle` into the other.
- Chosen: reuse the existing `DeathSignal` — each I/O loop also *watches* the signal
  and returns on the other half's death.

**Decision**: Chosen approach (DeathSignal-driven mutual teardown), because:
- No `AbortHandle` spawn-ordering chicken-and-egg (no `OnceLock`/reaper task).
- Reuses the already-correct never-lost edge (`DeathSignal`), same semantics as
  `wait_closed`/`closed_signal` — one mechanism, not two.
- Cancel-safe by construction: the writer races `closed.wait()` only at its idle
  `recv()` point (mpsc recv is cancel-safe; never mid-write); the reader checks
  `closed.is_dead()` only at its 1s idle-timeout boundary, never interrupting a
  non-cancel-safe in-flight frame read.
- Orthogonal to graceful `shutdown`/`close`/`Drop`: those fire `closed` only at/after
  teardown, so the new watch never pre-empts a graceful CLSE flush.

**Consequences**:
- Reader-dies-first (the reported bug): writer wakes from `recv()` **immediately**
  via `select!` → drops its clone → claim released at the death edge.
- Writer-dies-first: reader releases at its next idle `ReadTimeout` (≤ ~1s). This is
  a complete fix for the reported "permanent leak" (bounded, sub-second), and the
  read path's frame cancel-safety is preserved (cannot be tightened below the frame
  boundary without risking stream desync).
- On the self-teardown (death) edge the surviving half does NOT flush a connection
  CLSE — correct, the device already tore the connection down; graceful CLSE stays
  exclusive to `shutdown`/`close`.
- External `Arc<PersistentConnection>` holders degrade to a dead shell:
  `is_alive()==false`, calls fail fast (`BrokenPipe`/`SendError`), OS resource freed.

## Requirements

- R1: Add a `DeathSignal::is_dead()` (non-async) accessor for the reader's
  idle-boundary check.
- R2: `writer_loop` takes an `Arc<DeathSignal>` and `select!`s `writer_rx.recv()`
  against `closed.wait()`; on the death edge it breaks (drains nothing further).
  Must remain cancel-safe (only races at the idle recv point).
- R3: `reader_loop` takes an `Arc<DeathSignal>` and, at its `ReadStep::ReadTimeout`
  branch, breaks if `closed.is_dead()` else continues. No change to the
  non-cancel-safe frame-read path.
- R4: Wiring in `new_with_features`: pass `Arc::clone(&closed)` into both loops; keep
  the existing post-loop `fire()` in the spawn closures (idempotent).
- R5: No behavioral change to graceful `shutdown`/`close`/`Drop`, to flow control, or
  to the demux/routing. The death-edge path must not emit spurious "writer task gone"
  warnings.
- R6: Fix is transport-generic (USB + TCP) — no new transport-specific code.

## Acceptance Criteria

- [ ] AC1 (regression lock): a sim test where the reader dies single-sided after the
      handshake (`with_death_after_reads(1)`) while an external `SimulatedDevice`
      clone is held and `writer_rx` is never closed — asserts the writer task also
      terminates / both I/O transport clones are released, WITHOUT dropping the
      external `Arc<PersistentConnection>`. Fails on current code, passes after fix.
- [ ] AC2: `is_alive()` reports false after the death edge (already true; keep green).
- [ ] AC3: `wait_closed()` still resolves on single-sided death (keep
      `reader_death_after_handshake...` green).
- [ ] AC4: Existing graceful-path tests stay green
      (`close_sends_clse_then_drop_does_not_duplicate`,
      `drop_after_connection_closed_suppresses_per_stream_clse`,
      `drop_without_close_enqueues_clse_and_unregisters`,
      `drop_write_half_while_wrte_in_flight_is_clean`).
- [ ] AC5: `cargo test --features usb,test-support` green; `cargo clippy` clean.

## Definition of Done

- Sim regression test added to `usb/sim/tests.rs` (per the in-repo regression-net
  convention), named to reference this escaped bug.
- Doc comments on `writer_loop`/`reader_loop` updated to state the death-edge
  release invariant.
- Lint/clippy/test green; one bug = one task = one commit.
- No public API change required (R1 helper is private; verification helper, if any,
  is `#[cfg(any(test, feature="test-support"))]`).

## Out of Scope

- Reducing the reader's 1s idle timeout (writer-dies-first latency) — ≤1s is already
  a complete fix; tuning the timeout has broader perf implications.
- A public `force_close(&self)` API (report's Approach 2) — superseded by the
  in-library fix; consumers need no new API.
- Changing `USBTransport`'s clone/claim ownership model.
- xdb-side defense-in-depth (the consumer's own mitigation).

## Resolved Questions

- Q1 (verification mechanism for AC1) → **strong_count probe**. `SimulatedDevice`
  wraps `Arc<Mutex<SimState>>` (structurally analogous to `USBTransport`'s
  `Arc<Mutex<Connection>>` = the claim). After construction the strong_count = 3
  (external test clone + reader clone + writer clone). On the death edge both task
  clones drop, so the count falls to **1** (external only) WITHOUT dropping the
  external `Arc<PersistentConnection>` — directly mirroring "last transport clone
  dropped → nusb Interface dropped → claim released". Add a minimal
  `#[cfg(any(test, feature="test-support"))]` `strong_count()`-style accessor on
  `SimulatedDevice`. Current code: stuck at 2 (writer clone pinned) → AC1 fails;
  after fix: reaches 1 → AC1 passes.

## Technical Notes

- Files: `adboost/src/message_devices/usb/persistent.rs` (loops + wiring + DeathSignal
  helper), `adboost/src/message_devices/usb/sim/tests.rs` (regression test). Possibly
  a tiny `#[cfg(test/test-support)]` accessor on `SimulatedDevice` for AC1.
- tokio 1.x, `sync`+`macros`+`time` features present (`select!` available).
- Memory: aligns with `prefer-root-cause-fix-at-contract-layer`,
  `tcp-async-path-missing-usb-guarantees`, `sim-harness-regression-net`.
