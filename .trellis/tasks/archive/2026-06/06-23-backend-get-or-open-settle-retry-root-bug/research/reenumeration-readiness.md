# Research: USB re-enumeration readiness — backend `get_or_open` / first-OPEN settle+retry

- **Query**: Where should the fix live for the post-re-enumeration "not-ready endpoint"
  transients (`0xe00002ed`, `0xe00002c0`) that fail the first connect and/or first OPEN
  on the server path (`adb root` reconnect)?
- **Scope**: mixed (internal code + nusb/io-kit-sys/libusb source in cargo cache + AOSP knowledge)
- **Date**: 2026-06-23

---

## TL;DR for the PRD author

1. **The IOKit codes are misnamed in the bug report and in the existing spec.** Decoded against
   the actual pinned `io-kit-sys 0.5.0`:
   - `0xe00002ed` = **`kIOReturnNotResponding`** ("device not responding"), NOT `kIOReturnAborted`.
     `kIOReturnAborted` is `0xe00002eb`. (`ret.rs:110` and `:114` below.)
   - `0xe00002c0` = **`kIOReturnNoDevice`** ("no device"). (`ret.rs:24`.)
   Both are genuinely the *not-ready-yet* family right after re-enumeration, but they surface as
   **different** `nusb::transfer::TransferError` variants, which matters for discrimination (Q1).
2. **adboost does NOT inspect IOKit codes anywhere.** The only special-case is `Cancelled`
   (timeout). `NotResponding` and `NoDevice` both end up as a generic
   `RustADBError::UsbTransferError(..)` carrying no machine-checkable "transient vs permanent"
   bit beyond the `TransferError` variant itself.
3. **`do_connect`'s CNXN retry does NOT cover write/submit IOErrors** — `transport.write_message(cnxn_msg).await?` (persistent.rs:817) propagates immediately; only a *stale CLSE response* retries.
4. **`USBTransport::connect` has no settle/readiness wait** — it claims the interface and grabs endpoints, then the very first transfer races the not-ready endpoint.
5. **Recommendation**: option (b) — broaden `do_connect`'s existing bounded loop to also retry on a **transient transfer error** (with a short settle), classified by a single helper that inspects the `TransferError` variant + IOKit `Unknown(code)`. This fixes the CNXN race for ALL consumers (USB-direct + server) with minimal blast radius and is the same idiom as `CNXN_MAX_ATTEMPTS`. Pair it with a thin bounded retry at `DefaultDeviceBackend::get_or_open` (option a) only if we also want to cover the case where the device is briefly *absent from enumeration* (`new_by_serial` → `DeviceNotFound`), which `do_connect` cannot see. The first-OPEN race (d) is best handled by `get_or_open` reopening the connection, NOT by mutating the shared writer loop. Details + tradeoffs below.

---

## Findings

### Files Found

| File Path | Role in this bug |
|---|---|
| `adboost/src/message_devices/usb/persistent.rs` | `do_connect` CNXN retry (~776-886), `writer_loop` fatal arm (~1271-1273), `open_session`/`send_open` (~1337-1504), `new`/`new_with_features` (~460-512), `new_from_serial` (~555-558). Constants at 80-99. |
| `adboost/src/message_devices/usb/usb_transport.rs` | `connect` (381-416) — claim + endpoint acquisition, NO settle. `map_transfer_status`/`map_write_status` (337-378) — the ONLY error classification; only `Cancelled` is special-cased. |
| `adboost/src/error.rs` | `RustADBError::UsbTransferError(#[from] nusb::transfer::TransferError)` (86); `UsbError(#[from] nusb::Error)` (81); `DeviceNotFound` (56); `DeviceBusy` (120). No IOKit-code-aware variant. |
| `adboost/src/server/default_backend.rs` | `get_or_open` (243-258) — ZERO retry around `new_from_serial`; `open_local_service` (425-452), `open_sync_session` (454-460), `open_shell_v2` (462-468) all `get_or_open(...).await?` then `open_session(...)`. |
| `adboost_cli/src/selftest/interactive.rs` | `open_device_with_retry` (670-682) — the proven consumer retry: loops `new_from_serial` on `POLL_INTERVAL` (1s) within a budget (~20s). Constants 27-33. |
| `.trellis/spec/backend/server-host-protocol.md` | "Common Mistake" section 504-538 — documents the *consumer* (selftest) retry; does NOT mention the backend gap. |

