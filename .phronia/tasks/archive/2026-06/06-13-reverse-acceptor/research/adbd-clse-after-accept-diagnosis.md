# Research: Why adbd sends A_CLSE immediately after our acceptor A_OKAY

- **Query**: A real adbd opens a reverse `A_OPEN(arg0=388, arg1=0, "tcp:55992\0")`; our
  acceptor replies `A_OKAY(arg0=my_id=1306676300, arg1=388, empty payload)`; adbd
  then sends `A_CLSE(arg0=388, arg1=1306676300)` ~0.8 ms later, before any WRTE. Why?
- **Scope**: external (AOSP `platform/packages/modules/adb` branch `main`, fetched
  2026-06-13) + internal (our acceptor code)
- **Date**: 2026-06-13

## TL;DR (the answer)

**Your A_OKAY is correct. The CLSE is NOT a rejection — it is adbd's NORMAL close.**

- A rejection close is `A_CLSE(arg0 = 0, arg1 = device_id)` — see `send_close(0, p->msg.arg0, t)`
  at `adb.cpp:510` and `:532`. **arg0 == 0** is the "failed OPEN" marker.
- Your observed close is `A_CLSE(arg0 = 388, arg1 = 1306676300)` — **arg0 = adbd's own
  socket id (388), non-zero**. That is what `send_close(p->msg.arg1, p->msg.arg0, t)` /
  `s->peer->close(s->peer)` emits for a socket that was **fully linked and then torn down
  for an ordinary reason** (peer EOF / its own fd hit EOF), NOT a handshake rejection.

So adbd **accepted** your OKAY, linked its peer to your `local_id`, and then closed the
stream the normal way. The protocol handshake passed. The close is downstream of accept —
almost certainly the device-side connection (the `nc 127.0.0.1 <device_port>` socket adbd
accepted) reached EOF/closed and adbd propagated that close. The fix is NOT in the OKAY arg
order; it is in keeping the bridge alive / not tearing the session down on our side, and
in how we treat the device's TCP fd lifecycle.

---

## Citations — exact AOSP code paths

All line numbers from the `main` snapshot fetched 2026-06-13 (`adb.cpp` 1661 lines,
`sockets.cpp` 991 lines).

### Q1 + Q2 + Q3 — adbd's `case A_OKAY` when it receives OUR OKAY

`adb.cpp:554-592`:

```cpp
case A_OKAY: /* READY(local-id, remote-id, "") */
    if (t->online && p->msg.arg0 != 0 && p->msg.arg1 != 0) {
        asocket* s = find_local_socket(p->msg.arg1, 0);          // (A) find by arg1
        if (s) {
            std::optional<int32_t> acked_bytes;
            if (p->payload.size() == sizeof(int32_t)) {           // 4-byte delayed-ack delta
                int32_t value; memcpy(&value, p->payload.data(), 4);
                acked_bytes = value;                              // may be negative
            } else if (p->payload.size() != 0) {                  // any OTHER size = error
                LOG(ERROR) << "invalid A_OKAY payload size: " << p->payload.size();
                return;                                           // dropped (no close)
            }                                                     // size 0 => acked_bytes = nullopt

            if (s->peer == nullptr) {                             // (B) first READY
                s->peer = create_remote_socket(p->msg.arg0, t);   //     remote-id = OUR arg0
                s->peer->peer = s;
                local_socket_ack(s, acked_bytes);
            } else if (s->peer->id == p->msg.arg0) {              // subsequent READYs
                local_socket_ack(s, acked_bytes);
            } else {
                D("Invalid A_OKAY(%d,%d) ...");                   // mismatched id => ignored
            }
        } else {
            // socket not found: host closed it -> tell device to close too
            send_close(p->msg.arg1, p->msg.arg0, t);              // arg0 = OUR id (NON-ZERO)
        }
    }
    break;
```

Answers:

1. **How adbd finds the socket**: `find_local_socket(p->msg.arg1, 0)` — **by arg1**
   (`p->msg.arg1` = adbd's own local id = **388**, the value it put in the OPEN's arg0).
   The `0` second arg means "don't also match by peer id." So your OKAY's **arg1 MUST equal
   the device's OPEN arg0 (388)** — which yours does. Correct.

2. **Arg order adbd expects** = exactly what `send_ready(local, remote, ...)` emits
   (`adb.cpp:269-282`): `arg0 = local (= sender's own id)`, `arg1 = remote (= peer's id)`.
   For the acceptor that is `arg0 = our_local_id`, `arg1 = device_id`. **Your
   `A_OKAY(arg0=1306676300, arg1=388)` is in the correct order.** Not backwards.

