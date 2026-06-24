# SimulatedDevice — comprehensive software ADB device test harness

## Goal

Turn protocol/timing/state-machine bugs that today require a physical phone + a
manual `adb root` loop — and that have repeatedly *escaped to the downstream
`xdb` crate* before being caught — into a deterministic, exhaustive `cargo test`
suite inside adboost itself. Generalize the proven `ScriptedTransport`
(`adboost/src/message_devices/usb/persistent.rs:2861`, the only transport mock
today, exercising *only* `do_connect` retry budgets) into:

1. **`SimulatedDevice`** — a stateful adbd model at the **frame** layer
   (implements `ADBMessageTransport`), and
2. **`ChunkedTransport`** — a **byte-level** fault-injecting transport for the
   sub-frame bug class the frame model structurally cannot reach.

Plus **`SimDeviceBackend` + `SimRegistry`** (Phase C) to drive the smartsocket
frontend end-to-end through the `DeviceBackend` trait.

> **本任务为父跟踪任务，已拆为三个子任务（各一聚焦 commit）：**
> - [`../06-24-sim-phase-a-handshake/prd.md`](../06-24-sim-phase-a-handshake/prd.md) — 握手状态机 + 两 mock 骨架 + `test-support` 门控
> - [`../06-24-sim-phase-b-session/prd.md`](../06-24-sim-phase-b-session/prd.md) — session 状态机 + 流控 + `ChunkedTransport` 故障场景
> - [`../06-24-sim-phase-c-backend/prd.md`](../06-24-sim-phase-c-backend/prd.md) — `SimDeviceBackend`/`SimRegistry` + 前端重连/对齐
>
> 实现与 jsonl 策展落在子任务上；本 PRD 是共享的研究依据与全局设计。

Mandate (from the maintainer): the harness must be **comprehensive** — cover as
many scenarios, boundary conditions, and timing/ordering edges as possible — so
adboost's own `cargo test` is the net that catches these bugs before any external
consumer does.

## What I already know (verified against the code)

- `ADBMessageTransport` (`adboost/src/message_devices/adb_message_transport.rs:25`)
  is `: ADBTransport + Clone + Send + 'static`; the read-timeout contract
  (lines 37–48) mandates `RustADBError::ReadTimeout` for an idle deadline — the
  "idle ≠ failure" signal the reader loop keys on (`ReadStep::ReadTimeout =>
  continue`).