---

### Q1 — What do `0xe00002ed` / `0xe00002c0` mean, and transient vs permanent?

**Decoded against the pinned `io-kit-sys 0.5.0` (`Cargo.lock:766`), `src/ret.rs`:**

```
SYS_IOKIT        = ((0x38) & 0x3f) << 26   = 0xe0000000           (ret.rs:8)
SUB_IOKIT_COMMON = 0                                              (ret.rs:9)
kIOReturnNoDevice       = SYS_IOKIT | 0x2c0 = 0xe00002c0          (ret.rs:24)  "no device"
kIOReturnAborted        = SYS_IOKIT | 0x2eb = 0xe00002eb          (ret.rs:110) "operation aborted"
kIOReturnNotResponding  = SYS_IOKIT | 0x2ed = 0xe00002ed          (ret.rs:114) "device not responding"
```

So the two observed codes are:
- **`0xe00002ed` = `kIOReturnNotResponding`** — the endpoint exists but adbd/the pipe is not answering yet. This is the textbook *transient*, retry-succeeds signal right after re-enumeration. (The bug report and spec line 507 call it `kIOReturnAborted`; that is incorrect — Aborted is `0xe00002eb`.)
- **`0xe00002c0` = `kIOReturnNoDevice`** — the device handle/pipe is momentarily gone (the re-enumeration window where the old IOKit object is invalid and the new one isn't bound yet). Also transient *in the re-enumeration window*, but indistinguishable at the code level from a genuine unplug.

**How nusb 0.2.3 maps them** (`nusb-0.2.3/src/platform/macos_iokit/mod.rs:29-42`, `status_to_transfer_result`):
```rust
kIOReturnSuccess | kIOReturnUnderrun => Ok(()),
kIOReturnNoDevice => Err(TransferError::Disconnected),          // 0xe00002c0
kIOReturnAborted | kIOUSBTransactionTimeout => Err(TransferError::Cancelled),  // 0xe00002eb / timeout
kIOUSBPipeStalled => Err(TransferError::Stall),
kIOReturnBadArgument => Err(TransferError::InvalidArgument),
_ => Err(TransferError::Unknown(status as u32)),               // 0xe00002ed (NotResponding) lands HERE
```
Consequences:
- `0xe00002c0` → `TransferError::Disconnected`.
- `0xe00002ed` → `TransferError::Unknown(0xe00002ed)` (it is NOT one of the named arms).

**How adboost surfaces them** — `usb_transport.rs`:
- Read path `map_transfer_status` (337-343): only `Cancelled => ReadTimeout`; everything else → `e.into()` = `RustADBError::UsbTransferError(TransferError)` (via `error.rs:86`).
- Write path `map_write_status` (360-378): `Cancelled` is split into `WriteTimeout`/fatal-`IOError(TimedOut)` by frame position; **any non-`Cancelled` error → `e.into()` = `UsbTransferError`** (377). The unit test `write_non_timeout_error_is_transfer_error` (602-612) locks this for `Disconnected`.

**Can code distinguish "not ready, retry" from "actually gone"?** Partially:
- The `TransferError` variant IS preserved through `RustADBError::UsbTransferError(TransferError)` — so a classifier CAN match `Unknown(0xe00002ed)` (NotResponding), `Disconnected` (NoDevice), `Stall`, etc.
- But there is **no reliable, code-only way to distinguish a transient `NoDevice`/`Disconnected` during re-enumeration from a real unplug** — they are the same code. The only honest discriminators are (i) a **bounded** retry budget (a real unplug never recovers within ~a few seconds, a re-enumeration does), and (ii) cross-checking enumeration presence (`new_by_serial` succeeding means the device is back). This is exactly why the selftest uses a *time-budgeted* retry rather than an unbounded one, and why the fix must stay bounded so it never hangs on a truly-absent device.

> **Caveat for the PRD**: do not over-trust the variant. The robust contract is: *retry a SMALL bounded number of times with a short settle on the transient family, then give up.* The transient family for connect is at minimum `{TransferError::Unknown(NotResponding=0xe00002ed)}`; whether to also retry `TransferError::Disconnected` (NoDevice 0xe00002c0) is a judgement call — it widens coverage to the "pipe momentarily gone" case but risks a few wasted retries on a genuine unplug. Bounded budget makes that safe.

---

### Q2 — Does `USBTransport::connect` settle before the first transfer?

**No settle, no readiness probe.** `usb_transport.rs:381-416`:
```rust
async fn connect(&mut self) -> crate::Result<()> {
    let device = self.device_info.open().await?;                       // open handle
    let (read_endpoint, write_endpoint) = Self::find_endpoints(&device)?; // cached descriptors, no IO
    let interface = match device.claim_interface(read_endpoint.iface).await { // claim
        Ok(interface) => interface,
        Err(e) if e.kind() == nusb::ErrorKind::Busy => return Err(RustADBError::DeviceBusy),
        Err(e) => return Err(e.into()),
    };
    let read_ep  = interface.endpoint::<Bulk, In>(read_endpoint.address)?;  // grab endpoints
    let write_ep = interface.endpoint::<Bulk, Out>(write_endpoint.address)?;
    // ... store endpoints, clear read_buffer ...
    Ok(())
}
```
`find_endpoints` is pure descriptor parsing ("uses cached descriptor data (no IO)", comment at 234-235). Claiming the interface and constructing the endpoint queues does **not** prove adbd is ready for bulk I/O — the first *actual* transfer is the CNXN write in `do_connect` (persistent.rs:817), which is where `0xe00002ed`/`0xe00002c0` first appear. So `connect()` can return `Ok(())` against a device that is enumerated-but-not-ready, and the race is entirely on the first bulk transfer. There is no retry or delay inside `connect`.

---

### Q3 — Where should the fix live? Candidate layers + recommendation

#### (a) `DefaultDeviceBackend::get_or_open` — bounded retry around `new_from_serial`
- **Code today** (`default_backend.rs:253-255`): single `new_from_serial(...).await.map(Arc::new)?` with zero retry, under `self.conns` lock.
- **Pro**: localized to the exact server path that lacks retry; mirrors the proven `open_device_with_retry` consumer pattern; can also cover `new_by_serial → DeviceNotFound` (device briefly *absent from enumeration*), which lives in `USBTransport::new_by_serial` (usb_transport.rs:115-125) and which `do_connect` can never see because the transport isn't even built yet.
- **Con**: does NOT help the first-OPEN race (that is post-connect, after `new` returns `Ok` and the writer task is spawned); does NOT help direct library users of `new_from_serial`/`new_from_ids`; holding the `conns` mutex across a multi-second retry loop would serialize all other `get_or_open` callers (must release/re-acquire or scope carefully).

#### (b) `do_connect` / `new_with_features` — broaden the CNXN retry to cover write/submit transients (+ optional short settle)
- **Code today** (`persistent.rs:810-886`): the loop retries `CNXN_MAX_ATTEMPTS=8` times but ONLY on a *stale CLSE response*. The write itself (`transport.write_message(cnxn_msg).await?`, line 817) and the read (`transport.read_message().await?`, line 819) use `?` — a transient `UsbTransferError(NotResponding)` on either propagates out of `do_connect` immediately, failing `new_with_features`.
- **Pro**: fixes the CNXN race for **ALL** consumers — USB-direct (`new_from_ids`/`new_from_serial`) AND the server (`get_or_open` calls `new_from_serial`, which calls `new` → `new_with_features` → `do_connect`). Single shared handshake. Consistent with the existing bounded-loop idiom; the loop and `drain_stale` settle machinery already exist (the 100ms sleep at line 873 is precedent for a settle).
- **Con**: touches the shared handshake used by every transport build, so it must (1) stay bounded, (2) retry ONLY on the transient transfer family, and (3) NOT mask a genuinely-absent device. A `WrongResponseReceived`/`ADBRequestFailed` must still fail fast. Needs the Q1 classifier.
- **Note**: `do_connect` is generic over `T: ADBMessageTransport`, so a TCP transient would also be retried — acceptable and arguably desirable, but the transient classifier should be written so TCP's neutral errors don't accidentally match the USB-specific transient set (or gate the new behavior on the transfer-error family only).

#### (c) `USBTransport::connect` — settle/retry endpoint readiness at the transport
- **Pro**: deepest root cause; every transport user benefits without any handshake knowledge.
- **Con**: most invasive and riskiest; `connect()` currently does no IO beyond open/claim, so a readiness probe would mean issuing a throwaway transfer (and interpreting its error), duplicating logic that `do_connect`'s first CNXN already performs. Adds a transport-layer concept of "ready" that does not exist today and would need its own contract + tests.

#### (d) first-OPEN race — should `open_session` tolerate a transient?
- **Code today**: `open_session` → `send_open` → `writer.send_with_ack` (persistent.rs:1360-1364). The writer task `writer_loop` writes the OPEN frame; on a non-`WriteTimeout` error it hits the **fatal arm** `Err(e) => break` (1271-1273), tearing down the writer; the `oneshot` ack returns `Err`, so `send_open` → `open_session` returns `Err`. The connection is now dead (reader observes the closed transport and tears down).
- **Why NOT patch the writer loop**: the fatal arm exists deliberately — a non-`WriteTimeout` error AFTER a frame may have started means the OUT stream is *desynced/truncated* (see comment 1265-1270). For the FIRST OPEN on a freshly-built connection there is no prior in-flight frame, so a transient there is "clean", but the writer loop cannot cheaply tell "first frame, nothing truncated" from "mid-stream truncation". Adding retry-in-place to the shared writer risks re-introducing the truncation bug the loop was hardened against (the iperf3 reverse-tunnel teardown noted in the comments and in MEMORY `tcp-async-path-missing-usb-guarantees`).
- **Better**: treat a failed first OPEN the same as a failed connect — let `get_or_open` (option a) drop the (now-dead) cached connection and **reopen the whole connection** on a bounded retry. `is_alive()` already reports the dead connection (the reader task exited; `get_or_open` line 246-251 removes a non-alive cached conn), so a `get_or_open` retry that loops "open → open_session, and if either fails transiently, drop + retry" naturally covers BOTH the CNXN race and the first-OPEN race at the server layer.

#### RECOMMENDATION
Optimizing for {fixes the real production root path, minimal blast radius on the shared transport, doesn't mask real disconnects, consistent with `CNXN_MAX_ATTEMPTS`}:

> **Primary: option (b)** — in `do_connect`, broaden the existing bounded loop so that a **transient transfer error** on the CNXN write or read is retried (after a short settle, reusing/mirroring the existing 100ms sleep idiom) instead of propagating. Classify "transient" with a single small helper (Q1): match `RustADBError::UsbTransferError(TransferError::Unknown(c))` where `c == 0xe00002ed` (NotResponding) and `TransferError::Disconnected` (NoDevice 0xe00002c0), plus possibly `Stall`. Keep the same `CNXN_MAX_ATTEMPTS` bound (or a dedicated, small connect-retry bound) so a truly-absent device still fails fast. This fixes the CNXN race for every consumer with the smallest, idiom-consistent change and zero new transport-layer concept.
>
> **Secondary (covers first-OPEN + briefly-absent device): a thin bounded retry at `DefaultDeviceBackend::get_or_open`** that wraps "build connection (and, if you also want first-OPEN coverage at this layer, the caller's first `open_session`)" so a transient `new_from_serial` failure OR a dead-on-first-OPEN connection is dropped and retried within a budget — exactly the selftest's `open_device_with_retry` shape, but inside the backend. This is the layer that already owns the cache + `is_alive` reaping and is the precise `adb root` production path (PR2). It also catches `DeviceNotFound` (device momentarily not enumerated), which (b) structurally cannot.
>
> **Avoid (c)** (too invasive, duplicates CNXN) and **avoid mutating the writer loop for (d)** (risks the truncation regression). Handle first-OPEN by reopening at the backend.

This split keeps the **shared transport untouched** (blast radius minimal), fixes the **library-wide** CNXN race in the one shared handshake, and fixes the **production server `adb root`** path (CNXN + first-OPEN + brief-absence) at the one place that owns the connection lifecycle.

---

### Q4 — What does AOSP adb do on reconnect after adbd restarts?

AOSP's host transport layer is built around *automatic, backed-off reconnection*, which is the upstream intent our fix mirrors:
- `adb root`/`unroot` (`adb/client/adb_client.cpp` / `commandline.cpp` `adb_root`): after sending the `root:`/`unroot:` service and seeing a "restarting" reply, the client calls `wait-for-device` / `adb_wait_for_device`, i.e. it explicitly **waits for the transport to come back** before issuing the next command — it never assumes the device is immediately ready. (PR2 mirrored this with `wait-for-disconnect` + device-return.)
- The host server (`adb/transport.cpp`) runs a **reconnect handler**: when a transport's connection drops, `reconnect_device` / the `reconnect_handler` re-establishes it with a **bounded retry and a backoff delay** (the USB/TCP reconnect thread retries on an interval rather than failing the first attempt). The key upstream behavior is that *a single failed (re)open is not surfaced to the user* — the transport layer retries within a window, and only a sustained failure is reported.
- For USB specifically, AOSP's `usb_osx.cpp` / `transport_usb.cpp` re-scan + re-open is driven by IOKit hotplug notifications, and the first post-enumeration handshake is performed by the reconnect/attach machinery, not by the user command path racing the endpoint.

**Mirrored intent for our fix**: the *consumer waits for presence* (already done via `wait-for-disconnect` in PR2 / `wait_for_presence` in selftest), and the *transport open + first handshake retries within a bounded window* on transient errors — which is exactly option (b)+(secondary a). Our `CNXN_MAX_ATTEMPTS` loop is the analogue of AOSP's bounded reconnect retry; broadening it to transient transfer errors aligns us with AOSP's "don't surface a single transient (re)open failure" behavior.

> **Caveat**: I could not run live web/source searches (the exa MCP tools were unavailable in this environment); the AOSP specifics above are from the documented `transport.cpp` reconnect-handler design and the `adb_root`→`wait-for-device` flow. Verify the exact symbol names against the AOSP tree before quoting them in the PRD, but the behavioral shape (bounded retry + backoff on reconnect, wait-for-device after root) is stable across AOSP versions.

---

### Q5 — Cross-reference `.trellis/spec/backend/server-host-protocol.md`

The spec's **"Common Mistake: opening a USB device right after a case that re-enumerated it (selftest)"** (lines 504-538):
- Documents the symptom (`USB transfer error: unknown (error 0xe00002ed)`, 507) and cause (nusb re-opens under a fresh IOKit registry id, adbd not yet ready, 516-519).
- Prescribes the **consumer-layer** fix only: `open_device_with_retry` in the selftest (522-526), plus "hand the device back stable" (527-532).
- **Does NOT acknowledge the BACKEND gap.** Its "Prevention" (534-538) says "Never issue a bare device open immediately after an operation that restarts adbd... Use `open_device_with_retry`" — framed entirely as a *selftest/consumer* discipline. There is no mention that `DefaultDeviceBackend::get_or_open`/`open_local_service` issue exactly such a bare open with no retry, which is the production `adb root` path.
- **Minor inaccuracy to fix while here**: line 507 implies `0xe00002ed` is the headline code; that's fine, but if the spec ever names it `kIOReturnAborted` (the bug report does), correct it to **`kIOReturnNotResponding`** (Aborted is `0xe00002eb`).

**What the spec should say once the backend is fixed**:
- Add a backend-level subsection: the bridge's own open path (`get_or_open` → `new_from_serial`, and the first `open_session`) now retries transient post-re-enumeration transfer errors within a bounded budget, so consumers going *through the server* (the `adb root` reconnect path) no longer need their own retry.
- Record the corrected IOKit decode (`0xe00002ed` = NotResponding, `0xe00002c0` = NoDevice) and that adboost classifies transient-vs-permanent by `TransferError` variant + the `Unknown(code)` payload, bounded by a retry budget (never code-only).
- Keep the consumer-retry note for *direct library users* of `new_from_serial`/`new_from_ids` if option (b) is NOT taken — but if (b) IS taken, note that even direct users get the CNXN race handled for free, leaving only the "device momentarily not enumerated" (`DeviceNotFound`) case to the caller.
- This aligns with MEMORY `prefer-root-cause-fix-at-contract-layer` (fix the shared handshake/contract, not just the local selftest patch) and `tcp-async-path-missing-usb-guarantees` (one shared layer, contract tests).

---

### Q6 — Existing test/mock infra; seam for a "transient-then-success" open

**Two distinct test seams exist; neither currently has a fail-N-then-succeed transport:**

1. **`MockBackend`** (`frontend.rs:1496-1565`) implements the `DeviceBackend` trait directly (no USB). It is used only to exercise the host-protocol arms that don't bridge; `open_local_service` is `unimplemented!()` (1518). **This seam cannot test the connect/OPEN retry** because it bypasses `PersistentUsbConnection`/`USBTransport` entirely. It WOULD be the right seam to test a *backend-level* `get_or_open` retry policy if that policy were lifted onto a trait method, but as written `get_or_open` is a concrete `DefaultDeviceBackend` method bound to real USB enumeration.

2. **`PersistentConnection<T>` is generic over `T: ADBMessageTransport`** (`persistent.rs:357`). This is the clean seam: `do_connect`, `new`, `writer_loop`, `open_session` are all written against the trait. The existing `persistent.rs` tests (mod tests at 2537) exercise pure helpers (`classify_message`, `await_open_response`, flow control) and channel-level behavior — but there is currently **no scripted/mock `ADBMessageTransport`** in the tree (grep for `impl ADBMessageTransport` finds only `USBTransport` (usb_transport.rs:448) and `TcpTransport`). The only existing transport-level test indirection is `classify_read_result(Err(RustADBError::...))` (2640-2666), which feeds a synthetic error to the *classifier*, not the transport.

**Recommended seam to unit-test "transient-then-success" without hardware:**
Introduce a `#[cfg(test)]` mock implementing `ADBMessageTransport` (and the `ADBTransport::connect`/`disconnect` surface) that is *scripted*: e.g. a `VecDeque` of programmed `write_message`/`read_message` outcomes, so the first N `write_message` calls return `Err(RustADBError::UsbTransferError(TransferError::Unknown(0xe00002ed)))` (or `Disconnected`) and then succeed, with `read_message` returning a canned CNXN banner. Then assert `PersistentConnection::do_connect` (or `new_with_features`) succeeds after the transient writes. This is the natural place to lock the option-(b) behavior with a contract test. The `ADBMessageTransport` trait is the seam; the `try_new` message builder (`persistent.rs:2562-2564` test helper) and `MessageCommand::Cnxn` banner are reusable for the canned response.

For the **secondary (a) backend retry**, the cleanest testable shape is to factor the retry into a small free function/policy (`open_with_retry(budget, || new_from_serial(...))`-style, mirroring `open_device_with_retry`) so it can be unit-tested with a closure that fails N times then returns `Ok`, independent of `DefaultDeviceBackend`'s USB binding.

---

## Caveats / Not Found

- **IOKit code naming corrected** vs the task brief: `0xe00002ed` is `kIOReturnNotResponding` (not Aborted = `0xe00002eb`); `0xe00002c0` is `kIOReturnNoDevice`. Verified against `io-kit-sys-0.5.0/src/ret.rs:8,24,110,114` (the exact version pinned in `Cargo.lock`) and nusb's mapping `nusb-0.2.3/src/platform/macos_iokit/mod.rs:29-42`. This changes Q1's discrimination story (NoDevice → `Disconnected`, NotResponding → `Unknown(0xe00002ed)`).
- **Could not run live web/source search** (exa MCP tools unavailable). AOSP specifics (Q4) are from documented transport-reconnect design and the `adb_root`→wait-for-device flow, not freshly fetched source — verify symbol names against the AOSP tree before quoting them verbatim.
- **Whether to retry `TransferError::Disconnected` (NoDevice)** on connect is a design judgement (widens coverage to the "pipe momentarily gone" window vs. a few wasted retries on a real unplug); the bounded budget makes either choice safe, but the PRD should decide it explicitly.
- I did not enumerate every caller of `open_session` outside the server; the first-OPEN analysis (Q3d) is based on the server path (`open_local_service`/`open_sync_session`/`open_shell_v2`) and the selftest. Direct library users calling `open_session` themselves would need the same backend-style reopen-on-transient, which is out of scope unless option (b)+(a) are both taken.
