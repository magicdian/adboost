# Research: transport reopen vs in-place CNXN retry for back-to-back root/unroot re-enumeration

- **Query**: The 15-attempt in-place CNXN retry was the WRONG layer. The trace shows a re-enumerated
  device can only be recovered by REOPENING the transport (fresh `connect()` → fresh endpoints), not
  by re-sending CNXN on the dead old transport. Where should the re-enumeration retry actually live?
- **Scope**: mixed (internal adboost source + nusb 0.2.3 / io-kit-sys 0.5.0 in cargo cache + AOSP knowledge)
- **Date**: 2026-06-24
- **Prior research (read first, NOT redone)**:
  `.trellis/tasks/archive/2026-06/06-23-backend-get-or-open-settle-retry-root-bug/research/reenumeration-readiness.md`

---

## TL;DR for the PRD author

1. **Hypothesis 1 CONFIRMED.** `do_connect` retries IN PLACE on the same `transport: &mut T`. It never
   re-`connect()`s. The transport's endpoints are captured ONCE in `new_with_features`
   (`persistent.rs:593`, `transport.connect()`) and bound to one IOKit registry id (the `DeviceInfo`
   from `new_by_serial`'s `nusb::list_devices()`). When adbd re-enumerates under a NEW registry id, those
   endpoints are permanently dead → every in-place retry returns `device disconnected` (`Disconnected`)
   forever. The trace's attempts 2..15 are exactly this.
2. **Hypothesis 2 CONFIRMED, with the precise driver identified.** The 22.670 reopen was NOT
   `get_or_open`'s `retry_within` (it does not retry `ADBRequestFailed`) and NOT
   `open_session_with_reopen` (it `?`-propagates the `get_or_open` error before it can loop). It was the
   **NEXT top-level service request** from the client driving a FRESH `open_local_service` →
   `open_session_with_reopen` → `get_or_open` → `new_from_serial`, which re-enumerated and built a NEW
   transport (registry id B) and succeeded immediately. So today's recovery is *accidental* (it relies on
   the client issuing another command), not designed.
3. **Hypothesis 3 — the fix layer is the REOPEN layer.** The recovery MUST rebuild the transport. Only
   `new_from_serial`/`get_or_open` does that. The previous task's *decoupling* (outer
   `is_retryable_open_error` deliberately NOT retrying `ADBRequestFailed`/`Stall`) is what STOPS the outer
   layer from owning re-enumeration recovery — that decoupling is the bug. The inner `do_connect`
   transient retry should shrink to a tiny same-handle budget; the outer wall-clock retry should own
   re-enumeration by reopening a fresh transport, and therefore SHOULD re-drive a CNXN-exhausted
   `ADBRequestFailed`. Bounded by outer wall-clock × small inner = the outer budget, not a product.
4. **Hypothesis 4 — stop whack-a-mole with codes.** `0xe00002d8` = `kIOReturnNotReady` (verified, see Q4)
   is a NEW code that lands in `TransferError::Unknown(0xe00002d8)` and is matched by NEITHER current
   predicate. Rather than add it as constant #3, classify by `TransferError` VARIANT family +
   "any transfer error inside the bounded reopen window is retryable-by-reopen". Only `InvalidArgument`
   (and arguably `Fault`) are structurally non-transient. Bound, never code list, keeps it honest.
5. **Hypothesis 5 — AOSP precedent.** Native adb recovers from adbd restart by *tearing down and
   re-opening* the transport (hotplug-driven `register_usb_transport` / reconnect handler builds a NEW
   connection object), NOT by retrying bulk I/O in place on the dead handle. This is the upstream
   precedent for putting recovery at the reopen layer. (Marked partially-unverified: exa tools
   unavailable; symbol names from documented design, verify against AOSP tree.)
