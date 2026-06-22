# TCP `read_exact` not cancel-safe: timeout drops partial frame → fatal `ConversionError`

## Goal

IP-direct ADB connections (`adb connect <ip>`) randomly drop on ordinary
large-output commands (e.g. `ifconfig`). Root cause: `TcpTransport`'s read path
loses bytes already read off the wire when its 1s read timeout fires, permanently
desyncing the framed stream. Make the TCP read path **cancel-safe** so a read
timeout never discards received bytes — mirroring the robustness USB already has
via its `read_residual` buffer.

An architectural review (6-lens, adversarially verified — see
`research/transport-architecture-review-raw.json`) found the reported bug is **not
isolated**: it is one instance of a recurring class — *"the async/TCP path is
missing a robustness guarantee the USB path already has."* The single load-bearing
invariant — **a timeout must never be observed mid-frame** (`ReadTimeout` may only
be returned having consumed ZERO bytes of the current frame) — is enforced only by
convention and re-implemented (and mis-implemented) per transport. This task fixes
the whole **cancel-safety / frame-desync class** at once, in a shared layer:

- **#1 (critical)** TCP read drops partial bytes on timeout → desync → fatal
  `ConversionError` (the reported bug).
- **#2 (high)** TCP write leaves a truncated frame on the wire on timeout; the
  writer loop appends the next frame to the truncation → desyncs the *device-bound*
  stream; never classified fatal.
- **#3 (medium)** USB read loses partial bytes when a *later* transfer within a
  multi-transfer frame field times out (USB is immune only for single-transfer
  fields, not multi-transfer payloads).

