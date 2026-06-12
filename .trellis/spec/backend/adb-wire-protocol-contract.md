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

## read_exact must not silently discard

`read_exact` must error (`"USB frame desync: ..."`) if a single bulk completion
returns more bytes than requested, not silently truncate. AOSP adbd writes the
24-byte header and the payload as **separate** bulk writes (the 24-byte header is a
short packet terminating its transfer), so a compliant device never over-delivers;
the guard surfaces non-compliant firmware loudly instead of desyncing.

## Bug #3 status (honest)

The CLSE-routing fix turns the windowed-OPEN **hang into a fast, diagnosable
error**; it does NOT force a windowed OPEN to succeed. Root cause for *why* an
Android-16 device might reject a windowed OPEN (vs accept) is not closed without a
real-device usbmon/debug capture. The `read_exact`-drops-the-4-byte-OKAY theory
(report hypothesis 2e) was **refuted** against AOSP (separate header/payload writes).
Downstream `delayed_ack=false` workaround remains valid until a capture is obtained.