6. **REVERSES three previous-task decisions** (see Q6): the 8→15 inner enlargement, the inner-owns-`Stall`
   choice, and the outer `is_retryable_open_error` excluding `ADBRequestFailed`. The trace is the new
   evidence: 15 in-place attempts ALL failed `device disconnected`, then the FIRST reopen succeeded
   immediately. In-place retry count is irrelevant to re-enumeration; reopen is the only thing that works.

---

## Findings

### Files Found

| File Path | Role in this bug |
|---|---|
| `adboost/src/message_devices/usb/persistent.rs` | `do_connect` (924-1059) — in-place CNXN/transient retry, NEVER re-`connect()`s. `new_with_features` (571-660) — one-time `transport.connect()` at 593 then `do_connect` at 598. `CNXN_MAX_ATTEMPTS=15` (108). `CONNECT_RETRY_SETTLE=150ms` (130). `is_transient_connect_error` (169-178). `IOKIT_NOT_RESPONDING=0xe000_02ed` (122). `ScriptedTransport` mock + tests (2813-2900+). |
| `adboost/src/message_devices/usb/usb_transport.rs` | `new_by_serial` (115-125) — fresh `list_devices()` → fresh `DeviceInfo` (one registry id). `connect` (381-416) — one-time claim + endpoint capture into `Connection`/`write_connection`, NO settle, NO re-probe. `map_transfer_status` (337-343) / `map_write_status` (360-378) — only `Cancelled` special-cased; every other `TransferError` → `e.into()` = `UsbTransferError`. |
| `adboost/src/server/default_backend.rs` | `get_or_open` (342-387) — outer wall-clock reopen via `retry_within` (359-366). `open_session_with_reopen` (402-425) — first-OPEN reopen loop. `retry_within` (77-97). `is_retryable_open_error` (125-133) — the NARROWED outer predicate (no `Stall`, no `ADBRequestFailed`). `OPEN_RETRY_BUDGET=10s` (61), `OPEN_RETRY_POLL=500ms` (66). `open_local_service` (640-669) uses reopen; `open_sync_session` (671-677) / `open_shell_v2` (679-685) use bare `get_or_open`. |
| `nusb-0.2.3/src/transfer/mod.rs` | `TransferError` enum (23-52): `Cancelled, Stall, Disconnected, Fault, InvalidArgument, Unknown(u32)`. |
| `nusb-0.2.3/src/platform/macos_iokit/mod.rs` | `status_to_transfer_result` (33-40): the IOKit→variant mapping. |
| `io-kit-sys-0.5.0/src/ret.rs` | IOKit return-code decode (NotReady=0x2d8 at line 72, NoDevice=0x2c0 at 24, NotResponding=0x2ed). |
| `.trellis/spec/backend/server-host-protocol.md` | Existing "Common Mistake" re-enumeration section (consumer-retry framing). |

---

### Q1 — Does `do_connect` retry in place, never reopening? (CONFIRMED)

**Yes. The transport is bound to ONE enumeration instance, captured once, and `do_connect` only re-sends
I/O on that same handle.**

The build chain (server path):
```
get_or_open (default_backend.rs:359)
  → new_from_serial(serial)               (persistent.rs:703)
     → USBTransport::new_by_serial(serial) (usb_transport.rs:115)  ← fresh list_devices(), one DeviceInfo
     → new_with_features(transport, …)     (persistent.rs:571)
        → transport.connect().await?       (persistent.rs:593)     ← ONE-TIME claim + endpoint capture
        → do_connect(&mut transport, …)    (persistent.rs:598)
```

`USBTransport::connect` (usb_transport.rs:381-415) opens the device, claims the interface, and grabs the
bulk IN/OUT endpoints, storing them into the shared `Connection`/`write_connection` (402-409). These
endpoint objects are bound to the IOKit object behind that one `DeviceInfo`. There is **no** re-open / re-claim
/ re-acquire anywhere after this point. (Prior research Q2 already established no settle/probe; that holds.)

