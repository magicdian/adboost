# Harden USB receive path + fix windowed OPEN hang (bug #3)

Three independent issues found via deep audit (workflows: `magic-only-decision-audit`,
`bug3-windowed-open-rootcause`), all verified against AOSP `packages/modules/adb` source.

## Issue A — Unbounded allocation / OOM (CRITICAL, pre-existing)

`usb_transport.rs:366` and `tcp_transport.rs:184` allocate
`vec![0_u8; header.data_length() as usize]` directly from the wire `data_length`
(attacker/corruption-controlled u32, up to ~4 GiB) with **no bound check**, and the
magic integrity check runs *after* the allocation. A single 24-byte hostile/corrupt
header with `data_length = 0xFFFFFFFF` triggers a ~4 GiB allocation → OOM/abort.

AOSP `transport.cpp::check_header` rejects `data_length > MAX_PAYLOAD` *before* reading
the payload — adboost omits exactly this clause.

**Fix**: before the `vec!` allocation in BOTH transports, reject oversize length:
```rust
if header.data_length() as usize > MAX_PAYLOAD {
    return Err(RustADBError::ADBRequestFailed(format!(
        "frame data_length {} exceeds MAX_PAYLOAD {}", header.data_length(), MAX_PAYLOAD
    )));
}
```
`MAX_PAYLOAD` (1 MiB) is `pub` in `usb/flow_control.rs:26`. USB transport can import it
directly. For TCP: import the same constant (it lives under `usb/` but is a protocol
constant; if a cross-module import is awkward, define a shared `MAX_PAYLOAD` accessible to
both — prefer reusing the existing one over duplicating the literal). Independent of any
integrity-policy decision; must ship.

## Issue B — Reader tears down all sessions on one bad frame (MEDIUM, pre-existing)

`persistent.rs:647-650`: the reader loop treats ANY `ReadError` (a `ConversionError` from
an unknown command, or `InvalidIntegrity` from a bad magic) as a hard `break`, killing the
reader and thus EVERY multiplexed session. One malformed/garbled frame on the shared USB
pipe downs all sessions.

**Fix**: distinguish a *recoverable, frame-classifiable* decode/integrity error (log at
`warn`, skip the frame, `continue`) from a *fatal transport* error (disconnect/IO → break).
Concretely: `RustADBError::ConversionError` and `RustADBError::InvalidIntegrity(..)` from a
single frame read should be logged and skipped (the stream stays framed because the header
is fixed-size 24 bytes and each frame is read as header+`data_length` — a bad *magic* or
*unknown command* does not desync the next header read). A `UsbTimeout` already maps to
`ReadTimeout` (continue). Transport/IO errors (disconnect, endpoint gone) remain a `break`.
Keep the warn log including `cmd`/`arg0`/`arg1` for observability.

NOTE: this is conservative — only errors that leave the stream still frame-aligned may
`continue`. If unsure an error preserves framing, keep it fatal. Document the reasoning in a
comment.

## Issue C — Bug #3: windowed OPEN hangs 10 s on rejection (HIGH)

