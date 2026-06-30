# Research: in-repo design surface, risk, and benefit of a cancel-safe per-chunk reader `select!`

- **Query**: Strand 2 of the feasibility study — Research Questions 3, 4, 5, 6. Minimal `ADBMessageTransport` surface change for a chunk-level `select!`; death-edge safety; benefit; new risks. CODE-READING strand.
- **Scope**: internal (this repo's reader/transport source) + one nusb-0.2.3 source check for the queue model.
- **Date**: 2026-06-27

## Files read (with the load-bearing lines)

| File:line | What it establishes |
|---|---|
| `adboost/src/message_devices/usb/persistent.rs:1244-1273` | `reader_loop`: drains control FIRST, then `read_or_control` reads ONE whole frame to completion — never a `select!` against `control_rx`. |
| `persistent.rs:1254-1266` | The RECORD of the reverted naive `select!(read_whole_frame, control_rx)`: a Register/Unregister mid-read cancelled and **corrupted a large in-flight WRTE** (one of two concurrent device→host bulk streams silently stalled at 0 bytes). The crux constraint. |
| `persistent.rs:1291-1299` | Death observed ONLY at the idle `ReadStep::ReadTimeout` boundary: `if closed.is_dead() { break }`. Comment: "the only place it is safe to observe the death edge from the read path." |
| `persistent.rs:1443-1465` | `read_or_control`: `drain_control` (non-awaiting `try_recv` loop) then `read_message_with_timeout(1s)`. |
| `persistent.rs:1492-1512` | `drain_control`: pure non-blocking `try_recv` loop; `Closed` on disconnect. |
| `persistent.rs:1533-1558` | `writer_loop`'s `select!`: `writer_rx.recv()` vs `closed.wait()` — explicitly cancel-safe ("an mpsc `recv()` future dropped before it resolves loses no message, and we only ever race at this idle point, never mid-write"). |
| `persistent.rs:318-359` | `DeathSignal`: `is_dead()` (non-async snapshot for the read path) vs `wait()` (cancel-safe await for the writer); doc at 331-337 explains why the read path must NOT `.await` `wait()`. |
| `persistent.rs:685-699` | Both tasks `fire()` `closed` on exit; each watches it so a one-sided death releases the shared nusb claim. |
| `persistent.rs:1722-1733`, `:1895-1906` | `Register` sent (await) by `open_session` / `accept_device_open` BEFORE the OPEN/OKAY reply. |
| `persistent.rs:2278-2280` | `Unregister` sent (`try_send`, fire-and-forget) on session **drop**. |
| `adboost/src/message_devices/adb_message_transport.rs:24-73` | The trait: `read_message_with_timeout` (the only inbound primitive) + its transport-neutral `ReadTimeout` contract. `#[trait_variant::make(Send)]`. |
| `adboost/src/message_devices/usb/usb_transport.rs:64-78` | `Connection` struct owns `read_endpoint` + `read_buffer: FrameReadBuffer` behind `Arc<Mutex<Connection>>`. |
| `usb_transport.rs:306-327` | `transfer_with_timeout`: `submit(buf)` once, `timeout(next_complete())`; on timeout `cancel_all()` + drain. |
| `usb_transport.rs:459-498` | `read_message_with_timeout`: `loop { try_parse()? ; read_into_buffer(...) }`. |
| `usb_transport.rs:541-611` | `classify_read_completion` + `read_into_buffer`: the salvage path (`received_len > 0 ⇒ Salvage` regardless of `Cancelled` status). |
| `adboost/src/message_devices/framed_read.rs:1-50` | The feed-layer invariant: "A read timeout must never be observed mid-frame"; buffer is sans-io and lives on the transport. |
| `adboost/src/message_devices/tcp/tcp_transport.rs:116-172` | `FrameReader { reader, buffer, scratch }`; `read_message` = `loop { try_parse()? ; timeout(reader.read(scratch)) }`. |
| `tcp_transport.rs:407-465` | `upgrade_connection` builds a **fresh** `FrameReader` (new `FrameReadBuffer`) after the TLS handshake; pre-upgrade bytes deliberately dropped. |
| `adboost/src/message_devices/usb/sim/mod.rs:46-63` | Sim "Honest boundary": the harness tests "at and above the message-transport frame interface, plus byte-level cancel-safety via `ChunkedTransport`." |
| `adboost/src/message_devices/usb/sim/chunked.rs:44-58, 174-209` | `ChunkedTransport` implements only the frame-boundary trait (`read_message_with_timeout`); chunking is internal (`read_chunk`, `pending_bytes`), gated on a deadline threshold (`> 3600s ⇒ assemble whole`). |
| nusb-0.2.3 `src/device.rs:558-566, 673-687, 737-786` | Queue model: `submit` non-blocking enqueue; `pending()` count; `cancel_all()`; **`next_complete()` is documented cancel-safe** ("can be cancelled and re-created without side effects, enabling its use in `select!{}`"); panics if `pending()==0`. Drop cancels pending transfers. |

---

## RQ3 — Minimal `ADBMessageTransport` surface change

### The current surface (what a refactor displaces)

`ADBMessageTransport` (`adb_message_transport.rs:49-52`) exposes exactly one inbound
primitive: `read_message_with_timeout(&mut self, Duration) -> Result<ADBTransportMessage>`.
It returns a **whole frame**. Inside it, both transports run the same shape:
`loop { try_parse()? ; read_one_chunk_into(buffer) }` (`usb_transport.rs:491-497`,
`tcp_transport.rs:147-171`). The chunk read is private; the reader loop only ever sees
the frame boundary, so its only `select!`-able inbound event is "a whole frame, or a 1s
timeout" — which is precisely why the loop must poll on a 1s timer instead of selecting
on control.

### Minimal trait delta (sketched in prose, no impl)

The loop needs to `select!` over "one available chunk was fed into the buffer" vs
`control_rx.recv()`. The minimal, faithful surface is to **split the existing
`read_message_with_timeout` loop body into its two already-separate halves** and expose
the I/O half:

1. **A cancel-safe "feed one chunk" async method**, e.g.
   `read_chunk(&mut self) -> impl Future<Output = Result<ChunkOutcome>> + Send`, whose
   contract is: *await the next available bytes, push them into the transport's own
   `FrameReadBuffer`, and return* — it does NOT parse, does NOT take a timeout, and is
   **cancel-safe** (dropping the future before it resolves loses no buffered bytes,
   because every byte already received lives in the transport-owned buffer). `ChunkOutcome`
   would distinguish `Fed` (bytes appended) from `Eof`/error.
2. **A sans-io, non-async `try_next_frame(&mut self) -> Result<Option<ADBTransportMessage>>`**
   that just calls the already-existing `FrameReadBuffer::try_parse` (`framed_read.rs:115`)
   on the transport-owned buffer.

The reader loop then becomes (shape only): drain any frame already buffered via
`try_next_frame`; if none, `select! { _ = transport.read_chunk() => {} , ctl = control_rx.recv() => apply }`,
then re-loop to `try_next_frame`. The 1s timeout and the timeout→`continue` arm disappear.

`read_message_with_timeout` can be kept as a **default method** layered over the two new
primitives (`loop { try_next_frame()? ; timeout(read_chunk()) }`), so the handshake /
`do_connect` callers that legitimately want "read one whole frame" are unaffected. That
keeps the blast radius to "add two methods," not "rewrite every call site."

### How USB implements it (it already has the pieces)

USB is the easy side. `read_chunk` is exactly today's `read_into_buffer`
(`usb_transport.rs:582-611`) **minus the `timeout` argument and minus the `Timeout`
arm**: `submit` one max-packet-aligned buffer, `next_complete().await`, salvage via
`classify_read_completion`, `buffer.push(received)`. nusb's `next_complete()` is
documented cancel-safe (nusb `device.rs:772-773`), and the queue model means a transfer
submitted but not yet completed simply **stays pending** across a dropped `next_complete()`
future — `pending()` stays 1, and the next iteration re-awaits the SAME transfer with no
cancel and no resubmit. `try_next_frame` is the existing `read_buffer.try_parse()`
(`usb_transport.rs:492`). **The `FrameReadBuffer` does NOT need to move — it already lives
on the transport** inside `Arc<Mutex<Connection>>` (`usb_transport.rs:64-78`), which is the
whole reason a cancelled chunk loses nothing (`framed_read.rs:26-30`). One subtlety: the
loop holds `connection.lock()` across the chunk await today; under a `select!` the lock
is still held only for the duration of one `read_chunk` future and released between
iterations — but see RQ6 for the lifetime/borrow consequence of holding a `&mut self`
future across a `select!`.

### How TCP implements it without regressing the TLS-upgrade fresh-reader path

TCP's `FrameReader { reader, buffer, scratch }` (`tcp_transport.rs:116-123`) already
isolates exactly the two halves: `read_chunk` = `timeout-less` version of the inner
`timeout(reader.read(scratch))` block (`tcp_transport.rs:159-170`) — `reader.read()` on a
tokio `ReadHalf` is itself cancel-safe (a dropped `read` future loses no bytes; that is the
documented tokio `AsyncReadExt::read` contract and is the same property the TCP path
already relies on at `tcp_transport.rs:156-158`). `try_next_frame` = `self.buffer.try_parse()`.
**The TLS-upgrade path is untouched**: `upgrade_connection` (`tcp_transport.rs:407-465`)
runs during `do_connect` BEFORE the reader/writer tasks spawn (its own comment,
`:411-418`), and it constructs a brand-new `FrameReader::new` (fresh `FrameReadBuffer`) at
`:438`/`:464`. Because the buffer is a struct field that the upgrade rebuilds wholesale,
adding `read_chunk`/`try_next_frame` methods on top of it changes nothing about the
upgrade — there is no buffer to migrate, and the upgrade still reads its post-STLS CNXN
banner through the whole-frame `read_message` default (`:472`). **The buffer stays on the
transport** for TCP too (it is `FrameReader.buffer`).

**Net trait delta: two added methods (`read_chunk`, `try_next_frame`), `read_message_with_timeout`
demoted to a default method. Two-transport change, but each transport already contains the
exact code split.**

---

## RQ4 — Where is the death edge observed, and is it as safe?

### Today's model (the bar to match)

The reader observes death ONLY at `ReadStep::ReadTimeout` (`persistent.rs:1291-1298`):
when the 1s read elapses idle, it checks `closed.is_dead()` and breaks. It uses the
**non-async** `is_dead()` snapshot deliberately (`DeathSignal` doc `:331-337`) because
awaiting `wait()` inside the read path "would risk cancelling a non-cancel-safe in-flight
frame read." The writer observes death via the cancel-safe `closed.wait()` arm of its idle
`select!` (`persistent.rs:1552`). The asymmetry exists *only* because the reader's idle
unit (a whole-frame read) was not cancel-safe and the writer's (mpsc `recv`) was.

### Why a single-sided death must be observed promptly

`persistent.rs:1284-1290` + `:673-699`: when the writer hits a fatal write it `fire()`s
`closed`; the reader must then exit promptly to drop its `transport` clone and release the
shared nusb `Interface` claim (otherwise the device stays `DeviceBusy` until the last
external `Arc<PersistentConnection>` drops). Symmetrically the writer watches `closed` so a
reader death releases the claim. The reader's own fatal read errors (`ReadStep::ReadError`,
`:1304-1331`) already exit immediately — the death-observation question is specifically
about the **writer-died-while-reader-is-idle** case.