`do_connect` (persistent.rs:924-1059) takes `transport: &mut T` and its retry loop (958-1056) does ONLY:
- `transport.write_message(cnxn_msg).await` (972) — on transient: `sleep(CONNECT_RETRY_SETTLE); continue;` (977-978)
- `transport.read_message().await` (983) — on transient: same settle+continue (989-990)
- on stale CLSE: `drain_stale(transport); sleep; ` loop again (1046-1047)

It **never** calls `transport.connect()`, `disconnect()`, or rebuilds the transport between attempts. The
in-tree mock proves this structurally: `ScriptedTransport::connect()` (persistent.rs:2831) is a no-op and
the test re-enters `do_connect` on the *same* object — the test only exercises re-`write_message`, never
re-`connect`. So once registry id A's endpoints are dead, the loop is futile.

**The trace maps exactly:**
- `20.369 got read/write endpoint` = `USBTransport::connect` succeeded against registry id A.
- `20.369 attempt 1/15 (0xe00002ed NotResponding)` = first CNXN write, `Unknown(0xe00002ed)` → matched
  by `is_transient_connect_error` → settle+retry.
- `20.520..22.492 attempts 2..15 all "device disconnected"` = adbd has now re-enumerated under registry id
  B; registry id A's endpoints return `kIOReturnNoDevice` → `TransferError::Disconnected` → also matched →
  settle+retry, but the handle is permanently dead, so all 14 remaining attempts fail identically ~150 ms
  apart (= `CONNECT_RETRY_SETTLE`).
- `22.492 CNXN failed after 15 attempts → ADBRequestFailed` = loop exhausted (persistent.rs:1057-1059).

> The 15-attempt budget cannot help here: re-enumeration is not a "wait longer on the same handle" problem;
> the handle is gone. This is the core refutation of the previous task's `CNXN_MAX_ATTEMPTS` 8→15 sizing.

---

### Q2 — What actually drove the 22.670 reopen? (CONFIRMED: the NEXT command, not the retry layers)

Walk the error after `do_connect` returns `ADBRequestFailed`:

1. `new_with_features` (`do_connect(...).await?`, persistent.rs:598) propagates `ADBRequestFailed`.
2. `new_from_serial` propagates it.
3. Inside `get_or_open`, `retry_within(OPEN_RETRY_BUDGET, …, is_retryable_open_error, || new_from_serial…)`
   (default_backend.rs:359-364): on the returned error, `retry_within` (line 93) checks
   `!is_retryable(&e)`. `is_retryable_open_error` (125-133) matches ONLY
   `UsbTransferError(Unknown(0xe000_02ed) | Disconnected)` and `DeviceNotFound(_)` — it does **NOT** match
   `ADBRequestFailed`. So `retry_within` returns `Err(ADBRequestFailed)` IMMEDIATELY (no reopen).
4. `get_or_open` `?`-propagates it (366).
5. `open_session_with_reopen` (402-425): the loop body starts `let conn = self.get_or_open(serial).await?;`
   (409). The `?` propagates the `get_or_open` error **out of the function** before reaching the
   `match conn.open_session` reopen logic. So `open_session_with_reopen` does NOT reopen on a
   CNXN-exhausted connect either — its reopen path is reachable only when `get_or_open` SUCCEEDS and the
   subsequent `open_session` kills the connection (the first-OPEN race).
6. `open_local_service` propagates → the server returns `ADBRequestFailed` to the client for THIS command.

So nothing in the current retry machinery reopened at 22.670. The reopen was the **client's next service
request** entering a fresh `open_local_service` → fresh `get_or_open` → fresh `new_from_serial`, which
re-enumerated (now seeing registry id B), built a new transport, and the very first CNXN succeeded
(`22.775 unencrypted connection established`). Today's "recovery" is incidental and command-driven, which
is why a single failed command surfaces an error to the user even though the device is fine.

