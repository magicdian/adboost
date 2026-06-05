# Research: AOSP `delayed_ack` flow-control wire semantics (authoritative)

- **Query**: Definitive AOSP `delayed_ack` flow-control wire semantics to ground a Rust windowed-flow-control reimplementation in a fork of `cocool97/adb_client`.
- **Scope**: external (AOSP primary source) — critical path, replacing stop-and-wait with real windowed flow control.
- **Date**: 2026-06-05

## Source provenance (pin this)

All quotes are from `platform/packages/modules/adb` on android.googlesource.com, branch `refs/heads/main`, pinned at the HEAD commit fetched for this research:

- **Commit**: `1cf2f017d312f73b3dc53bda85ef2610e35a80e9` ("Merge ... into main", committed 2025-03-25)
- Base URL pattern: `https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/<file>`
- These delayed-ack semantics are stable; they were introduced years before this commit and are unchanged across recent `android-14`/`android-15` releases. The protocol prose docs (`docs/dev/internals.md`) are high-level and do NOT describe delayed-ack; the **C++ source is the only authoritative spec** for this feature.

> NOTE: There is no `protocol.txt`/`OVERVIEW.TXT` anymore (404 on `main`). The historical `protocol.txt` text never covered delayed_ack. Do not trust secondary blog summaries — several are wrong on cumulative-vs-delta (see TL;DR row 2).

---

## TL;DR — definitive answers

| # | Question | Definitive answer | Source |
|---|---|---|---|
| 1a | Where is the OKAY byte count carried? | In the **payload** (4-byte LE `int32`), NOT in `arg0`/`arg1`. `arg0`/`arg1` remain the local/remote socket IDs. | `adb.cpp:269-282` (send), `adb.cpp:558-570` (recv) |
| 1b | Cumulative or delta? | **DELTA** — bytes acked since the last OKAY. Receiver sends `bytes_flushed` (just-flushed amount); sender does `*available_send_bytes += *acked_bytes`. | `sockets.cpp:150`, `sockets.cpp:434` |
| 1c | Signed? | **Signed `int32_t`; can be negative** (reserved for future preemptive backpressure). | `adb.cpp:559-566`, `socket.h:126-129` |
| 2 | Initial window | `INITIAL_DELAYED_ACK_BYTES = 32 * 1024 * 1024` (**32 MiB**, NOT 1 MiB). Carried in the **OPEN `arg1`** (host→device) and in the responding **OKAY payload** (device→host). | `adb.h:38`, `sockets.cpp:574-577`, `adb.cpp:544` |
| 3 | Feature gate | Both ends must advertise `kFeatureDelayedAck = "delayed_ack"` in CNXN banner features. Per-transport bool `delayed_ack_` set via `CanUseFeature`. | `transport.cpp:99,1232-1235,1260-1271`, `transport.h:363-364,442` |
| 4 | Classic mode | Strict stop-and-wait: WRTE → wait OKAY → next WRTE. Classic OKAY carries **empty payload** (0 bytes); `acked_bytes` is `nullopt`; sender just calls `ready()`. | `adb.cpp:275-279` (no payload when !delayed), `sockets.cpp:438-441` |
| 5 | Constants | `MAX_PAYLOAD_V1 = 4096`, `MAX_PAYLOAD = 1 MiB`, `INITIAL_DELAYED_ACK_BYTES = 32 MiB`. Window (in-flight bytes) is decoupled from per-WRTE chunk size (`get_max_payload()`). | `adb.h:33-38`, `sockets.cpp:982-990` |
| 6 | Overflow | No explicit close on overflow. `available_send_bytes` is `int64_t` and is *allowed* to go ≤ 0 (and negative) for one in-flight max-payload packet; sender then stops reading from fd (`fdevent_del FDE_READ`) until a future OKAY pushes it back > 0. Receiver emits OKAY **eagerly per flush** of incoming data. | `sockets.cpp:213-239`, `sockets.cpp:146-158`, `socket.h:126-129` |

---

## Q1 — A_OKAY byte-count semantics under delayed_ack

### 1a. Location: the PAYLOAD, not arg0/arg1

`amessage` is the 24-byte header; `arg0`/`arg1` are always the socket IDs for OKAY (`READY(local-id, remote-id, "")`). The ack byte count is appended as a 4-byte payload.