3. **First-READY peer creation & id validation**: yes — `s->peer == nullptr` on the first
   OKAY, so adbd runs `s->peer = create_remote_socket(p->msg.arg0, t)` with
   `p->msg.arg0 = OUR id = 1306676300`. **No range / sign validation on this id.**
   `create_remote_socket` (`sockets.cpp:544-558`) only rejects **id == 0**
   (`if (id == 0) LOG(FATAL)`). The id is `unsigned`; 1306676300 < 2^31 anyway, so even a
   signed misread would be harmless. **Your large random local_id is NOT the problem.**

   - **If your OKAY's arg1 had been wrong** (no socket with that id), adbd would have
     taken the `else` branch and sent `send_close(p->msg.arg1, p->msg.arg0, t)` =
     `A_CLSE(arg0 = your_arg1, arg1 = your_arg0)`. That close carries **arg0 = the value
     you sent as arg1**. Your observed CLSE has `arg0 = 388`, `arg1 = 1306676300` — which
     is `A_CLSE(arg0 = adbd_id, arg1 = our_id)`, i.e. the *peer-close* shape, **not** the
     `find_local_socket`-miss shape (that would be `arg0 = 388, arg1 = 1306676300` only if
     you'd sent `arg0=388,arg1=1306676300` in the OKAY — you sent the opposite). So this is
     not the lookup-miss path; the socket WAS found and linked.

### Q4 — delayed-ack / per-stream window mismatch (ruled out for your trace)

`local_socket_ack` (`sockets.cpp:418-442`) guards a delayed-ack mismatch:

```cpp
void local_socket_ack(asocket* s, std::optional<int32_t> acked_bytes) {
    if (s->available_send_bytes.has_value() != acked_bytes.has_value()) {
        LOG(ERROR) << "delayed ack mismatch: ...";
        return;                                  // NB: just returns, does NOT close here
    }
    if (s->available_send_bytes.has_value()) { *s->available_send_bytes += *acked_bytes;
        if (*s->available_send_bytes > 0) s->ready(s); }
    else { s->ready(s); }                        // classic: just become readable
}
```

On the **device-opener** side, `s->available_send_bytes` is set to a value **only if**
`s->transport->SupportsDelayedAck()` was true when it sent the OPEN
(`connect_to_remote`, `sockets.cpp:574-577`: `arg1 = INITIAL_DELAYED_ACK_BYTES;
s->available_send_bytes = 0;`). Your OPEN had **arg1 = 0**, which means this transport's
`SupportsDelayedAck()` is **false** → device `available_send_bytes = nullopt` (classic).
Your reply carries an **empty payload** → `acked_bytes = nullopt` → `has_value()` matches →
**no mismatch**. So the delayed-ack guard does NOT fire and is NOT your cause.

(Also note: even if it *had* mismatched, `local_socket_ack` only `return`s — it does not
send a CLSE. And `send_ready`'s payload presence is gated on the connection-level
`t->SupportsDelayedAck()` at `adb.cpp:275`, consistent with arg1=0 ⇒ empty payload, which
is what your acceptor already does via `windowed = open_msg.arg1 != 0`.)

There is **no MAX_PAYLOAD / version renegotiation per stream**; payload size is the
connection's `get_max_payload()`. Nothing stream-specific differs in the handshake for a
device-initiated (reverse) OPEN vs a host-initiated one — both use the same
`A_OPEN → A_OKAY → (peer linked) → A_WRTE/A_OKAY` machinery. The only asymmetry is the
*source* of the initial send window (opener: peer's first OKAY payload; acceptor: OPEN
arg1), already handled in `accept_device_open`.

### Q5 — does the opener expect a SECOND okay / anything before delivering data?

No. After adbd processes our first A_OKAY it calls `local_socket_ack(s, ...)` →
`s->ready(s)` = `local_socket_ready` (`sockets.cpp:270-274`) which just
`fdevent_add(s->fde, FDE_READ)` — i.e. it starts **reading from its device-side fd** and
will `enqueue`/WRTE whatever it reads toward us. There is **no second OKAY** required from
the acceptor before data flows. A single correctly-addressed OKAY is the whole accept.

### What actually emits `A_CLSE(arg0=388, arg1=our_id)` — the normal close

The non-zero-arg0 close is produced by the standard teardown chain. When the device-side
local socket `s` (id 388, peer = remote socket whose id = our 1306676300) closes,
`local_socket_close` (`sockets.cpp:344-380`) runs:

```cpp
static void local_socket_close(asocket* s) {
    if (s->peer) {
        if (s->peer->shutdown) s->peer->shutdown(s->peer);
        s->peer->peer = nullptr;
        s->peer->close(s->peer);          // remote_socket_close => sends A_CLSE to us
        s->peer = nullptr;
    }
    ...
}
```

`s->peer->close` is `remote_socket_close`, which sends `A_CLSE(arg0 = s->id (=388),
arg1 = peer id (=our 1306676300))`. **That is exactly your observed packet.** It means
adbd's *local* socket 388 closed and propagated the close to its remote peer (us).

Why would socket 388 close ~0.8 ms after becoming ready, before reading any data? Because
adbd's local socket 388 is wired to the **TCP connection it `accept()`ed on the device**
(the `nc 127.0.0.1 <device_port>` connection, via `local_socket_ready` →
`local_socket_event_func` → `local_socket_flush_incoming/outgoing`). When that device-side
fd hits **EOF or error** — e.g. `nc` connected and immediately closed its write side, or
the connection was reset — `local_socket_flush_*`/event handling calls `s->close(s)` and
the CLSE is forwarded to us. This is ordinary EOF propagation, NOT a protocol rejection.

---

## Internal cross-reference — our acceptor is protocol-correct

`adb_client/src/message_devices/usb/persistent.rs::accept_device_open` (lines 1124-1199):

- `remote_id = open_msg.header().arg0()` (= 388) ✓
- `windowed = open_msg.header().arg1() != 0` → for arg1=0 ⇒ `false` (classic) ✓
- replies `ADBTransportMessage::try_new(Okay, local_id, remote_id, &ready_payload)` with
  `local_id` = our `rng.random::<u32>()` (= 1306676300) as **arg0** and `remote_id` (388)
  as **arg1** ✓ — matches `send_ready(local, remote)` order.
- `encode_okay_payload(windowed=false, ...)` ⇒ **empty payload** (`flow_control.rs:162-169`)
  ✓ — matches adbd's empty-payload expectation for a classic stream.
- Registers the session in the reader map **before** sending the OKAY (lines 1152-1158) so
  the device's follow-up WRTE (targeting arg1 = our local_id) routes to the session ✓.

So none of the OKAY fields are wrong. The bug the trace shows is **after** a successful
accept.

### The likely real causes on OUR side (where to look next — not part of this research's
remit to fix, just where the CLSE-after-accept originates)