> **Design consequence**: the recovery the trace proves works (build a NEW transport) is exactly what
> `get_or_open`/`retry_within` would do on the NEXT poll IF it retried `ADBRequestFailed`. The fix is to
> let the outer layer re-drive the CNXN-exhausted error by reopening — i.e. reverse the prior decoupling.

---

### Q3 — Where should the re-enumeration retry live?

**Inner `do_connect` scope (shrink it).** The only thing the inner loop can legitimately recover is a
genuinely-transient hiccup on a STILL-VALID handle — adbd briefly not answering before it re-enumerates
(the `20.369 attempt 1 NotResponding` is the one honest case). It CANNOT recover a re-enumerated handle
(attempts 2..15 prove it). So the inner budget should drop from 15 to a small N (e.g. 2-3) purely for the
same-handle not-ready blip; it should NOT be sized for the "re-enumeration window" (the doc comment at
persistent.rs:97-107 sizing 15 to the 1177 ms reopen window is now known to be the wrong model — you
cannot ride out a reopen on a dead handle). Stale-CLSE drain retries can keep their own bound if needed,
but the transient-transfer arm should be small.

**Outer `get_or_open`/`retry_within` (make it own re-enumeration).** This is the only layer that rebuilds
the transport (`new_from_serial` → `USBTransport::new_by_serial` → fresh `DeviceInfo` → fresh `connect`).
It already has the right shape: wall-clock budget (`OPEN_RETRY_BUDGET=10s`), poll interval
(`OPEN_RETRY_POLL=500ms`), drop-and-rebuild each attempt. The single change needed: `is_retryable_open_error`
must ALSO retry the CNXN-exhausted `ADBRequestFailed` (and `Stall`-exhausted) coming out of `do_connect`,
because that is precisely the "device re-enumerated, the old transport is dead, build a new one" signal.

**Bounded + non-amplifying:** with the inner shrunk to ~2-3 same-handle attempts (≈ a few hundred ms) and
the outer a wall-clock 10 s budget that REBUILDS each poll, total time ≈ outer wall-clock (10 s), NOT a
product. Each outer poll spends a small fixed inner cost then sleeps `OPEN_RETRY_POLL`; the inner can never
blow up the outer because it is a small constant. This is the opposite of the previous fear (which
decoupled them precisely to avoid a 10 s × 2.25 s product) — once the inner is small, coupling them is
SAFE and is the only thing that recovers re-enumeration.

**Distinguishing "same handle, not ready" from "re-enumerated, must reopen":** the code layer cannot do it
reliably (prior research Q1 established `NoDevice`/`Disconnected` is identical to a real unplug; the trace
shows `Disconnected` is the re-enumeration signal here). The robust contract is therefore NOT discrimination
but **always recover by reopening on the outer wall-clock budget, and shrink the inner to near-zero**. The
outer bound (a real unplug never returns within 10 s; a re-enumeration does, ~850 ms median per PR0) is
what stays honest — exactly the prior research's "bound, not code" principle, now applied at the reopen
layer instead of the in-place layer.

> Optional refinement (not required): on the FIRST inner transient (`NotResponding` on a still-valid
> handle) retry once or twice in place to catch the cheap blip without a full reopen; on `Disconnected`
> (handle gone) skip straight to returning so the outer reopens immediately rather than burning 14
> pointless same-handle attempts. But the simplest correct design is: inner≈1-2, outer owns everything.

---

### Q4 — Family-style transient classification (stop whack-a-mole)

