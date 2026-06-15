# ADB Wire Protocol Contract (CNXN version / `delayed_ack` / `data_check`)

> Executable contract for the AOSP `adb` wire protocol fields that adboost must
> get right. Forked from `cocool97/adb_client` v3.2.2. This document exists
> because **two consecutive downstream regressions** came from getting the
> coupling between the CNXN protocol version, the `delayed_ack` feature, and the
> per-message `data_check` (crc) field wrong. AOSP couples all three; changing
> one without the others breaks real devices.

---

## The coupling (read this before touching CNXN, feature negotiation, or integrity)

Three AOSP `apacket` concerns are coupled by the **negotiated protocol version**:

| AOSP version constant | value | meaning |
|---|---|---|
| `A_VERSION_MIN` / legacy | `0x0100_0000` | classic stop-and-wait; no windowed flow control |
| `A_VERSION_SKIP_CHECKSUM` (= `A_VERSION`) | `0x0100_0001` | windowed `delayed_ack` flow control **and** `data_check` is sent as `0` |

Negotiated version = `min(local_advertised, peer_arg0)` (AOSP `atransport::update_version`).
A peer can down-negotiate by replying with a lower `arg0`.

**The contract, stated three ways:**

1. **`delayed_ack` ⇒ version ≥ `0x0100_0001`.** Windowed `OPEN` (non-zero `arg1`
   receive-window grant) is only honored at `>= A_VERSION_SKIP_CHECKSUM`.
   Advertising `delayed_ack` while connecting at legacy version → adbd ignores
   the windowed `OPEN`, never sends `OKAY` → `open_session` times out (bug #1).

2. **version ≥ `0x0100_0001` ⇒ `data_check` is `0` on the wire.** From
   `A_VERSION_SKIP_CHECKSUM` onward, *both ends* send the header `data_check`
   (crc) field as `0`. A receiver that recomputes and compares crc will reject
   **every payload-bearing frame** from such a peer (bug #2 — CNXN banner is the
   first casualty; live-session WRTE / windowed OKAY would be next).

3. **`data_check` is NEVER validated on receive, in any version.** AOSP's
   `check_header` validates only `magic` (`command ^ 0xffffffff`) and
   `data_length`. The "crc32" is a vestigial unsigned byte-sum
   (`calculate_apacket_checksum`), redundant with USB hardware CRC16 / TCP
   checksums. adboost matches this: **magic-only** integrity on receive.

---

## Signatures / current implementation

- CNXN version selection — `usb/persistent.rs` `do_connect`:
  ```rust
  let cnxn_version = if features.delayed_ack {
      A_VERSION_SKIP_CHECKSUM   // 0x0100_0001
  } else {
      A_VERSION_LEGACY          // 0x0100_0000
  };
  ```
- `delayed_ack` negotiation gate — `usb/persistent.rs` `negotiate_delayed_ack`:
  ```rust
  fn negotiate_delayed_ack(local_delayed_ack: bool, device_banner: &str, device_version: u32) -> bool {
      local_delayed_ack
          && banner_advertises_delayed_ack(device_banner)
          && device_version >= A_VERSION_SKIP_CHECKSUM   // device_version = CNXN reply arg0
  }
  ```
- Receive integrity — `adb_transport_message.rs`:
  ```rust
  pub fn check_message_integrity(&self) -> bool {
      ADBTransportMessageHeader::compute_magic(self.header.command) == self.header.magic
      // NO data_crc32 comparison — AOSP never validates data_check on receive.
  }
  ```
  Called for **every** received frame (payload-bearing AND zero-payload) in both
  `usb/usb_transport.rs` and `tcp/tcp_transport.rs` read paths. On failure:
  `RustADBError::InvalidIntegrity(expected_magic, got_magic)`.

---

## Validation & Error Matrix

| Condition on receive | Result |
|---|---|
| `magic == command ^ 0xffffffff` | accept (regardless of `data_check`) |
| `magic` mismatch | `InvalidIntegrity(expected_magic, got_magic)` |
| `data_check == 0`, magic OK, non-empty payload (skip-checksum peer) | **accept** (this is the bug #2 lock) |
| `data_check` wrong, magic OK | accept (crc not consulted) |
| zero-payload frame, magic OK | accept (still magic-checked — no bypass) |

| Connect-time condition | Required behavior |
|---|---|
| `features.delayed_ack == true` | send CNXN at `0x0100_0001` |
| `features.delayed_ack == false` | send CNXN at `0x0100_0000` |
| device CNXN reply `arg0 < 0x0100_0001` | `delayed_ack_negotiated = false`, `OPEN arg1 = 0` |

---

## Good / Base / Bad cases

- **Good**: Android 16 (banner has `delayed_ack`) → CNXN `0x0100_0001`, negotiate
  windowing, accept its `data_check=0` banner + WRTE frames, `open_session` OK.
- **Base**: Android 11 (no `delayed_ack` in banner) → CNXN `0x0100_0001` sent,
  device replies legacy or without feature → `delayed_ack_negotiated=false`,
  classic stop-and-wait, `OPEN arg1=0`. Works.
- **Bad (the two regressions)**:
  - Advertise `delayed_ack` at legacy `0x0100_0000` → windowed OPEN ignored → OKAY timeout.
  - Bump to `0x0100_0001` but keep crc comparison on receive → `Invalid integrity ... got 0`.

---

## Tests Required (assertion points)

- `negotiate_delayed_ack` (`usb/persistent.rs`): true only when local+banner+`version >= 0x0100_0001`; the legacy-version-with-feature case → false (bug #1 lock).
- `check_message_integrity` (`adb_transport_message.rs`): non-empty payload + `data_crc32=0` + correct magic → **true** (bug #2 lock); wrong magic → false; wrong crc + correct magic → true; zero-payload + correct magic → true.

---

## Wrong vs Correct

### Wrong
```rust
// (1) feature without version — windowed OPEN ignored, OKAY timeout
let cnxn = msg(Cnxn, 0x0100_0000, ...); // but banner advertises delayed_ack
// (2) version without skipping crc on receive — every payload frame rejected
fn check(&self) -> bool { magic_ok && compute_crc32(payload) == data_crc32 }
```

### Correct
```rust
// version agrees with feature
let v = if features.delayed_ack { 0x0100_0001 } else { 0x0100_0000 };
// receive validates magic only (AOSP-faithful); crc is vestigial / sent as 0
fn check(&self) -> bool { compute_magic(command) == magic }
```

---

## Out of scope / deliberate non-changes

- **Send side still populates `data_check`** via `compute_crc32` in `try_new`.
  Harmless: AOSP receivers ignore the field. Zeroing it at skip-version is
  cosmetic, not required.
- **`adb_message_device.rs`** (non-persistent device) hardcodes legacy
  `0x0100_0000` and advertises no features — correct, leave as-is. If anyone
  bumps it to `0x0100_0001`, the magic-only receive check already covers it.
- Do **not** replace the byte-sum `compute_crc32` with a real CRC32 — would
  diverge from AOSP and break the legacy byte-sum interop on the send side.

---

## `check_header` is TWO clauses: magic **and** `data_length <= MAX_PAYLOAD`

AOSP `transport.cpp::check_header` validates magic AND rejects
`data_length > max_payload` BEFORE reading the payload. adboost must do both. The
receive path reads a fixed 24-byte header, then **must bound `data_length` before
allocating** the payload buffer, then reads exactly `data_length` bytes, then
checks magic.

- `MAX_PAYLOAD` (1 MiB) lives in `adb_transport_message.rs` (always compiled; the
  TCP path can't see the `usb` module without the `usb` feature). `usb/flow_control.rs`
  re-exports it. Bound via the pure `payload_len_within_bound(data_length)` shared by
  both USB and TCP read paths.
- **Why**: `data_length` is an attacker/corruption-controlled `u32` (up to ~4 GiB).
  Allocating `vec![0; data_length]` from it without a bound is an OOM/DoS
  (`usb_transport.rs` / `tcp_transport.rs`).

## CNXN banner payload must NOT be NUL-terminated

The CNXN banner sent to adbd (`host::features=<csv>`) must contain **no trailing
`\0`** in the payload. AOSP `adb.cpp send_connect` appends no NUL, and adbd's
`StringToFeatureSet` splits the `features=` value on `,` **without trimming**. A
trailing NUL therefore corrupts the **last** CSV feature token (`delayed_ack\0` !=
`delayed_ack`) → adbd's `SupportsDelayedAck()` is false → it rejects our windowed
`OPEN(arg1=32MiB)` with `A_CLSE(0, local_id)`. This was bug #3's TRUE root cause
(device-confirmed on Android 16: removing the single NUL flips windowed OPEN from
rejected → accepted with a 4-byte windowed OKAY grant).

- `to_banner_string()` → `format!("host::features={}", csv)` — no `\0`.
- The corruption only hit the LAST feature, so `shell_v2` (first) always worked and
  masked the bug until #1/#2 let the handshake reach the OPEN.
- adbd never trims/strips the CNXN banner (only the A_OPEN address gets
  `StripTrailingNulls`). Any trailing junk on the banner's last feature is fatal.
- Regression-locked: a test asserts the banner does not end in `\0` and the last
  CSV token is exactly `delayed_ack`.
- `adb_message_device.rs` legacy banner `host::{pkg}\0` is a DIFFERENT, feature-less
  banner (no `features=`); its NUL corrupts no feature token. If it ever advertises
  features it would need the same no-NUL fix.

## CLSE-rejection routing: `open_session` must race ack **and** data channels

On OPEN rejection AOSP adbd sends `A_CLSE(arg0=0, arg1=host_local_id)`
(`adb.cpp: send_close(0, p->msg.arg0, t)`). The reader's `classify_message` keys on
`arg1`, finds the registered session, and routes any non-OKAY (incl. this CLSE) to
the **data** channel. So `open_session` must `tokio::select!` (biased toward OKAY)
over BOTH `ack_rx` (OKAY → proceed) and `data_rx` (early CLSE → fail fast with
`"OPEN rejected by device (CLSE)"`). Waiting only on `ack_rx` → silent 10 s timeout
(bug #3). Recognize rejection by `Clse`-on-data-channel; do **not** require a
specific `arg0` (it's 0).

## Reader resync invariant: only post-payload-read errors may be skipped

The persistent reader loop must NOT tear down all sessions on a single bad frame —
but it may only `continue` (drop-frame-and-keep-serving) for errors that leave the
stream **frame-aligned**. The framing rule, from the read order
(header decode → `data_length` bound → payload read → magic check):

- `InvalidIntegrity` (bad magic): raised **after** the full payload is read →
  stream aligned → **recoverable** (`continue`).
- `ConversionError` (unknown command in header decode) and the oversize-`data_length`
  bound error: raised **before** the payload is read → the `data_length` payload
  bytes are still pending on the wire → skipping desyncs the next header read →
  **fatal** (`break`).
- All IO / disconnect errors: **fatal**.

> **Gotcha**: "it's just a malformed frame, skip it" is only safe if the entire
> frame (header + `data_length` payload) has already been consumed. A pre-payload
> error that you `continue` past leaves orphaned payload bytes that desync every
> subsequent frame. When unsure an error preserves framing, keep it fatal.

## read_exact must CARRY over-read bytes, never discard or fatal

`read_exact` must copy out exactly what the current frame field needs and **stash
any overshoot** in a persistent residual buffer (`Connection::read_residual`),
consuming it first on the next read. It must NOT discard the extra bytes and must
NOT raise a fatal "frame desync" error on over-read.

A single bulk IN completion can legitimately return MORE bytes than the field we
logically requested: `nusb` requires the requested length to be a nonzero multiple
of `max_packet_size`, so a 24-byte header read requests a full `max_packet_size`
buffer, and under sustained **device→host bulk throughput (the `reverse` data
plane)** the device/host controller coalesces a frame's payload tail and the next
frame's header (or the start of the next payload) into one transfer. The earlier
assumption that "AOSP adbd writes header and payload as separate bulk writes so a
compliant device never over-delivers" was WRONG on the IN path: the over-read
happened on a real Android 16 device the instant the first large reverse WRTE
arrived, and the fatal-error guard tore down the **whole multiplexed connection**
(every session's channel closed → 0 bytes transferred). Carrying the overshoot is
the correct, normal buffered-reader behavior and keeps the framed stream aligned.

The pure copy/carry logic is `fill_and_carry(received, dst, residual)`
(`usb_transport.rs`), unit-tested for exact-fit, overshoot, short-packet, and
residual-append cases.

## Reader frame reads must be atomic — never `select!`ed against control mid-frame

The persistent reader's per-frame read (`read_message_with_timeout`: header +
`data_length` payload across many bulk transfers + the residual carry-over) is
**NOT cancel-safe**: dropping it mid-frame discards the partial payload and
desyncs the stream. The reader must therefore drain `control_rx`
(`Register`/`Unregister`/`Subscribe`) **between** frames (non-blocking `try_recv`
loop), NOT race it against the read in a `tokio::select!`. Racing them was a real
bug: a `Register`/`Unregister` arriving while the reader was mid-way through a
large device→host WRTE (e.g. accepting a second concurrent `reverse` session, or
iperf3's parallel control+data connections) cancelled and corrupted the in-flight
frame — one of two concurrent device→host bulk streams silently stalled at 0
bytes (device-reproduced; iperf3 reverse showed `sender>0, receiver 0`).

To preserve the register-before-route guarantee (`open_session` /
`accept_device_open` send `Register(local_id)` then the device replies), the
reader also drains control **again right before classifying** each freshly-read
frame, so a `Register` that was queued during the uninterruptible read is applied
before its session's first frame is routed (otherwise the reply misroutes to the
device-OPEN queue). Frame-read latency bounds how long a queued control message
waits, which is short.

## Bug #3 status: RESOLVED (root cause found + device-verified)

The TRUE root cause was the **CNXN banner trailing NUL** (see the no-NUL section
above): it made adbd's `SupportsDelayedAck()` false, so adbd rejected the windowed
`OPEN(arg1=32MiB)` with `A_CLSE`. Removing the NUL was verified end-to-end on the
real Android-16 device — windowed `open_session` now succeeds (12–18 ms) with a
4-byte windowed OKAY grant `[00,00,00,02]` (32 MiB).

Three complementary fixes shipped, all still valid:
1. **The fix**: drop the CNXN banner trailing NUL (this makes windowed mode actually
   work).
2. CLSE-routing fast-fail (`open_session` races ack+data): turns any future OPEN
   rejection into a 1–2 ms diagnosable error instead of a 10 s hang — this is what
   made the root cause observable in the first place.
3. `read_exact` desync guard: defensive; report hypothesis 2e (read_exact dropping
   the 4-byte OKAY) was **refuted** against AOSP (separate header/payload writes).

Downstream may now drop the `delayed_ack=false` workaround and use windowed mode.

## Graceful teardown: flush ONE connection-level CLSE while the writer is alive

A persistent connection MUST be torn down through an explicit graceful path —
`PersistentUsbConnection::shutdown(&self)` (for `Arc`-held connections, e.g. the
server backend cache) or `close(self)` — **not** left to `Drop`. The graceful
path flushes exactly one **connection-level CLSE** (`A_CLSE(arg0=0, arg1=0)`)
via `send_with_ack` (awaiting the write) and sets a shared `conn_closed` flag.

- **Why not Drop**: `Drop` can only fire-and-forget a CLSE onto the writer
  channel (no async in `Drop`). At process teardown the tokio runtime often
  retires the **writer task before** the connection's `Drop` runs, so the
  enqueue fails (`BrokenPipe "writer task gone"`) and the device is left with
  orphaned streams. On the NEXT connection those orphans surface as stale
  `A_CLSE` replies to the fresh `CNXN`, which (without enough retries) fails the
  handshake — observed as flaky `usb_direct` SKIPs in selftest across runs.
- **One CLSE, not N**: a connection-level `CLSE(0,0)` tells adbd the whole
  connection is gone, so per-stream CLSEs are redundant. Setting `conn_closed`
  makes each live `SessionInner::Drop` **skip** its per-stream CLSE — otherwise
  they race the retiring writer and emit spurious `writer task gone` warnings.
- **Idempotent**: `flush_connection_clse_impl` uses a `compare_exchange` on
  `conn_closed` so the first caller wins; repeat `shutdown`/`close` + the later
  `Drop` send nothing more. `Drop` also checks `conn_closed` and skips its
  fallback CLSE when a graceful close already ran.
- **Wiring (cross-layer, all required)**: `daemon::run_server` (after
  SIGTERM/ctrl_c) and selftest's `InProcessServer::shutdown` both call
  `UsbDeviceBackend::shutdown().await`, which drains the `conns` cache and
  `shutdown()`s each connection BEFORE aborting the accept loop / exiting. A bare
  `Drop`/`task.abort()` is insufficient.

## Stale-CLSE drain is bounded and re-run per stale reply

`do_connect` drains buffered stale frames before the handshake (`drain_stale`,
bounded by `STALE_DRAIN_MAX_FRAMES`) AND re-drains after each stale `CLSE`
response, retrying CNXN up to `CNXN_MAX_ATTEMPTS` (8, was a fixed 3). The count
matters: an unclean teardown can leave one orphaned stream's CLSE **per session**
the previous connection had open, so the multi-session server path can queue
several — a fixed-3 bound under-counted and failed the handshake. This is
belt-and-suspenders behind the graceful-teardown fix above: even if a CLSE is
ever missed, the next connect self-heals instead of failing.

## STLS upgrade: `upgrade_connection()` already consumes the post-STLS CNXN — do NOT read again

When adbd replies to CNXN with `A_STLS` (TCP/IP transports; USB never does), the
persistent multiplexer's `do_connect` must: ACK with an `STLS` frame, call
`transport.upgrade_connection().await`, and then **return immediately** — it must
NOT issue another `read_message()`.

`TcpTransport::upgrade_connection()` (`tcp/tcp_transport.rs`) performs the TLS
handshake AND reads+consumes the device's post-STLS CNXN banner internally before
returning `Ok(())`. This matches the proven direct path
`ADBMessageDevice::connect`, which likewise returns right after
`upgrade_connection()` with no further read. A second read in the persistent path
(e.g. a `finish_after_stls` helper that re-reads the banner) **blocks forever on a
frame the device never sends** → `host:connect` to any TLS-requiring wireless
device hangs. (Caught in PR4b review; the multiplexer is transport-generic now, so
this path is exercised for the first time over TCP.)

Because the post-STLS banner/version are swallowed by `upgrade_connection`, the
persistent path reports `(A_VERSION_LEGACY, "")` after the upgrade →
`delayed_ack_negotiated == false` → classic stop-and-wait over the TLS channel.
That is the safe, AOSP-faithful default for a freshly TLS-upgraded link; the
USB/non-STLS branch is unchanged and still negotiates `delayed_ack` from the real
CNXN banner. USB's `upgrade_connection` is the trait-default no-op, so USB never
enters this branch and its behavior is unaffected.