### In the new model: death observation becomes BETTER, not merely as-safe

In the chunk-select model the reader's idle point is itself a `select!`. The death edge
can be added as a **third, cancel-safe arm**:
`select! { _ = transport.read_chunk() => …, ctl = control_rx.recv() => …, () = closed.wait() => break }`.

This is provably at least as safe, and strictly more prompt, than today:

- The reader is parked on `read_chunk()` (a single in-flight bulk transfer / single socket
  read), NOT on a non-cancel-safe whole-frame read. nusb's `next_complete()` and tokio's
  `read` are both cancel-safe (nusb `device.rs:772-773`; tokio `read`), so dropping the
  `read_chunk` future when `closed.wait()` fires loses **nothing** — every received byte is
  already in the transport-owned `FrameReadBuffer` (`framed_read.rs:26-30`). This is exactly
  the property the writer already exploits for its `recv()`-vs-`wait()` race
  (`persistent.rs:1525-1530`).
- A submitted-but-incomplete USB transfer left pending when the reader breaks is cancelled
  by nusb on `Endpoint` drop ("When the `Endpoint` is dropped, any pending transfers are
  cancelled," nusb `device.rs:566`) — and we are tearing the connection down anyway, so
  there is no next-read alignment to preserve.
- Reaction latency drops from "up to 1s" (worst case: writer dies just after the reader
  began a fresh 1s idle read) to "immediate" (the `closed.wait()` arm fires the instant
  `fire()` runs). That tightens the `DeviceBusy`-release window the memory
  [[tcp-async-path-missing-usb-guarantees]] cares about.

**The reason today's comment says the idle timeout is "the only safe place" is that the
chunk was hidden inside a non-cancel-safe whole-frame read. Exposing the cancel-safe chunk
boundary makes EVERY inter-chunk point a safe death-observation point — and the writer
already demonstrates the exact pattern. So RQ4 is satisfied: the new death-observation
point is provably as safe (cancel-safe await, no byte loss) and strictly more prompt.**

---

## RQ5 — Benefit, quantified honestly

### (a) Does it eliminate the race CLASS, or merely the harm?

It eliminates the **class**, at the level this strand can verify. The race is structural:
today the reader cancels an in-flight bulk transfer (`cancel_all()`, `usb_transport.rs:319`)
*purely to poll* for control/death at the 1s boundary. The salvage fix
(`classify_read_completion`, `usb_transport.rs:541-556`) makes that cancellation lossless
**after the fact** — it copes with a transfer that completed in the same instant the timer
fired. The chunk-select design removes the *reason to cancel at all*: a transfer is never
cancelled to poll, only when the connection actually dies. So the "cancel-an-idle-transfer-
to-poll" race ceases to be reachable, rather than being made harmless. Caveat: the salvage
logic should be **kept** anyway, because a genuine timeout-based cancel still exists in the
whole-frame `read_message_with_timeout` default used by the handshake; and the strand
cannot prove no *other* cancellation site exists without the web strand's nusb confirmation
(RQ1/RQ2). Within the reader loop, the race class is eliminated.

### (b) Is removing the 1/sec idle cancel+resubmit churn measurable or negligible?

**Negligible.** The churn is: one `cancel_all()` + one drain `next_complete()` + one fresh
`submit()` per idle second per connection (`usb_transport.rs:314-326`). At 1 Hz per
connection this is far below any throughput-relevant rate — ADB bulk streaming issues
thousands of transfers/sec when active, and when *idle* (the only time the timeout fires)
there is by definition no work being displaced. There is no measured cost in the repo and
no plausible one. This is not a performance argument; it is a code-simplicity / race-class
argument. (Honest: it does remove a once-per-second syscall pair, but that is not a benefit
a maintainer would spend risk budget on.)

### (c) Control-apply latency "≤ one frame" → "immediate": does any consumer need it?

Searched every control-send site:

- `Register` is sent with `.send().await` by `open_session` (`persistent.rs:1722-1733`) and
  `accept_device_open` (`persistent.rs:1895-1906`), each **before** the corresponding
  OPEN/OKAY reply is written. The register-before-route guarantee is already handled by
  `drain_control` running both at the loop top AND immediately after a frame read
  (`persistent.rs:1342-1352`), specifically so a `Register` queued during the reply read is
  applied before that reply is classified. So correctness does NOT depend on sub-frame
  latency — it depends on draining-before-classify, which both models keep.
- `Unregister` on drop is `try_send` fire-and-forget (`persistent.rs:2278-2280`); a late
  Unregister only means a few extra frames route to a dead session id and are dropped
  harmlessly (`RouteDecision::Unknown`, `persistent.rs:1422-1428`). No latency requirement.
- `Subscribe` (`persistent.rs:959`) is a raw-tee subscription; no ordering/latency
  guarantee documented.

**Conclusion: NO real consumer needs sub-frame control latency.** The "≤ one frame" worst
case today (one frame is read to completion before queued control is applied) is already
fine for reverse-session accept and multi-session open, because the *register-before-route*
ordering is enforced by the post-read `drain_control`, not by control-apply latency. The
latency improvement is real but **not load-bearing for any current consumer**.

### Benefit summary

The only durable benefit is **race-class elimination + a simpler reader loop** (no 1s timer,
no timeout→continue arm, death observed via the same cancel-safe `wait()` arm the writer
already uses). The efficiency and latency gains are real but negligible / non-load-bearing.

---

## RQ6 — New risks (concrete)

### Risk 1 (TOP): does the chunk-select re-introduce the reverted WRTE-corruption bug?

**No — and the reason is precise.** The reverted attempt (`persistent.rs:1254-1266`)
`select!`ed control against `read_message_with_timeout`, i.e. against a **whole-frame**
future. Dropping that future on a control event cancelled an in-flight bulk transfer that
was *part-way through a multi-transfer frame* and the partial payload was discarded →
desync → the stalled WRTE. The unit-of-cancellation was a **multi-transfer frame**.

The chunk-select cancels at the **single-transfer** boundary instead. A single nusb bulk
transfer is atomic (`usb_transport.rs:158-159`: "a timeout cancels the WHOLE transfer; no
partial bytes are delivered"), and crucially the chunk future *appends into the transport-
owned `FrameReadBuffer` before resolving* — so dropping the `read_chunk` future between
transfers loses nothing: either the transfer hadn't completed (it stays pending in nusb's
queue, re-awaited next loop) or it completed and its bytes are already pushed. The
`FrameReadBuffer` "byte-at-a-time delivery stays aligned" test (`framed_read.rs:243-264`)
is the pure-form proof that a frame delivered one cancellable chunk at a time reassembles
intact. **The crux: whole-frame select cancels mid-frame (lossy); chunk select cancels
mid-stream-between-frames (lossless because the buffer retains every byte).** This is the
same structural reason the salvage fix and the cancel-safe `FrameReadBuffer` were built.
A design that re-exposed a whole-frame `select!` would be an automatic NO; this one does
not.

One concrete must-not-regress: `read_chunk` must keep the salvage semantics
(`classify_read_completion`, `usb_transport.rs:541-556`) — a transfer that completes in the
same instant it is dropped must still have its bytes pushed. If the cancel-safe form simply
*never cancels to poll* this case largely evaporates, but the salvage classification must
remain for the genuine teardown-time cancel.

### Risk 2: half-open / death-detection regression

Low, but real to verify. Today an idle reader wakes every 1s and checks `is_dead()`
(`persistent.rs:1292`). In the new model, if the death arm is `closed.wait()`, the reader
wakes immediately on `fire()` — strictly better. BUT: if a future implementer forgets the
`closed.wait()` arm and relies only on `read_chunk` erroring, a reader parked on
`read_chunk` while the **writer** dies but the **device keeps the IN endpoint silent** would
park forever (no chunk, no error) — exactly the `DeviceBusy` leak `:673-699` warns about.
So the death arm is mandatory, not optional. This is a design-discipline risk, recorded
here so the implementation task cannot drop it.

### Risk 3: TCP-path divergence

Moderate. A trait change is a two-transport change (`prd.md` constraint;
[[tcp-async-path-missing-usb-guarantees]] is the recurring bug class where the async path
silently lacks a USB guarantee). The mitigation is already structural: both transports hold
the same `FrameReadBuffer` and already split into chunk-read + try-parse, so the two
`read_chunk` impls are thin. The risk is that the TCP `read_chunk`'s cancel-safety rests on
tokio `ReadHalf::read` being cancel-safe (true, but undocumented in-repo), whereas USB's
rests on nusb `next_complete` (documented `device.rs:772`). Both must be asserted by tests
on both transports or the divergence re-opens.

### Risk 4: reader complexity / borrow lifetime

Moderate. `read_chunk(&mut self)` returns a future borrowing `&mut self` (the transport).
Holding that future in a `select!` arm borrows the transport mutably for the arm's
lifetime; the other arm (`control_rx.recv()`) borrows `control_rx`, which is fine (disjoint).
But the USB impl locks `connection` inside `read_chunk`; the lock guard must be acquired and
released *inside* one `read_chunk` call (not held across the `select!`), or a second
borrow (e.g. `try_next_frame` needing the same buffer) deadlocks. This is solvable but is a
genuine new source of borrow/lock subtlety the current "one method does the whole loop body"
design avoids. The `#[trait_variant::make(Send)]` macro (`adb_message_transport.rs:24`) also
constrains the method shape: per the trait's own note (`:19-23`), default methods must be
written as `-> impl Future + Send`, not `async fn`, which the new default
`read_message_with_timeout` must follow.

### Risk 5: test-reachability — can the sim exercise a chunk-level select?

**This is the sharpest risk for proving the design.** The sim's honest-boundary doc
(`sim/mod.rs:46-63`) states the harness tests "at and above the message-transport frame
interface, plus byte-level cancel-safety via `ChunkedTransport`." But both
`SimulatedDevice` and `ChunkedTransport` implement the trait at the **frame boundary**:
`ChunkedTransport::read_message_with_timeout` (`chunked.rs:174-209`) does its chunking
**internally** and only ever returns a whole frame or `ReadTimeout`; its `read_chunk`
knob is private state gated on a deadline threshold (`chunked.rs:191`). So **today's sim
plugs in at exactly the frame boundary the refactor wants to move below.**

Consequences:
- If `read_chunk`/`try_next_frame` are added to the trait, `ChunkedTransport` would have to
  implement them — which it *can* (it already owns `pending_bytes` + a `FrameReadBuffer`),
  so a chunk-level select *is* reachable in sim after a sim refactor. The sim is NOT a hard
  blocker.
- BUT the sim cannot prove the property that actually matters: that nusb's `next_complete`
  leaves a pending transfer intact across a dropped future and re-awaits the SAME transfer.
  That is a real-kernel/nusb behavior the honest-boundary doc explicitly disclaims
  ("the mocks *emit* the variants … they do not prove the kernel, nusb, or tokio actually
  produces them," `sim/mod.rs:52-55`). So the riskiest correctness claim of the whole
  refactor (RQ1/RQ2, the web strand's domain) is **hardware-only** to fully verify.

---

## Preliminary lean (THIS strand only)

**CONDITIONAL — leaning NO-GO on the current evidence; at best a tightly-scoped GO.**

Reasoning, tied to the project's risk/benefit bar ([[prefer-root-cause-fix-at-contract-layer]]
one-bug-one-commit / no speculative refactors; [[user-maintainer-profile]] standard-defaults):

- **A correct design demonstrably exists** (RQ3 trait delta is minimal and each transport
  already contains the split; RQ4 death observation becomes strictly safer via the same
  cancel-safe `wait()` arm the writer already uses; RQ6/Risk-1 shows it does NOT reproduce
  the WRTE-corruption bug because the cancellation unit drops from multi-transfer-frame to
  single-transfer-between-frames). So this is *not* an automatic NO.
- **But the benefit is weak against the bar.** The race is already *correct* post-salvage
  (`prd.md:41-45`); this refactor buys race-class elimination + a simpler loop, with
  efficiency (RQ5b) negligible and latency (RQ5c) non-load-bearing for every real consumer
  found. That is a "nice cleanup," not a bug fix.
- **The new risks are real and asymmetric**: a two-transport contract change
  ([[tcp-async-path-missing-usb-guarantees]]), new borrow/lock subtlety (RQ6/Risk-4), a
  mandatory-not-optional death arm (RQ6/Risk-2), and — decisively — the single
  highest-value correctness claim (nusb re-await-pending-across-select) is **hardware-only
  to verify** (RQ6/Risk-5 + `sim/mod.rs:52-55`). Spending real risk on a non-bug refactor
  whose core safety property the regression net cannot cover runs against the maintainer
  philosophy.

**Recommendation from this strand:** unless the web strand returns an *unambiguous*,
*documented* nusb guarantee that a pending IN transfer survives a dropped `next_complete()`
across a `select!` and is re-awaited losslessly (which would shrink Risk-5 to a sim-coverable
property), the higher-value action is the PRD's NO-GO branch (`prd.md:95-98`): **document the
current design as intentionally-correct** (salvage + frame-boundary drain-control + the
deliberately-asymmetric `is_dead()` vs `wait()` death observation) so the now-harmless race
is not mistaken for an unfixed hazard. The main agent should weigh this against the web
strand's nusb findings before settling go/no-go.

## Caveats / Not Found

- RQ1/RQ2 (nusb cancel-safety of holding a pending transfer across selects) are the WEB
  strand's domain; I confirmed only the in-repo-visible facts from nusb-0.2.3 source
  (`device.rs:558-566, 737-786`): `next_complete()` is documented cancel-safe and the queue
  retains pending transfers, drop cancels them. Whether a *dropped-mid-await* `next_complete`
  re-awaits the identical pending transfer with zero side effects is the claim the web strand
  must nail down; the source comment supports it but I did not exercise it.
- I did not run or build anything (research-only task).