**The new code `0xe00002d8` decodes to `kIOReturnNotReady`** (`io-kit-sys-0.5.0/src/ret.rs:72`:
`SYS_IOKIT | SUB_IOKIT_COMMON | 0x2d8`). nusb's macOS mapping
(`nusb-0.2.3/src/platform/macos_iokit/mod.rs:33-40`) names only:
```
kIOReturnSuccess | kIOReturnUnderrun  => Ok(())
kIOReturnNoDevice                     => Disconnected          (0xe00002c0)
kIOReturnAborted | TransactionTimeout => Cancelled            (0xe00002eb)
kIOUSBPipeStalled                     => Stall
kIOReturnBadArgument                  => InvalidArgument
_                                     => Unknown(status as u32)  ← NotReady 0x2d8 AND NotResponding 0x2ed land here
```
So `kIOReturnNotReady (0xe00002d8)` → `TransferError::Unknown(0xe00002d8)`, matched by NEITHER
`is_transient_connect_error` (only `Unknown(0xe00002ed)`) NOR `is_retryable_open_error`. Adding it as a
third constant is the whack-a-mole the brief warns against.

**`nusb 0.2.3` `TransferError` variants** (`nusb-0.2.3/src/transfer/mod.rs:23-52`):
| Variant | Meaning | Re-enumeration-window classification |
|---|---|---|
| `Cancelled` | cancelled/timed out (Aborted / TransactionTimeout) | already handled as timeout (`ReadTimeout`/`WriteTimeout`); not a connect transient |
| `Stall` | endpoint STALL | transient-in-window (adbd restarting, pipe not ready) |
| `Disconnected` | device disconnected (`kIOReturnNoDevice`) | transient-in-window (re-enumeration) AND a real unplug — bound disambiguates |
| `Fault` | hardware issue / protocol violation | likely fatal; do NOT retry (or retry only under the bound, conservative) |
| `InvalidArgument` | invalid/unsupported request (`kIOReturnBadArgument`) | DEFINITELY fatal, never retry (programming error, not transient) |
| `Unknown(u32)` | OS-specific unmapped (incl. NotResponding 0x2ed, NotReady 0x2d8, and any future IOKit code) | transient-in-window — the catch-all that ends whack-a-mole |

nusb's own doc on `Unknown` (mod.rs:46-50): *"It won't be considered a breaking change to map unhandled
errors from `Unknown` to one of the above variants"* — so pinning behavior to a specific `Unknown(code)`
is fragile across nusb patch releases; classifying by variant family is more robust.

**Recommended family predicate** (the reopen-window classifier owned by the OUTER layer):
> Treat **any `TransferError` except `InvalidArgument`** (and the already-special-cased `Cancelled`
> timeout) as retryable-by-reopen WITHIN the wall-clock budget. Concretely:
> `Stall | Disconnected | Unknown(_)` ⇒ retryable-by-reopen; `InvalidArgument` ⇒ fatal; `Fault` ⇒ fatal
> (conservative — a protocol violation is not a re-enumeration blip). PLUS the CNXN-exhausted
> `ADBRequestFailed` from `do_connect` (the "old transport dead" signal, Q2/Q3) ⇒ retryable-by-reopen.
> PLUS `DeviceNotFound` (device momentarily absent from enumeration) ⇒ retryable-by-reopen (already there).
This stops matching specific IOKit codes entirely — `0xe00002d8` and any future code are covered by
`Unknown(_)`, bounded by the 10 s wall-clock, never hanging on a real fault or unplug.

> Caveat: matching `Unknown(_)` broadly is safe ONLY because the OUTER retry is wall-clock bounded and
> rebuilds the transport (a real fault fails fast within budget). Do NOT use a broad `Unknown(_)` predicate
> for the in-place inner loop — there it could spin on a dead handle (which is the very bug here).

---

### Q5 — AOSP reference: reconnect by reopen, not in-place I/O retry

AOSP adb's host transport recovers from an adbd restart by **rebuilding the transport (a new connection
object)**, not by retrying bulk read/write on the dead handle:
- The read pump detects the disconnect (read returns error/EOF) and the transport is **removed/kicked**
  (`transport.cpp` `kick_transport` / `handle_offline` → the `BlockingConnectionAdapter` read thread exits).