`types.h:148-161`:
```cpp
struct amessage {
    uint32_t command;     /* command identifier constant      */
    uint32_t arg0;        /* first argument                   */
    uint32_t arg1;        /* second argument                  */
    uint32_t data_length; /* length of payload (0 is allowed) */
    uint32_t data_check;  /* checksum of data payload         */
    uint32_t magic;       /* command ^ 0xffffffff             */
};
struct apacket {
    using payload_type = Block;
    amessage msg;
    payload_type payload;
};
```

**Send path** — `adb.cpp:269-282` (`send_ready`):
```cpp
void send_ready(unsigned local, unsigned remote, atransport* t, uint32_t ack_bytes) {
    D("Calling send_ready");
    apacket *p = get_apacket();
    p->msg.command = A_OKAY;
    p->msg.arg0 = local;
    p->msg.arg1 = remote;
    if (t->SupportsDelayedAck()) {
        p->msg.data_length = sizeof(ack_bytes);
        p->payload.resize(sizeof(ack_bytes));
        memcpy(p->payload.data(), &ack_bytes, sizeof(ack_bytes));
    }
    send_packet(p, t);
}
```
So under delayed_ack the OKAY carries a 4-byte LE payload; otherwise the payload is empty (`data_length = 0`). `arg0`/`arg1` are unchanged (socket IDs).

**Receive path** — `adb.cpp:554-591` (case `A_OKAY`):
```cpp
case A_OKAY: /* READY(local-id, remote-id, "") */
    if (t->online && p->msg.arg0 != 0 && p->msg.arg1 != 0) {
        asocket* s = find_local_socket(p->msg.arg1, 0);
        if (s) {
            std::optional<int32_t> acked_bytes;
            if (p->payload.size() == sizeof(int32_t)) {
                int32_t value;
                memcpy(&value, p->payload.data(), sizeof(value));
                // acked_bytes can be negative!
                acked_bytes = value;
            } else if (p->payload.size() != 0) {
                LOG(ERROR) << "invalid A_OKAY payload size: " << p->payload.size();
                return;
            }
            if (s->peer == nullptr) {
                s->peer = create_remote_socket(p->msg.arg0, t);
                s->peer->peer = s;
                local_socket_ack(s, acked_bytes);   // first READY also creates the connection
            } else if (s->peer->id == p->msg.arg0) {
                local_socket_ack(s, acked_bytes);
            } else { /* invalid */ }
        } else {
            send_close(p->msg.arg1, p->msg.arg0, t);
        }
    }
    break;
```
Note: the value is read as **`int32_t`** (signed), little-endian host memcpy. Empty payload → `acked_bytes == nullopt` (classic mode). 8-byte or other non-{0,4} sizes are rejected and the packet dropped.

### 1b. DELTA, not cumulative

**Sender accounting** — `sockets.cpp:418-442` (`local_socket_ack`):
```cpp
void local_socket_ack(asocket* s, std::optional<int32_t> acked_bytes) {
    // acked_bytes can be negative!
    if (s->available_send_bytes.has_value() != acked_bytes.has_value()) {
        LOG(ERROR) << "delayed ack mismatch: socket = " << s->available_send_bytes.has_value()
                   << ", payload = " << acked_bytes.has_value();
        return;
    }
    if (s->available_send_bytes.has_value()) {
        D("LS(%d) received delayed ack, available bytes: %" PRId64 " += %" PRIu32, s->id,
          *s->available_send_bytes, *acked_bytes);
        // This can't (reasonably) overflow: available_send_bytes is 64-bit.
        *s->available_send_bytes += *acked_bytes;   // <-- DELTA: add to the window
        if (*s->available_send_bytes > 0) {
            s->ready(s);
        }
    } else {
        D("LS(%d) received ack", s->id);
        s->ready(s);                                // classic mode: just resume
    }
}
```
The `+=` is unambiguous: each OKAY carries the **incremental** number of bytes the receiver just freed, and the sender adds it to its remaining window. It is NOT a cumulative absolute counter.