1. **We drop the `MultiplexedSession` before/while bridging.** In
   `adb_client/src/server/reverse.rs::run_reverse_pump` (lines 139-165): after
   `accept_device_open`, if `parse_tcp_target` returns `None`, or the dial path drops the
   session, the session `Drop` enqueues a CLSE to the device — but that close would be
   *initiated by us* (we'd send the CLSE), not received from adbd. Your trace is the
   device sending CLSE to us, so this is not it unless our CLSE crossed on the wire.
2. **Device-side EOF is real and expected for `nc` patterns.** `echo MARKER | nc 127.0.0.1
   <port>` (selftest `reverse_echo`, `reverse_cases.rs:86`) closes `nc`'s **write** side
   right after sending the marker (stdin EOF). Depending on the `nc` build, the whole
   connection may close before the host echo round-trips, so adbd legitimately closes
   socket 388 and CLSEs us. If our bridge has already forwarded the marker WRTE and read
   the echo back, an early device CLSE is harmless; if the CLSE arrives *before any WRTE*
   (as in your trace) it suggests the device fd closed before adbd read anything to send —
   i.e. our OKAY → adbd `ready` → adbd read its fd → got 0 bytes / EOF immediately.
3. **Timing**: the 0.8 ms gap with zero WRTE strongly indicates adbd's local fd was
   already at EOF when `s->ready(s)` armed FDE_READ. That points at the **device-side
   connection lifecycle** (how/when the device app connects and closes), not at our OKAY.

> Net: the OKAY handshake is accepted. To stop the immediate teardown, investigate the
> device-side connection (is the device app closing immediately? is `nc` half-closing?)
> and ensure our bridge starts reading/forwarding before the device fd EOFs — but the
> A_OKAY itself needs no change.

---

## Caveats / Not Found

- I could not capture adbd's own `local_socket_event_func` debug log from your device, so
  the *precise* trigger (EOF vs RST vs `has_write_error`) on socket 388 is inferred from the
  close shape (`arg0 != 0`) plus the code paths, not directly observed. To confirm, run
  adbd with `ADB_TRACE` and look for `LS(388): closed` / `local_socket_flush_incoming rc=0`.
- Line numbers are from the `main` branch on 2026-06-13 and may drift; the `A_OKAY`/
  `A_CLSE`/`send_close`/`local_socket_ack`/`local_socket_close` logic has been stable since
  Android 12.
- One thing NOT ruled out by code alone: if your transport ACTUALLY negotiated delayed_ack
  at the connection level but the device still sent `arg1=0` on this particular OPEN, then
  adbd's `send_ready` (gated on connection-level `SupportsDelayedAck()`, `adb.cpp:275`)
  would put a 4-byte payload on ITS OKAYs while expecting your stream to be classic — but
  that asymmetry would surface as a `local_socket_ack` "delayed ack mismatch" log on a
  later WRTE/OKAY, and would only `return` (not CLSE). It cannot produce the immediate
  arg0=388 close. Verify your connection-level `delayed_ack_negotiated` matches what the
  banner advertised if you want to fully exclude it.
