# Research: nusb 0.2.x queue-model & cancel-safety of a per-chunk reader `select!`

- **Query**: Is `endpoint.next_complete()` a cancel-safe "await one chunk WITHOUT cancelling it" primitive? Can a single in-flight bulk transfer be held across a `select!` losslessly, where a whole-frame read could not? (Task Research Questions 1 & 2; supports 3 & 4.)
- **Scope**: external (nusb crate source) + internal (repo grounding)
- **Date**: 2026-06-27

## Version note (READ FIRST)

The task hint pointed at `nusb-0.2.4`, but `Cargo.lock` pins **`nusb 0.2.3`**
(`Cargo.lock:1038-1039`, `version = "0.2.3"`). All citations below are from the
**locked 0.2.3** source at
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/nusb-0.2.3/`. I spot-read
0.2.4 too; the `Endpoint` queue API (`submit` / `next_complete` /
`poll_next_complete` / `pending` / `cancel_all`) is identical in shape between
the two. The findings below hold for the version actually built. A future
implementation task should re-verify if the pin is bumped.

The repo's own doc comments already assert these semantics
(`usb_transport.rs:59-60`, `:287-305`); this document independently confirms them
against nusb source rather than taking the comment at face value.

## Findings

### Files Found

| File Path | Description |
|---|---|
| `~/.cargo/.../nusb-0.2.3/src/device.rs` | Public `Endpoint<EpType,Dir>` API: doc block (`:554-651`), `pending` (`:675`), `cancel_all` (`:684`), `submit` (`:753`), `next_complete` (`:784`), `poll_next_complete` (`:796`), `wait_next_complete` (`:811`), `transfer_blocking` (`:829`) |
| `~/.cargo/.../nusb-0.2.3/src/platform/linux_usbfs/device.rs` | Linux backend: `LinuxEndpoint` queue = `VecDeque<Pending<..>>` (`:748`), `submit` (`:786`), `poll_next_complete` (`:801`), `cancel_all` (`:769`), `Drop` (`:844`) |
| `~/.cargo/.../nusb-0.2.3/src/transfer/internal.rs` | `Notify`/`NotifyState` (`:19-89`), transfer state atomics `STATE_IDLE/PENDING/...` (`:106-115`), `take_completed_from_queue` (`:198`), `notify_completion` (`:232`) |
| `adboost/src/message_devices/usb/usb_transport.rs` | Repo read primitive: `transfer_with_timeout` (`:306-327`), `classify_read_completion` (`:541`), `read_into_buffer` (`:582`), `read_message_with_timeout` loop (`:459-498`) |
| `adboost/src/message_devices/usb/persistent.rs` | Reader loop + prior-attempt rationale (`:1244-1298`) |

---

### Q1 — Is `next_complete()` truly cancel-safe? What happens to a submitted transfer if the `select!` control branch wins?

**YES — confirmed by both the doc contract AND the implementation.** A transfer
already `submit()`ed remains pending and is yielded intact by a LATER
`next_complete()`; no bytes are lost and no resubmit is needed.

Doc contract (`device.rs:770-779`):

```
/// Return a `Future` that waits for the next pending transfer to complete.
///
/// This future is cancel-safe: it can be cancelled and re-created without
/// side effects, enabling its use in `select!{}` or similar.
```

Why this is *structurally* true, not just a claim — the future carries no state:

- `next_complete()` is `poll_fn(|cx| self.poll_next_complete(cx))`
  (`device.rs:784-786`). The returned future borrows `&mut self` and holds
  **nothing else**. Dropping it (the `select!` loser) drops only the borrow.
- All completion/queue state lives in the **backend endpoint**, not the future.
  On Linux the queue is `pending: VecDeque<Pending<TransferData>>` owned by
  `LinuxEndpoint` (`device.rs:748`). `submit()` does
  `self.pending.push_back(...)` (`device.rs:786-791`) — the transfer sits in that
  `VecDeque` independent of any future.
- `poll_next_complete` (`device.rs:801-810`) does exactly two things:
  `self.inner.notify.subscribe(cx)` then
  `take_completed_from_queue(&mut self.pending)`. It returns `Poll::Ready` only if
  the front transfer's atomic state says complete; otherwise `Poll::Pending`. It
  does **not** pop, cancel, or mutate a still-pending transfer.
- Completion is recorded by the kernel-completion path setting the transfer's
  `AtomicU8` state (`internal.rs:106-115`, `notify_completion` `:232`) — this
  happens **whether or not** anyone is currently subscribed/polling. So a
  completion that lands while the future is dropped is not lost; it's latched in
  the atomic and observed by the next poll.

`Notify::subscribe` just overwrites the stored waker
(`internal.rs:56-58`: `*self.state.lock() = NotifyState::Waker(cx.waker().clone())`).
Dropping the future leaves a stale waker in the slot at worst; the **next**
`next_complete()` re-subscribes with the fresh waker and re-checks the queue. No
edge is dropped because the completion truth is the atomic, not the waker.

So in:

```rust
tokio::select! {
    c = endpoint.next_complete() => ...,      // loser: future dropped
    _ = control_rx.recv() => ...,             // winner
}
```

if `control_rx` wins, the submitted transfer **stays in `pending`**,
`endpoint.pending()` is unchanged, and a subsequent `endpoint.next_complete()`
yields that same transfer's completion intact. **No cancel, no resubmit, no byte
loss.** ✅

---

### Q2 — Queue model: can `next_complete()` be awaited repeatedly across many `select!` iterations after a single `submit`? What is `pending()` between a dropped future and the next await?

**YES — one `submit`, then `next_complete()` may be awaited (and dropped)
arbitrarily many times until it actually yields.** This is the supported, lossless
pattern.

- The `Endpoint` is explicitly "a queue of pending transfers" with submission and
  completion **separated**, and the doc states this separation "makes the API
  cancel-safe" (`device.rs:558-564`).
- `pending()` returns "the number of transfers that have been submitted with
  `submit` that have not yet been returned from `next_complete`"
  (`device.rs:673-676`; impl `self.pending.len()`, `linux .../device.rs:765-767`).
  A dropped `next_complete()` future does not call `poll`-to-Ready and does not pop
  the queue, so **`pending()` is identical before and after the dropped future** —
  it stays `1` (for one in-flight read) across as many `select!` iterations as you
  like, until a `next_complete()` actually resolves and pops it.
- A transfer is removed from the queue ONLY when `take_completed_from_queue` pops
  a *completed* front entry inside `poll_next_complete`
  (`internal.rs:198-206`, `linux device.rs:803-806`). A pending (not-yet-complete)
  front is never popped by polling.
- **Caveat (panic hazard, important for the design):** `next_complete()` /
  `poll_next_complete()` **panic if `pending() == 0`** (`device.rs:781-783`,
  `:793-795`; the `expect("no transfer pending")` is in
  `take_completed_from_queue`, `internal.rs:199`). So a select-based reader MUST
  guarantee a transfer is in flight before it `select!`s on `next_complete()`. The
  natural shape is: submit once, then on each completion re-submit before looping.
  This is exactly nusb's documented "Optimized Streaming" pattern
  (`device.rs:617-651`), which keeps `while ep_in.pending() < N { submit }` and
  re-submits `completion.buffer` each loop. Holding a single in-flight transfer
  across selects is a degenerate (N=1) case of that documented pattern.

Conclusion: **holding one in-flight transfer across a `select!` is a supported,
lossless, intended usage** — provided the reader never awaits `next_complete()`
with an empty queue. ✅

---

### Q3 — Hazard of a select-based design that NEVER calls `cancel`?

The current code's `cancel_all()` + drain (`usb_transport.rs:318-320`) is required
ONLY because it wraps `next_complete()` in `tokio::time::timeout` and must reclaim
the buffer/queue slot when the timer fires. A select-based design that *stops
awaiting and comes back later* (never cancels) AVOIDS that machinery and is free of
the cancel-path hazards:

- **No leaked transfer:** the transfer stays in `pending` and is consumed by a
  later `next_complete()`. If the reader exits entirely, `Drop for LinuxEndpoint`
  calls `cancel_all()` for any still-`pending` transfers (`device.rs:844-855`), so
  even an abrupt exit does not leak at the kernel level.
- **No buffer-ownership problem:** the submitted `Buffer` is owned by the queued
  `Pending`/`Idle` transfer (`submit` moves it in, `device.rs:786-791`); userspace
  does not touch it until the completion is taken. Not awaiting does not transfer
  ownership anywhere.
- **No double-submit:** as long as the reader re-submits ONLY after consuming a
  completion (and respects the `pending() == 0` panic guard from Q2), there is no
  path to submit twice for one in-flight slot.
- **Subtlety vs the existing timeout path:** the *current* code intentionally
  forces `status: Err(TransferError::Cancelled)` and relies on
  `classify_read_completion` to salvage any drained bytes
  (`usb_transport.rs:298-305`, `:524-556`). A never-cancel select design SHEDS
  this whole salvage-vs-timeout dance for the *polling* case, because it never
  manufactures a `Cancelled` status just to poll. (Cancellation would still be
  used for genuine teardown / true I/O timeout if the design keeps one — see the
  feasibility doc; that's a design choice, not a forced hazard.)

So: **no resource/queue hazard from "just stop awaiting and return later,"** given
the `pending()==0` panic guard is honored. ✅

---

### Q4 — Unit of in-flight I/O: single bulk transfer (one `Buffer`) vs the old whole-frame read. Is a chunk-boundary select structurally lossless where a frame-boundary select was not?

**YES — this is the crux, and it holds.**

The unit of in-flight I/O in nusb is exactly **one submitted `Buffer` = one bulk
transfer**. `submit(buf)` enqueues one transfer; `next_complete()` yields that one
transfer's `Completion` (`device.rs:737-768`, `:770-779`). An IN transfer completes
on a short packet or when `requested_len` is reached (`device.rs:775-779`) — i.e. at
a **chunk boundary**, never spanning a logical ADB frame.

Repo grounding that one transfer = one chunk, and a frame = MANY transfers:

- `read_into_buffer` issues exactly ONE bulk-IN transfer per call
  (`usb_transport.rs:558` "Issue ONE bulk IN transfer", `:592-593` one
  `Buffer::new(request_len)` through `transfer_with_timeout`).
- `read_message_with_timeout` loops calling `read_into_buffer` repeatedly until
  `read_buffer.try_parse()` yields a full frame (`usb_transport.rs:491-497`). A
  24-byte header + `data_length` payload therefore spans **many** transfers
  (explicitly noted `usb_transport.rs:566-576` and `persistent.rs:1255-1258`).
- Every received byte is pushed into the persistent `FrameReadBuffer` — including
  bytes from a timed-out transfer (the salvage contract,
  `usb_transport.rs:564-581`, `classify_read_completion` `:541-556`). The buffer
  *retains* partial-frame bytes across calls.

Therefore the cancellation/abandonment unit differs fundamentally between the two
designs:

- **Old frame-boundary select** (`select!(read_whole_frame, control_rx)`):
  dropping the future cancels an in-flight read that may be **mid-frame**, having
  already consumed several transfers' worth of payload that lived only inside the
  read future's transient state. The reverted attempt corrupted a large in-flight
  WRTE — one of two concurrent device→host streams silently stalled at 0 bytes
  (`persistent.rs:1259-1266`). NOT cancel-safe — automatic NO per the task
  constraints.
- **New chunk-boundary select** (`select!(endpoint.next_complete(), control_rx)`):
  the only thing "in flight" is ONE bulk transfer holding ONE `Buffer`. Per Q1/Q2,
  the `select!` does not cancel it — it stays queued and is consumed next
  iteration. Even if it WERE cancelled, every byte already received is salvaged
  into `FrameReadBuffer` (the salvage path already shipped), so a chunk-boundary
  interruption loses nothing. Frame assembly is **outside** the cancelled scope,
  in the persistent buffer.

**Structural conclusion:** a chunk-boundary select is lossless *for two independent
reasons* — (a) `next_complete()` doesn't cancel the held transfer, and (b) even
under cancellation, chunk bytes are retained by `FrameReadBuffer` — whereas the
frame-boundary select was lossless for *neither* (a multi-transfer frame's interior
state was discarded on cancel). ✅

---

## Caveats / Not Found

- **Version:** confirmed against the **locked 0.2.3**, not the 0.2.4 the task hint
  named. API shape matches across both, but a pin bump should re-verify.
- **Panic guard is load-bearing:** `next_complete()`/`poll_next_complete()` panic
  if `pending() == 0` (`device.rs:781-783`, `:793-795`). Any select-based reader
  MUST keep exactly one transfer in flight (submit-then-await, re-submit on
  completion) and must NOT enter the `select!` arm with an empty queue. This is the
  single biggest correctness footgun for the implementation.
- **Cancel-safety of the OTHER select arm** (`control_rx.recv()` on a tokio
  `mpsc::Receiver`) is a tokio property, out of scope for this nusb strand; tokio's
  `mpsc::Receiver::recv` is documented cancel-safe, but the feasibility doc should
  state it explicitly.
- **`next_complete()` borrows `&mut endpoint`** for the duration of the await. The
  reader already holds `&mut read_endpoint` via the split borrow in
  `read_message_with_timeout` (`usb_transport.rs:471-484`); a trait-level
  "await next chunk" primitive must preserve that exclusive borrow shape. Trait
  surface design itself is Q3 of the parent task — covered by the sibling strand,
  not here.
- I did NOT benchmark the 1/sec cancel+resubmit churn (parent Q5b) — out of strand.
- This strand makes **no go/no-go recommendation** and proposes **no trait surface**
  (parent Q3/Q5/Q6 / the synthesis doc owns those). It establishes only the nusb
  primitive's cancel-safety and the chunk-vs-frame structural distinction.