**Receiver side — what value gets sent** — `sockets.cpp:146-158` (in `local_socket_flush_incoming`, after writing queued data to its fd):
```cpp
bool fd_full = !s->packet_queue.empty() && !s->has_write_error;
if (s->transport && s->peer) {
    if (s->available_send_bytes.has_value()) {
        // Deferred acks are available.
        send_ready(s->id, s->peer->id, s->transport, bytes_flushed);   // <-- DELTA just drained
    } else {
        // Deferred acks aren't available, we should ask for more data as long as we have less
        // than a full packet left in our queue.
        if (bytes_flushed != 0 && s->packet_queue.size() < MAX_PAYLOAD) {
            send_ready(s->id, s->peer->id, s->transport, 0);
        }
    }
}
```
`bytes_flushed` (set earlier from the `adb_write` return, `sockets.cpp:130`) is the number of bytes just written out of the receive queue this flush — i.e. the delta of newly-freed buffer space. (Curiosity: a receiving socket also has `available_send_bytes.has_value()` true on both peers of a delayed-ack transport — the optional presence tracks "delayed acks negotiated", not a direction.)

**Sender debit** — `sockets.cpp:205-240` (in `local_socket_flush_outgoing`, after reading from fd and before enqueueing to the remote peer):
```cpp
if (avail != max_payload && s->peer) {
    data.resize(max_payload - avail);
    ...
    if (s->available_send_bytes) {
        *s->available_send_bytes -= data.size();   // <-- debit window by bytes sent
    }
    r = s->peer->enqueue(s->peer, std::move(data)); // remote_socket_enqueue -> A_WRTE
    ...
    if (r > 0) {
        if (s->available_send_bytes) {
            if (*s->available_send_bytes <= 0) {
                D("LS(%u): send buffer full (%" PRId64 ")", saved_id, *s->available_send_bytes);
                fdevent_del(s->fde, FDE_READ);      // stop reading source until window reopens
            }
        } else {
            D("LS(%u): acks not deferred, blocking", saved_id);
            fdevent_del(s->fde, FDE_READ);          // classic: block after one packet
        }
    }
}
```

### 1c. Signed and may go negative
`socket.h:126-129`:
```cpp
// The number of bytes that have been acknowledged by the other end if delayed_ack is available.
// This value can go negative: if we have a MAX_PAYLOAD's worth of bytes available to send,
// we'll send out a full packet.
std::optional<int64_t> available_send_bytes;
```
The local accumulator is `int64_t`; the wire delta is `int32_t`. The sender keeps sending while `available_send_bytes > 0`; the *last* packet before stopping may push it ≤ 0 (even negative by up to one chunk).

---

## Q2 — Initial window establishment

The initial window is `INITIAL_DELAYED_ACK_BYTES`, **NOT** `A_CNXN.arg1` (maxdata) and not a magic separate value.

`adb.h:33-38`:
```cpp
constexpr size_t MAX_PAYLOAD_V1 = 4 * 1024;
constexpr size_t MAX_PAYLOAD = 1024 * 1024;

// When delayed acks are supported, the initial number of unacknowledged bytes we're willing to
// receive before the other side should block.
constexpr size_t INITIAL_DELAYED_ACK_BYTES = 32 * 1024 * 1024;   // 32 MiB
```

It is communicated **twice**, once per direction, at stream-open time:

1. **Host → device, via OPEN `arg1`** — `sockets.cpp:560-586` (`connect_to_remote`):
```cpp
p->msg.command = A_OPEN;
p->msg.arg0 = s->id;
if (s->transport->SupportsDelayedAck()) {
    p->msg.arg1 = INITIAL_DELAYED_ACK_BYTES;   // tell device how much WE can buffer
    s->available_send_bytes = 0;               // we may not send until device OKAYs
}
```
So the OPEN’s `arg1` carries the opener’s receive-window grant. The opener sets its own send window to **0** and waits for the device’s first OKAY before sending.

2. **Device receives OPEN, grants its own window via the first OKAY payload** — `adb.cpp:500-551` (case `A_OPEN`):
```cpp
uint32_t send_bytes = static_cast<uint32_t>(p->msg.arg1);
if (t->SupportsDelayedAck() != static_cast<bool>(send_bytes)) {
    LOG(ERROR) << "unexpected value of A_OPEN arg1: " << send_bytes
               << " (delayed acks = " << t->SupportsDelayedAck() << ")";
    send_close(0, p->msg.arg0, t);
    break;
}
...
if (t->SupportsDelayedAck()) {
    VLOG(PACKETS) << "delayed ack available: send buffer = " << send_bytes;
    s->available_send_bytes = send_bytes;            // device's send window = opener's grant (arg1)
    // TODO: Make this adjustable at connection time?
    send_ready(s->id, s->peer->id, t, INITIAL_DELAYED_ACK_BYTES);  // device grants its own window
} else {
    VLOG(PACKETS) << "delayed ack unavailable";
    send_ready(s->id, s->peer->id, t, 0);
}
s->ready(s);
```
Key invariant: **OPEN `arg1` must be non-zero iff delayed_ack is negotiated** — a mismatch (`SupportsDelayedAck() != bool(send_bytes)`) is fatal to the stream (`send_close`). The host’s initial OKAY in response (the `send_ready(..., INITIAL_DELAYED_ACK_BYTES)`) is what unblocks the host’s own send side (`available_send_bytes` goes 0 → 32 MiB via the `+=` path).