- **Reconnection** is driven by `ReconnectHandler` / `transport.cpp`'s reconnect path: it calls the
  transport's stored `reconnect` callback, which for USB re-attaches via the IOKit/usbfs hotplug machinery
  (`usb_osx.cpp` / `usb_libusb.cpp` `register_usb_transport`) — building a FRESH `usb_handle` /
  connection object and a fresh CNXN handshake on it. It is bounded with a backoff/retry count, and a
  single failed reopen is retried rather than surfaced to the user.
- For `adb root`/`unroot`, the client (`commandline.cpp` `adb_root`) sends the service, sees the
  "restarting" reply, then `wait-for-device`/`adb_wait_for_device` until the transport returns — it never
  re-uses the old transport's endpoints.

**Upstream precedent for our fix**: the previous research already noted native detects disconnect at the
read pump and removes the transport. This research's addition: native's RECONNECT path **reopens** (builds
a new connection object via the hotplug/`register_usb_transport` path); it does NOT loop bulk I/O on the
torn-down handle. That is the exact precedent for putting re-enumeration recovery at adboost's reopen layer
(`get_or_open`/`new_from_serial`) rather than in `do_connect`'s in-place loop.

> Caveat: exa/web tools unavailable in this environment; symbol names (`ReconnectHandler`, `kick_transport`,
> `register_usb_transport`, `adb_wait_for_device`) are from the documented AOSP transport design, not a
> freshly fetched tree. The behavioral shape (detect-at-read-pump → remove → hotplug-driven reopen of a new
> connection object, bounded backoff) is stable across AOSP versions; verify exact symbols before quoting.

---

### Q6 — Concrete recommendation for adboost (with file:line integration points)

**Reverses these previous-task decisions (justified by the trace):**
1. **`CNXN_MAX_ATTEMPTS` 8→15 enlargement** (persistent.rs:108, doc 91-107). The trace shows 15 in-place
   attempts ALL failed `device disconnected` on a dead handle. In-place count is irrelevant to
   re-enumeration. → **Reverse**: shrink the *transient-transfer* arm to a small same-handle budget (≈2-3).
   (Stale-CLSE drain may keep a separate, modest bound — that IS a same-handle case.)
2. **Inner owns `Stall`** (`is_transient_connect_error` includes `Stall`, persistent.rs:175). With the
   inner shrunk and the outer owning reopen, `Stall` belongs to the reopen-window family at the OUTER
   layer. → **Reverse**: move `Stall` (and the broad `Unknown(_)` family) into the outer reopen predicate.
3. **Outer `is_retryable_open_error` excludes `ADBRequestFailed`/`Stall`** (default_backend.rs:125-133, doc
   111-124, anti-amplification). This is the decoupling that PREVENTS the outer layer from recovering
   re-enumeration. → **Reverse**: the outer predicate SHOULD retry the CNXN-exhausted `ADBRequestFailed`
   and the transient family — because the inner is now small, the product fear no longer applies.

**Inner `do_connect` retry (persistent.rs:958-1056):**
- Scope: ONLY a genuinely-transient same-handle blip (adbd not answering before re-enumeration).
- Attempt count: small (≈2-3) for the transient-transfer arm, replacing the re-enumeration-sized 15.
- Classifier: keep `is_transient_connect_error` NARROW (the cheap same-handle blip) — or, simplest, on a
  `Disconnected` (handle gone) return immediately so the outer reopens rather than burning attempts.
- Keep the existing stale-CLSE drain behavior (that is a real same-handle case).

**Outer `get_or_open`/`retry_within` (default_backend.rs:342-387, 77-97):**
- Make `is_retryable_open_error` the REOPEN-WINDOW predicate (Q4 family): retry
  `UsbTransferError(Stall | Disconnected | Unknown(_))` (NOT `InvalidArgument`/`Fault`),
  `ADBRequestFailed` (CNXN-exhausted from `do_connect`), and `DeviceNotFound(_)`. Keep `DeviceBusy` fatal.