External bug report #3 (`/private/tmp/adboost-bugreport-3-delayed-ack-windowed-open-timeout.md`).
Root cause settled against AOSP source (the report's hypothesis 2a, NOT 2e):

- On OPEN rejection, adbd sends `A_CLSE(arg0=0, arg1=host_local_id)`
  (AOSP `adb.cpp`: `send_close(0, p->msg.arg0, t)`).
- `classify_message` (`persistent.rs:181-203`) keys on `arg1` (= our `local_id`), finds the
  registered session, and since `command != Okay` routes the CLSE to `RouteDecision::SessionData`
  → `data_rx`.
- `open_session` (`persistent.rs:874-881`) waits EXCLUSIVELY on `ack_rx.recv()`. The CLSE
  lands on `data_rx`, is never observed, and the call burns the full 10 s timeout.

The report's 2e (read_exact dropping the 4-byte windowed-OKAY payload) is **refuted**: AOSP
adbd sends the 24-byte header and the 4-byte window payload as TWO separate bulk writes; the
24-byte header is a short packet that terminates its own transfer, so host-side `read_exact`
receives header and payload as distinct completions. `read_exact`'s `min(remaining)` discard
branch is dead code against any spec-compliant ADB device (which frame-delimits with short
packets/ZLPs).

**Fix C1 (the real fix)**: make `open_session` `tokio::select!` over BOTH `ack_rx` (OKAY →
proceed) and `data_rx` (early `CLSE` → fail fast). Recognize OPEN-rejection by
`command == Clse` arriving on `data_rx` while awaiting the first ack (arg1 == our local_id is
already guaranteed by routing; do NOT require a specific arg0 — AOSP sends arg0=0). Return
`RustADBError::ADBRequestFailed("open_session: OPEN rejected by device (CLSE)")` immediately.
This turns a silent 10 s hang into a fast, diagnosable error **regardless** of why adbd
rejected — which is also the diagnostic that will reveal whether windowed OPEN is rejected
for a deeper reason.

**Fix C2 (defensive hardening, not the bug)**: in `read_exact`, when
`received.len() > remaining` (a device packed more than requested into one completion),
do NOT silently discard. At minimum return a clear error
(`RustADBError::ADBRequestFailed("USB frame desync: transfer exceeded requested length")`)
so a future non-compliant device/firmware surfaces loudly instead of desyncing silently.
A `debug_assert!` is insufficient (stripped in release). Document that real adbd never
triggers this (separate header/payload writes), so this is a guard, not a hot path.

**Diagnostic (cheap, include)**: the reader's per-frame log is currently `trace`
(`persistent.rs:653`). Leave as trace (it's per-frame and noisy), but ensure the new
open_session CLSE-rejection error message is distinct and actionable so downstream sees
"OPEN rejected by device (CLSE)" rather than "timeout".

## Honest open item (state in commit + to downstream)

Root-cause for *why* an Android-16 device would reject a windowed OPEN (vs accept it) is not
fully closed without a real-device debug/usbmon capture — the report notes Android 16 works
in classic mode. Fix C1 does not force windowed OPEN to succeed; it makes the failure mode
fast and diagnosable (CLSE rejection surfaces immediately). If adbd is in fact rejecting the
windowed OPEN, the fast error + the (now safe) reader will expose it. The downstream
`delayed_ack=false` workaround remains valid until that observation is captured. Do NOT claim
bug #3 "fully fixed" — claim "hang→fast-fail + ruled out 2e; 2a routing fixed".

## Acceptance criteria

- [ ] A: oversize `data_length` (> MAX_PAYLOAD) returns an error BEFORE any large allocation,
      on both USB and TCP read paths. Unit test with a hand-built header (sans-io).
- [ ] B: a single frame with bad magic / unknown command is logged and skipped; the reader
      loop continues and other sessions survive. (Unit-test the error-classification helper if
      one is extracted; otherwise document + targeted test.)
- [ ] C1: `open_session` returns a fast `ADBRequestFailed("OPEN rejected by device (CLSE)")`
      when a CLSE for the session arrives before any OKAY, instead of a 10 s timeout. Add a
      test exercising the select over ack_rx/data_rx (drive a CLSE into data_tx and assert
      fast failure; drive an OKAY into ack_tx and assert success).
- [ ] C2: `read_exact` returns a clear desync error (not silent discard) when a completion
      exceeds the requested length. Sans-io unit test on the helper if feasible.
- [ ] `cargo build`, `cargo clippy --all-targets --features usb -- -D warnings`,
      `cargo clippy --all-targets -- -D warnings`, `cargo test --features usb`, `cargo fmt --check`
      all green.
- [ ] No regression in existing classify/flow_control/integrity tests.

## Out of scope
- Forcing windowed OPEN to succeed (needs device capture; tracked as open item).
- read_exact residual-buffer rewrite (C2 is a guard/error, not a buffering redesign).
- Any change to the magic-only integrity decision (audited and confirmed optimal).