So the handshake per stream is:
- Host sends `A_OPEN(local_id, 32MiB, "service\0")`, sets its own send window = 0.
- Device sets its send window = 32 MiB (from arg1), replies `A_OKAY(payload=32MiB)`.
- Host’s `local_socket_ack` does `available_send_bytes (0) += 32MiB` → host send window = 32 MiB.

---

## Q3 — The delayed_ack feature gate

Feature string constant — `transport.cpp:99`:
```cpp
const char* const kFeatureDelayedAck = "delayed_ack";
```

Advertisement in the local feature set — `transport.cpp:1230-1238` (`supported_features()`):
```cpp
#if ADB_HOST
    if (burst_mode_enabled()) {
        result.push_back(kFeatureDelayedAck);
    }
#else
    result.push_back(kFeatureDelayedAck);   // adbd always advertises it
#endif
    return result;
```
On the device (adbd) it is always advertised. On the host (adb server) it is advertised only when burst mode is enabled (`ADB_BURST_MODE=1` env var) — `transport.cpp:1189-1195`:
```cpp
#if ADB_HOST
bool burst_mode_enabled() {
    static const char* env = getenv("ADB_BURST_MODE");
    static bool result = env && strcmp(env, "1") == 0;
    return result;
}
#endif
```

Negotiation = intersection. The feature set is parsed from the CNXN banner and `delayed_ack_` is set per-transport — `transport.cpp:1260-1271`:
```cpp
bool CanUseFeature(const FeatureSet& feature_set, const std::string& feature) {
    return contains(feature_set, feature) && contains(supported_features(), feature);
}
...
void atransport::SetFeatures(const std::string& features_string) {
    features_ = StringToFeatureSet(features_string);
    delayed_ack_ = CanUseFeature(features_, kFeatureDelayedAck);
}
```
`CanUseFeature` requires the feature to be present in **both** the peer’s advertised set (`features_`, parsed from the banner) **and** our own `supported_features()`. The accessor — `transport.h:363-364`, backing field `transport.h:442`:
```cpp
bool SupportsDelayedAck() const {
    return delayed_ack_;
}
...
bool delayed_ack_ = false;
```
Features ride in the CNXN banner string (`get_connection_string()` / `parse_banner`), not in a header arg.

---

## Q4 — Classic (non-delayed-ack) mode contrast

Confirmed strict stop-and-wait, and OKAY carries **no byte count** (empty payload).

- **Send**: `send_ready` only appends the payload `if (t->SupportsDelayedAck())` (`adb.cpp:275-279`). Classic OKAY → `data_length = 0`, empty payload.
- **Receive**: empty payload → `acked_bytes == nullopt` → `local_socket_ack` takes the else branch (`sockets.cpp:438-441`): just `s->ready(s)`, no window math.
- **Sender blocks after one packet**: `sockets.cpp:236-239`:
  ```cpp
  } else {
      D("LS(%u): acks not deferred, blocking", saved_id);
      fdevent_del(s->fde, FDE_READ);   // stop reading source fd until the single OKAY arrives
  }
  ```
- **Receiver requests more conservatively** — `sockets.cpp:151-156`: in classic mode it only sends OKAY when it actually flushed data AND its queue is below one MAX_PAYLOAD, i.e. it asks for the next packet once it has room.

So in classic mode: send one WRTE (≤ get_max_payload), stop reading the source, wait for the (payload-less) OKAY, resume. Exactly one packet in flight.

---

## Q5 — MAX_PAYLOAD / window sizing and decoupling