- Each `retry_within` poll already calls `new_from_serial` fresh → fresh `USBTransport::new_by_serial` →
  fresh `connect` → fresh endpoints. This IS the reopen the trace proves works. No new code path needed;
  just widen the predicate so the loop actually re-drives the re-enumeration error instead of returning it.
- Bound stays `OPEN_RETRY_BUDGET=10s` / `OPEN_RETRY_POLL=500ms`. With a small inner, total ≈ 10 s wall
  clock (NOT a product). A truly-absent device / real fault still fails fast within 10 s.
- Also route `open_sync_session` (671-677) and `open_shell_v2` (679-685) — they call bare `get_or_open`,
  which now reopens, so they inherit re-enumeration recovery automatically. (`open_local_service` already
  uses `open_session_with_reopen` for the first-OPEN race; that stays.)

**Family-style transient predicate (Q4):** classify by `TransferError` variant family +
`ADBRequestFailed`/`DeviceNotFound`, NOT by enumerating IOKit codes. `0xe00002d8 NotReady`,
`0xe00002ed NotResponding`, and any future code are all `Unknown(_)` and covered. Drop the
`IOKIT_NOT_RESPONDING` constant from the OUTER predicate (the inner narrow predicate may keep it).

**Bounded + non-amplifying invariant to assert in the PRD/tests:** inner attempts ≤ small constant; outer
governed by wall clock; total worst-case ≈ outer budget. Add a test that simulates "first transport's CNXN
exhausts → ADBRequestFailed; second `new_from_serial` (fresh transport) succeeds" at the `get_or_open`/
`retry_within` level (the existing `retry_within` is already pure over a closure + predicate —
default_backend.rs:77, tests at 988+ — so a closure that returns `ADBRequestFailed` once then `Ok`, with
the widened predicate, locks this without hardware). Pair with the existing `ScriptedTransport`
(persistent.rs:2813) shrunk-inner test.

**Spec follow-up** (main agent via `update-spec`, not me): `.trellis/spec/backend/server-host-protocol.md`
should record that re-enumeration recovery lives at the REOPEN layer (`get_or_open` rebuilds the transport),
that the inner `do_connect` retry is only for a same-handle blip, and the family-style (`Unknown(_)`-catch-all,
bounded) transient classification — superseding the prior task's "broaden the inner CNXN loop" guidance.

---

## Caveats / Not Found

- **Hypotheses 1 and 2 are CONFIRMED from source with line citations** (the build chain, the one-time
  `connect()` at persistent.rs:593, the in-place-only retry loop, and the exact `?`-propagation in
  `open_session_with_reopen:409` / `retry_within` not matching `ADBRequestFailed`).
- **`0xe00002d8 = kIOReturnNotReady`** verified against `io-kit-sys-0.5.0/src/ret.rs:72`; it surfaces as
  `TransferError::Unknown(0xe00002d8)` (nusb mod.rs:40 catch-all), matched by neither current predicate —
  this is the new whack-a-mole instance.
- **`endpoint stalled`** = `TransferError::Stall` (nusb mod.rs:58 Display); **`device disconnected`** =
  `TransferError::Disconnected` (mod.rs:59); **`NotResponding 0xe00002ed`** / **`NotReady 0xe00002d8`** =
  `Unknown(...)` (mod.rs:62-63 Display "unknown (...)"). These match the trace strings exactly.
- **AOSP specifics (Q5) partially unverified** — exa/web tools unavailable; behavioral shape (reopen, not
  in-place I/O retry) is stable and is the upstream precedent, but verify symbol names against the tree
  before quoting verbatim in the PRD.
- **Whether to retry `Fault`** in the outer family is a judgement call left to the PRD: I recommend NOT
  (protocol violation ≠ re-enumeration blip), but the wall-clock bound would make either choice safe.
- I did not re-verify every non-server caller of `open_session`; the analysis covers the server path
  (`open_local_service`/`open_sync_session`/`open_shell_v2`) which is the production `adb root` path.