Out-of-class findings (#4 panic, #5 writer teardown, #6/#7 unbounded wire allocs)
are split into their own follow-up tasks per the *one bug = one task = one commit*
rule. See "Follow-up tasks" below.

## What I already know (verified against rev 641baf9)

- **Drop site** — `tcp/tcp_transport.rs:108-120` `read_exact_timeout`:
  `tokio::time::timeout(timeout, reader.read_exact(buf))`. `AsyncReadExt::read_exact`
  is **not cancel-safe**: when the timeout fires the future is dropped and any bytes
  already written into `buf` are lost.
- **Two reads per frame** — `read_message_with_timeout` (`tcp_transport.rs:253`, `:269`)
  calls `read_exact_timeout` once for the 24-byte header and once for the payload.
  Either call crossing the 1s boundary mid-read permanently desyncs the stream.
- **Desync → fatal** — next iteration reads 24 bytes from a misaligned offset as a
  header → `ADBTransportMessageHeader::try_from` → illegal command word →
  `RustADBError::ConversionError`.
- **Reader treats it as fatal (correctly)** — `usb/persistent.rs:1018-1045`: only
  `InvalidIntegrity` (full frame already consumed, still aligned) is recoverable;
  `ConversionError` is deliberately fatal because the payload is still pending on the
  wire. The classification is right; the *desync* is the bug.
- **1s timeout is load-bearing** — `persistent.rs:1173-1178`: the 1s read timeout
  returns control to the reader loop between frames so queued control mutations are
  applied. We must NOT remove it (report's mitigation #3 rejected).
- **USB is immune** — `usb/usb_transport.rs:471-509` `read_exact` keeps a
  `read_residual: Vec<u8>` across calls and its timeout is nusb's atomic transfer
  cancellation (`Cancelled`, whole transfer voided, no half-frame).
- **Trait contract** — `adb_message_transport.rs:35-52`: on timeout before a complete
  message, impls MUST return `ReadTimeout`. The contract is silent on alignment, but
  USB's impl guarantees it; TCP's does not. This is the implicit-contract gap to close.

## Key insight: the report's two "approaches" are one fix

Approach #2 (`timeout(single read)` + accumulate) only stays aligned if its
accumulation buffer **survives across `read_message_with_timeout` calls** — and that
surviving buffer *is* the residual of approach #1. The robust fix is both together:

- A **persistent per-connection read buffer** that holds bytes already read but not yet
  consumed by the current frame field, AND
- **timeout granularity at the single-`read()` level**, so cancellation can only ever
  happen *between* `read` syscalls, with every received byte already safely in the buffer.

## Status: IMPLEMENTED (awaiting user acceptance)

All Class A + Class B fixes are committed on `main` (not pushed). Quality gate
green on both feature sets (fmt, clippy pedantic, full workspace tests).

| Commit | Scope |
|--------|-------|
| `1aac71c` | **Class A** — shared `FrameReadBuffer` cancel-safe read; TCP read (#1), USB read (#3), TCP write poison + writer_loop teardown (#2) |
| `23c2078` | #4 `recv_file` short/empty-frame panic guard |
| `5bd58ae` | #6 proxy framebuffer allocation bound |
| `f45e91d` | #7 proxy LIST/RECV wire-length bound |
| `584dd75` | #5 `is_alive()` reflects writer task |

Verified by an adversarial code review (no blockers). Note: TCP write poisoning is
intentionally conservative (fires even on a pre-write timeout with 0 bytes sent) —
safe-over-latency, can never desync.

## The single invariant this task enforces

> **A per-read/per-write timeout must never be observed mid-frame.** The neutral
> `ReadTimeout` may be returned only when ZERO bytes of the current frame have been
> consumed; on the write side, a timeout after any byte of a frame has been written is
> connection-fatal (a truncated frame is unrecoverable for the framed peer).

Today this invariant is implicit and re-implemented per transport. This task makes it
**explicit and shared**.

## Requirements

### Read cancel-safety (#1 TCP, #3 USB)
- A read timeout MUST NOT discard any bytes already read off the socket/endpoint.
- After any number of read timeouts, the next `read_message_with_timeout` MUST resume
  frame parsing exactly where the previous read left off (stream stays aligned).
- `ReadTimeout` is returned only on a true idle window with zero current-frame bytes
  consumed (per-read idle-timeout model — see Decision).
- USB: a mid-field timeout across a *multi-transfer* frame field must preserve the
  already-filled prefix (carry it across calls), not drop `out[..offset]`.

### Write cancel-safety (#2 TCP)
- A write timeout that fires after any byte of a frame has been written MUST be treated
  as connection-fatal: poison the write half rather than returning a recoverable-looking
  error that lets the writer loop append the next frame to a truncated one.
- `persistent.rs::writer_loop` MUST tear the connection down on a fatal write failure
  (matching the reader's fatal handling) instead of warn-and-continue.

### Shared layer + contract
- Introduce ONE shared cancel-safe, residual-buffered framed-I/O abstraction used by
  both `tcp_transport` and `usb_transport`, owning the persistent read buffer and the
  "complete-or-zero-consumed" invariant. (#1 and #3 collapse into one implementation.)
- Preserve the existing trait timeout contract (`RustADBError::ReadTimeout` on
  incomplete-message timeout); keep the 1s reader-loop control-drain behavior intact.
- Keep the read/write split (independent locks) unchanged; `#![forbid(unsafe_code)]`
  must still hold; clippy pedantic clean; MSRV 1.88.0.

## Acceptance Criteria

- [ ] **Shared layer exists** and is used by both TCP and USB read paths (single
      residual buffer implementation; USB's bespoke `read_residual`/`fill_and_carry`
      either folds into it or is the basis for it).
- [ ] **Transport contract tests** run against every `ADBMessageTransport` impl via a
      scriptable byte source that can: (a) deliver a frame in fragments straddling the
      deadline, (b) deliver a header then stall before the payload, (c) deliver a short
      packet mid-field (USB multi-transfer), (d) stall mid-write under backpressure.
      Assert: `ReadTimeout` is only ever observed at a frame boundary, and the next
      frame still decodes (no desync).
- [ ] **#1**: TCP read — a payload/ header read straddling the 1s timeout yields an
      intact message and an aligned next frame (no `ConversionError`).
- [ ] **#2**: TCP write — a mid-frame write timeout poisons the connection and
      `writer_loop` tears down rather than appending the next frame to a truncation.
- [ ] **#3**: USB read — a mid-field timeout across a multi-transfer payload preserves
      the filled prefix and resumes aligned.
- [ ] Existing tests (`connect_sets_tcp_nodelay`, `read_does_not_block_concurrent_write`,
      USB `fill_and_carry_*`, `*_maps_to_*`) still pass.
- [ ] `cargo test`, `cargo clippy -- -D warnings` (pedantic), `cargo fmt --check` green.

## Definition of Done

- Shared cancel-safe framed-I/O layer + contract tests added and passing.
- All three Class-A findings (#1/#2/#3) closed and covered by tests.
- Lint / fmt / build green; doc comments explain the invariant (mirroring USB's rationale).
- One coherent commit (the Class-A contract fix); follow-up tasks filed for #4–#7.

## Follow-up tasks (filed, NOT in this commit)

- **#4 (high)** `recv_file` panic: guard `payload[len-8..len-4]` against short/empty
  device frames (`adb_session.rs:113`). Remote-triggerable panic on a 0-byte WRTE.
- **#5 (medium)** `writer_loop` connection teardown + `is_alive()` reflecting
  `writer_handle` (half-open connection reuse + silent FireForget credit loss).
  *Note: #2's writer-loop teardown overlaps here — coordinate so they don't conflict.*
- **#6 (medium)** Proxy framebuffer `vec![0u8; size]` unbounded device u32 → bound it.
- **#7 (low)** Proxy LIST/RECV unbounded wire-length allocations → clamp.

## Out of Scope

- Findings #4–#7 (own tasks above).
- Removing or enlarging the 1s reader timeout (report mitigation #3 — rejected; race not
  eliminated and control drain depends on it).
- The reader's fatal-vs-recoverable *read* classification (it is correct; the desync is
  the bug). We DO add a *write*-side fatal classification (#2).
- TLS handshake logic (only the steady-state read/write paths change).

## Decision (ADR-lite)

**Context**: Once the TCP read path keeps a persistent buffer (so no received bytes are
ever dropped on cancellation), the 1s read timeout can wrap either each individual
socket `read()` or a whole frame field. The choice changes how often `ReadTimeout`
surfaces to the reader loop.

**Decision**: **Per-read idle timeout.** The 1s budget applies to each individual
`reader.read(&mut chunk)`; whenever bytes arrive, received data is appended to the
persistent buffer and the timer effectively resets for the next read. `ReadTimeout` is
returned only on a *true* 1s idle (no bytes at all for the window) before a complete
message is assembled. An actively-streaming large frame (e.g. `ifconfig`) keeps
assembling across many reads and never times out mid-frame.

**Consequences**:
- Cancellation can only ever occur *between* `read()` calls, with every received byte
  already safely in the buffer → the stream can never desync. This is the exact analog
  of USB's atomic-transfer model.
- Control-channel drain still happens between frames and on any true idle window, so the
  load-bearing 1s reader-loop behavior (`persistent.rs:1173`) is preserved.
- Slightly fewer `ReadTimeout` returns to the loop than today (only on real idle, not
  mid-frame), which is strictly better for liveness and still drains control between frames.

## Research References

- [`research/transport-architecture-review-raw.json`](research/transport-architecture-review-raw.json)
  — 6-lens, adversarially-verified architectural review. 10 confirmed findings (3 refuted),
  deduped to 7 issues in 2 classes. Source of the #1–#7 numbering and the shared-layer
  architectural recommendations.

## Technical Notes

- Primary files: `tcp/tcp_transport.rs` (read `:108-120`/`:244-288`, write `:122-141`/`:290-308`),
  `usb/usb_transport.rs:471-509` (residual model — basis for the shared layer),
  `adb_message_transport.rs:35-52` (timeout contract), `persistent.rs:1173-1178` (1s reader
  timeout), `persistent.rs:1237-1256` (writer_loop — #2 teardown).
- Spec to honor: `.trellis/spec/backend/adb-wire-protocol-contract.md` (magic-only receive
  integrity; do NOT add crc validation), `.trellis/spec/backend/error-handling.md`
  (`RustADBError` variants, no panics on wire path), `.trellis/spec/backend/quality-guidelines.md`
  (clippy pedantic, MSRV 1.88.0, testing style).
- Bug report: `/private/tmp/adboost-bug-report-tcp-read-cancel-desync.md`.
- Prior art / meta-pattern: commits `1e28628` (split halves), `e90ab60` (NODELAY),
  `4951301` (unify read-timeout contract) — all "TCP/async path missing a USB guarantee".
  The shared layer is the structural fix so this class stops recurring.
- Architectural recommendation (from review) worth honoring even if it grows scope slightly:
  make the invariant *executable* via contract tests run against every transport impl, so the
  next transport added cannot silently lack it.