Constants (`adb.h:33-38`):
- `MAX_PAYLOAD_V1 = 4 * 1024` (4096) — pre-negotiation cap; the CNXN/AUTH banner itself is limited to this (`adb.cpp:338-341`).
- `MAX_PAYLOAD = 1024 * 1024` (1 MiB) — max bytes in a single WRTE payload.
- `INITIAL_DELAYED_ACK_BYTES = 32 * 1024 * 1024` (32 MiB) — initial in-flight window.

Per-WRTE chunk size is `asocket::get_max_payload()` — `sockets.cpp:982-990`:
```cpp
size_t asocket::get_max_payload() const {
    size_t max_payload = MAX_PAYLOAD;
    if (transport) {
        max_payload = std::min(max_payload, transport->get_max_payload());
    }
    if (peer && peer->transport) {
        max_payload = std::min(max_payload, peer->transport->get_max_payload());
    }
    return max_payload;
}
```
Transport max_payload is negotiated from CNXN `arg1` (maxdata), clamped to MAX_PAYLOAD — `transport.cpp:1172-1175`:
```cpp
void atransport::update_version(int version, size_t payload) {
    protocol_version = std::min(version, A_VERSION);
    max_payload = std::min(payload, MAX_PAYLOAD);
}
```
CNXN sends `arg1 = t->get_max_payload()` (`adb.cpp:335`); default `max_payload = MAX_PAYLOAD` (`transport.h:284`). Outgoing WRTE refuses payloads > MAX_PAYLOAD (`sockets.cpp:495-498`), and the read path rejects `data_length > MAX_PAYLOAD` / `> get_max_payload()` (`transport.cpp:482-485`, `1699-1703`).

**Decoupling**: in classic mode, in-flight bytes ≤ one `get_max_payload()` chunk (stop-and-wait). With delayed_ack, in-flight bytes are governed by `available_send_bytes` (up to 32 MiB initially), entirely independent of the per-packet `get_max_payload()` (≤ 1 MiB). The sender keeps reading source data and emitting back-to-back WRTEs (each ≤ get_max_payload) as long as the running window stays > 0, debiting `available_send_bytes` by each WRTE’s size and crediting it back by each OKAY delta.

---

## Q6 — Overflow / edge cases

- **Overflow handling = backpressure, not close.** There is no "sender exceeded window → close" path. `available_send_bytes` is `int64_t` and deliberately allowed to reach ≤ 0 (and slightly negative — one chunk’s worth). When it hits ≤ 0 after a send, the sender stops reading its source fd (`fdevent_del(s->fde, FDE_READ)`, `sockets.cpp:232-234`) and only resumes when a future OKAY pushes the window back > 0 (`local_socket_ack` → `s->ready(s)` only `if (*s->available_send_bytes > 0)`, `sockets.cpp:435-436`). So a well-behaved sender never overflows the receiver because it self-throttles before the next read.
- **What IS fatal**: a delayed_ack/payload presence mismatch. (a) OPEN with `arg1 == 0` while delayed_ack negotiated (or non-zero while not) → `send_close` (`adb.cpp:506-512`). (b) An OKAY whose payload size is neither 0 nor 4 → logged error and the whole `handle_packet` returns/drops (`adb.cpp:567-569`). (c) A socket/payload `has_value()` mismatch in `local_socket_ack` → logged error, ack ignored (`sockets.cpp:423-426`). (d) WRTE/read with `data_length > MAX_PAYLOAD` → connection read fails (`transport.cpp:482-485`).
- **Receiver’s OKAY obligation = eager, per flush of incoming data.** Every time `local_socket_flush_incoming` drains some queued bytes to the destination fd, if delayed_ack is on it immediately sends `A_OKAY(payload = bytes_flushed)` (`sockets.cpp:148-150`). It is NOT batched to a low-watermark; it’s emitted opportunistically each time buffer space frees up (`bytes_flushed` may be 0, in which case a 0-delta OKAY can still be sent under delayed_ack — harmless: `+= 0`). In classic mode the OKAY is gated on `bytes_flushed != 0 && queue < MAX_PAYLOAD`.

---

## Related internal research

- `.trellis/tasks/.../research/03-adb-protocol-truth.md` — general ADB protocol message framing.
- `.trellis/tasks/.../research/05-forward-and-async-facts.md`, `06-forward-async-synthesis.md` — async/device-originated-open context that this flow-control work plugs into.

---

## Rust implementation implications

Map directly to the AOSP model. Suggested per-stream state:

```text
struct FlowControl {
    // None  => classic stop-and-wait (delayed_ack NOT negotiated)
    // Some  => delayed_ack negotiated; tracks remaining send window in bytes.
    available_bytes: Option<i64>,   // mirror of asocket::available_send_bytes (int64, may go <= 0)
}
```

The task brief proposed `{ available_bytes, bytes_sent, bytes_acked }`. AOSP does NOT keep separate cumulative `bytes_sent`/`bytes_acked` counters — it keeps a single signed running window and applies deltas. You can keep `bytes_sent`/`bytes_acked` as observability/debug counters, but the load-bearing field is the single `available_bytes` accumulator. Do not treat the wire value as cumulative.

Concrete wiring (match these exactly):

1. **Negotiation**: include `"delayed_ack"` in your CNXN banner `features=` list. Enable windowing only if the peer’s banner also lists it (intersection). Constant string: `delayed_ack`.
2. **Header vs payload for OKAY**:
   - Read/write the ack count in the **WRTE/OKAY payload** as a **4-byte little-endian `i32`** (`data_length = 4`). `arg0`/`arg1` stay as `(local_id, remote_id)` exactly as in classic OKAY.
   - Classic OKAY: empty payload (`data_length = 0`); do not append bytes.
3. **Stream open (when you originate OPEN)**: set OPEN `arg1 = INITIAL_DELAYED_ACK_BYTES = 0x0200_0000` (32 MiB) when delayed_ack is on, else `arg1 = 0`. Initialize your own `available_bytes = Some(0)` and do not send stream data until the first OKAY credits the window.
4. **Stream open (when you receive OPEN / device-originated)**: validate `(arg1 != 0) == delayed_ack_negotiated` (else close the stream). Set your send window `available_bytes = Some(arg1)`. Reply with `OKAY(payload = INITIAL_DELAYED_ACK_BYTES)` to grant your own receive window.
5. **Sending data**: while `available_bytes > 0`, read up to `max_payload` (negotiated maxdata, clamped to `MAX_PAYLOAD` = 1 MiB) and send a WRTE; debit `available_bytes -= chunk_len`. When `available_bytes <= 0`, pause reading the source until an incoming OKAY credits it.
6. **Receiving an OKAY**: parse the `i32` delta from payload (treat empty payload as classic/`None`); `available_bytes += delta` (delta is **signed**, may be negative); if `available_bytes > 0`, resume sending. Reject OKAY payloads whose length is not 0 or 4.
7. **Receiving data + emitting OKAY (your receive side)**: each time you flush received bytes to the consumer, emit `OKAY(payload = bytes_just_flushed_as_i32_le)` eagerly. Under delayed_ack, even a 0-delta is acceptable. In classic mode, send the (empty) OKAY only after consuming the single in-flight WRTE.
8. **Constants to define**: `MAX_PAYLOAD_V1 = 4096`, `MAX_PAYLOAD = 1 << 20` (1 MiB), `INITIAL_DELAYED_ACK_BYTES = 32 << 20` (32 MiB).

## Caveats / not confirmed

- **1 MiB initial-window claim is WRONG.** Some secondary sources say the initial window is 1 MiB; the AOSP constant is **32 MiB** (`INITIAL_DELAYED_ACK_BYTES = 32 * 1024 * 1024`). 1 MiB is `MAX_PAYLOAD` (the per-packet cap), a different thing. This is the most likely source of the earlier self-contradiction.
- **Cumulative-vs-delta is settled: it is DELTA** (`+=` in both `local_socket_ack` and the `send_ready(bytes_flushed)` call). Any source claiming the OKAY carries a cumulative total is wrong.
- **There is no AOSP prose spec for delayed_ack.** `docs/dev/internals.md` and the old `protocol.txt` do not describe it; the only authoritative reference is the C++ source quoted above. Treat the code as ground truth.
- Host-side adb advertises delayed_ack only under `ADB_BURST_MODE=1` on the version pinned here; adbd always advertises it. If you are the host in this fork, you must advertise it unconditionally (you control your own banner) and rely on the device always supporting it — confirm against the specific device’s banner at runtime rather than assuming.
- The `TODO: Make this adjustable at connection time?` (`adb.cpp:543`) means the 32 MiB grant is currently a hard constant in AOSP; it is not negotiated down. Interop only requires you to honor the peer’s advertised grant (their OPEN arg1 / OKAY payload), not to match 32 MiB yourself.