- `PersistentConnection<T>` is generic over `T: ADBMessageTransport`
  (`persistent.rs:508`); the transport is **moved into** the reader/writer tasks
  and **cloned** into two halves (`persistent.rs:649`) — bulk-IN (reader's
  `read_message`) and bulk-OUT (writer's `write_message`). A faithful sim MUST
  share device state across the two clones (behind `Arc<Mutex<_>>`/channels,
  lock never held across `.await`, exactly as `ScriptedTransport` does).
- Reader uses a **1s** per-frame read timeout; `drain_stale` uses **100ms**;
  `open_session`/AUTH use **10s** — all timeout-bound edges need
  `#[tokio::test(start_paused = true)]` (the existing `do_connect_*` tests
  already do this).
- Reuse, never re-implement: framing (`ADBTransportMessage::try_new`,
  header decode), `negotiate_delayed_ack` (`persistent.rs:483`),
  `DeviceFeatureSet::from_banner` (`persistent.rs:639`), `FlowControl` +
  `encode_okay_payload`/`parse_okay_delta`/`INITIAL_DELAYED_ACK_BYTES`/
  `MAX_PAYLOAD` (`flow_control.rs`), `DeathSignal` (`persistent.rs:317`),
  `is_alive()` (`persistent.rs:1952`).
- **Trait is already transport-neutral** (refinement vs the original design
  note): `DeviceBackend` (`server/backend.rs:220`) session methods return
  `MultiplexedSession`/`SyncSession`, NOT `PersistentUsbConnection`. Only
  `DefaultDeviceBackend::get_or_open` (`default_backend.rs:371`) is USB-locked.
  So `SimDeviceBackend` implements the trait directly — no fork of
  `DefaultDeviceBackend`.
- The byte-layer cancel-safety/framing parity bugs are **already unified +
  regression-tested**: both transports share `FrameReadBuffer`
  (`framed_read.rs`), with byte-layer tests in `framed_read.rs`,
  `usb_transport.rs`, `tcp_transport.rs`. `ChunkedTransport` adds the
  **consumer-side, live reader/writer-loop** assertions over partial/over-
  delivered/fail-after-k-bytes byte streams that those FrameReader-level tests
  don't drive end-to-end.
- The 90 frontend tests run on a `MockBackend` whose `open_local_service` is
  `unimplemented!()` (`frontend.rs:1611`) and whose `LifecycleEvent`s are
  hand-fed (`frontend.rs:1821`) — so the **session-bridge path** and
  **real-death→event emission** are untested. Phase C closes exactly these.
- No `sim`/`SimulatedDevice`/`SimDeviceBackend`/`SimRegistry`/`test-support`
  exists yet — all new work. Existing features: `default=["framebuffer"]`,
  `server=["usb",...]`, `tracing-init`.

## Research References

- [`research/escaped-bug-history.md`](research/escaped-bug-history.md) — ~26
  reactively-fixed bugs (B1–B20) across 4 themes, each with repro verdict; the
  delayed_ack saga (B1/B2/B3a/B-feat), half-open `is_alive` (B8), two-OKAY
  wait-for (B10), short-sync-frame panic (B-recv), and the error-family
  classifiers (B14/B17) are the strongest frame-level wins.
- [`research/protocol-state-edges.md`](research/protocol-state-edges.md) — the
  **~80-edge catalog** (CNXN, stale-CLSE drain, AUTH, delayed_ack, flow control,
  OPEN, accept, reader routing, liveness/teardown, session byte-stream, Drop,
  STLS) with per-edge test status + sim-reproducibility. This is the master
  checklist the suite must exhaust.
- [`research/parity-bug-classes.md`](research/parity-bug-classes.md) — the two
  recurring bug classes mapped to phases; class-1 byte-layer is already covered
  (sim adds consumer-side), class-2 host-protocol is highly sim-reproducible at
  Phase C.

## Decision (ADR-lite)

**Context**: Bugs keep escaping to xdb because the only deterministic test seam
(`ScriptedTransport`) covers just `do_connect`; everything above it (AUTH, live
reader/writer loops, sessions, flow control, liveness, server bridging) is
hardware-only. The maintainer wants an exhaustive in-repo net.

**Decision**:
- Build TWO complementary mocks: a frame-level `SimulatedDevice` (the bulk of
  the suite) and a byte-level `ChunkedTransport` (the sub-frame cancel-safety
  class B4/B5/B7/B9).
- Exhaustively cover **all ~80 edges** end-to-end through the live
  `PersistentConnection`, including re-covering edges that already have I/O-free
  unit tests — the harness becomes the single comprehensive net (accepting some
  redundancy with existing pure-helper unit tests).
- Gate with `#[cfg(any(test, feature = "test-support"))]`: free for adboost's
  inline tests via `test`; opt-in `test-support` feature lets `adboost_cli`
  selftest / `xdb` reuse it (matches "standard defaults + opt-in" philosophy).
- `SimDeviceBackend` implements the `DeviceBackend` trait directly.

**Consequences**: ~2× the originally-sketched surface (two mocks, exhaustive,
server layer). Some overlap with existing unit tests is intentional. The honest
boundary (below) is documented so coverage claims stay truthful.

## Phasing (each phase = one focused commit)

- **Phase A — handshake (no `server`):** `SimulatedDevice` + `DeviceProfile`
  (banner/version axis) + `ChunkedTransport` skeleton. Exhaust the CNXN
  (CNXN-1..13), stale-CLSE drain (DRAIN-1..5), AUTH (AUTH-1..6), delayed_ack
  (DACK-1..7), and the transient/retry budget edges through full
  `PersistentConnection::new`. Replaces the 3 existing `ScriptedTransport`
  tests. Death-signal seam (reader death → `is_alive()==false`).
- **Phase B — session + flow control (no `server`):** OPEN (OPEN-1..8), accept
  (ACC-1..5), reader routing (RTE-1..12), flow control (FC-1..14), session
  byte-stream (SES-1..15), teardown/Drop (TD-1..5), liveness (LIV-1..14), plus
  the `ChunkedTransport` cancel-safety scenarios (B4/B5/B7/B9 consumer-side).
- **Phase C — server (`server` + `test-support`):** `SimDeviceBackend` +
  `SimRegistry`. Host-protocol parity (host-usb/transport-usb `-d`/`-e`, tport,
  per-device `host:features`, devices/track-devices), real session bridging
  (`open_local_service` → sim-backed `MultiplexedSession`), and the headline
  re-enumeration: connection death → `LifecycleEvent::TransportReset` →
  `wait-for-disconnect` unblock + rule retention, back-to-back root/unroot
  recovery via reopen.

## Acceptance Criteria

- [ ] `SimulatedDevice` implements `ADBTransport` + `ADBMessageTransport`,
      reusing existing framing/negotiation/flow-control helpers (zero
      re-implementation), sharing state across the reader/writer clones.
- [ ] Read on an empty outbound queue returns `RustADBError::ReadTimeout`
      (honors the idle≠failure contract; drives `ReadStep::ReadTimeout`).
- [ ] `ChunkedTransport` can: split a frame across a read timeout, over-deliver
      (>1 frame per read), and fail a write after k bytes — driving the live
      reader/writer loop, not just `FrameReader`.
- [ ] Every edge in `research/protocol-state-edges.md` (~80) has a corresponding
      end-to-end test OR an explicit, documented reason it stays byte/hardware.
- [ ] Each escaped bug (B1, B2, B3a, B8, B10, B14, B17, B-feat, B-recv, and the
      Phase-C reconnect cluster) has a named regression test that fails against
      the pre-fix behavior in spirit.
- [ ] The 3 existing `ScriptedTransport` tests are replaced with no loss of
      coverage.
- [ ] `SimDeviceBackend` drives the frontend end-to-end: real `open_local_service`
      bridge + real connection-death `TransportReset` emission.
- [ ] `cargo test` (default, `server`, `test-support`) + `cargo clippy
      --all-targets` green; NO new default-feature dependencies.

## Definition of Done

- Tests added/updated; lint/typecheck/CI green across feature combos.
- Module-level doc states the honest boundaries (below).
- No change to default-feature dependency surface.
- Journal + spec note capturing the harness as the standing regression net.

## Honest boundaries (documented in the module)

The sim suite tests protocol/state-machine logic **at and above** the
message-transport frame interface, plus byte-level cancel-safety via
`ChunkedTransport`. It does NOT prove: real OS IOKit error codes / latency
distributions (the mocks *emit* these variants to drive the classifier; they do
not prove the kernel/`nusb`/`tokio` produces them); real device shell/filesystem;
real TLS/STLS upgrade (trait default is a no-op); IOKit re-enumeration to a new
registry id (B12/B15 — an OS artifact; only the reopen-layer *reaction* is
testable). Those remain hardware tests.

## Out of Scope (explicit)

- Real USB/TLS/filesystem behavior; discovering an *unimplemented* native adb
  prefix (needs diffing the real `adb` binary, not a sim).
- Migrating `pyadb_client` / `examples/mdns`.

## Technical Notes

- New module `adboost/src/message_devices/sim/` (+ `server/sim_backend.rs` for
  Phase C), declared `#[cfg(any(test, feature = "test-support"))]`.
- Components: `SimulatedDevice` (frame state machine: CNXN→banner|AUTH,
  OPEN→OKAY|CLSE, host-OKAY→credit), `DeviceProfile` (android_11 classic /
  android_16 windowed / unauthorized), `Scenario` (fault/lifecycle injection:
  transient_writes, die_after_reads, withhold_credit, slow_read),
  `ChunkedTransport` (byte-level), `SimDeviceBackend` + `SimRegistry`
  (checkout/restart = re-enumeration model).
- State = single `Arc<Mutex<SimState>>` shared across clones; lock never across
  `.await`. Timeout edges use `start_paused`.
