# Research: Native AOSP `adb root`/`unroot` disconnect detection (end-to-end) + adboost integration design

- **Query**: How does native AOSP adb detect "the transport I just talked to went away" so fast on `adb root`/`unroot` (measured 0.7–0.9s typ, ~4s worst, NEVER 60s)? Design an adboost fix that mirrors native event-driven timing, replacing the broken presence-polling `wait-for-disconnect`.
- **Scope**: mixed (extends prior AOSP research; new AOSP transport-teardown trace derived from source knowledge; adboost integration points verified against source @ HEAD)
- **Date**: 2026-06-23
- **Builds on**: `.trellis/tasks/archive/2026-06/06-23-adb-root-unroot-frontend/research/aosp-wait-for-disconnect.md` (the wire framing, two-OKAY contract, `wait_service` pinning, `acquire_one_transport`→null unblock). NOT redone here — read it first; this doc answers the NEW questions about the *mechanism and timing* of the teardown, and maps it onto adboost.

> Tooling note: the `mcp__exa__*` web/source-fetch tools named in the task are NOT available in this session. The AOSP file:line citations below are derived from (a) the prior research doc, which was fetched from `android.googlesource.com/platform/packages/modules/adb` branch `main` on 2026-06-23 and is authoritative, and (b) established knowledge of the AOSP adb transport layer. Where a citation is from memory rather than re-fetched, it is marked **(unverified — confirm against source)**. The adboost citations are all verified against the working tree.

---

## TL;DR (answers to the 6 new questions)

1. **What tears down the host-side transport on `adb root`?** The **transport's read pump erroring out at the I/O layer**, NOT a device-list poll and NOT (primarily) a kernel hotplug notification. When adbd execs/restarts, it closes its end of the USB function (or the kernel tears the FunctionFS endpoints), so the host's bulk-IN read returns an error/short read. The transport's `read_thread` / connection then calls `transport->Kick()` → the transport is unregistered and removed from `transport_list`. This is detected at the **connection/transport I/O layer** — confirmed below.

2. **Timing chain.** `root:` reply ("restarting adbd as root") → adbd closes the USB connection → host read pump errors **within milliseconds** → `Kick()` → transport unregistered → removed from `transport_list` → `wait_service`'s next `acquire_one_transport(transport_id)` (≤100 ms poll) returns null → second `OKAY` sent → client returns. Sub-second because it waits on the **OLD transport object dying**, which is an immediate I/O event, NOT on USB re-enumeration or the NEW transport appearing.

3. **Does native wait for the device to come BACK?** **No** — in the transport-id-pinned case (`previous_id != 0`) `adb_root` SKIPS the follow-up `wait-for-device`. It waits only for DISCONNECT (the old transport to GO), never for reconnect. That is precisely why it returns sub-second: it does not wait for adbd to finish restarting / the device to re-enumerate.

4. **adboost analogue.** Native's "read pump errors out" == adboost's `PersistentUsbConnection` **reader task dying** (`reader_loop` breaks on a fatal `ReadError`, `persistent.rs:1116-1117`; `is_alive()` then false, `persistent.rs:1804-1810`). Today this death is observable ONLY by polling `is_alive()`; there is **no event emitted on reader death**. The existing `LifecycleEvent::Disconnected` fires from `spawn_usb_disconnect_watch` via nusb **hotplug** (`default_backend.rs:437-467`) — which does NOT fire on adbd restart (no re-enumeration on many devices, e.g. MTK). A NEW connection-death signal is needed.

5. **Design recommendation: Form 2 (push event) — `LifecycleEvent::TransportReset{serial}` (or `Reconnected`).** Emit it the instant the cached connection's reader task dies. It is genuinely event-driven (sub-second, like native); Form 1 (pull `transport_epoch`) reintroduces polling latency unless polled fast, so it does NOT match native. Reuse the existing `subscribe_lifecycle()` broadcast seam (`backend.rs:228`, `default_backend.rs:487`). `serve_wait_for`'s disconnect branch subscribes and `select!`s the matching serial's reset event vs a **bounded fallback timeout of ~5–10s** (down from 60s). Drop presence-polling as the primary mechanism; optionally keep a coarse presence check as a secondary fallback only.

6. **`do_connect` retry interaction.** The disconnect SIGNAL is the reader **death**, which fires BEFORE/INDEPENDENT of any reopen. The reopen happens later, lazily, on the NEXT `open_local_service`→`get_or_open` (`default_backend.rs:324-361`, which already rides the re-enumeration window via `retry_within`). So wait-for-disconnect unblocks on the death (native semantics: unblock on disconnect, not reconnect). The signal must be emitted from the reader-death observation point, NOT from a successful reopen.

---

## Findings

### Part 1 — Native AOSP: what tears down the transport, and why it's sub-second

#### 1a. The teardown is an I/O-layer event, not a list poll

AOSP's host transport for USB runs a dedicated **read loop** that owns the bulk-IN endpoint (the direct structural analogue of adboost's `reader_loop`). Each transport has a `Connection` (`UsbConnection` / `BlockingConnectionAdapter`) whose read thread loops on `connection->Read(&packet)`. When the underlying bulk read fails (adbd closed the function, or the kernel tore the endpoints), `Read` returns false/error and the read loop calls the transport's error/kick path.

- **`transport.cpp` — `transport_registration_func` / `remove_transport` / `transport->Kick()` / `kick_transport`** and the read-thread error path that unregisters the transport and removes it from the global `transport_list` (guarded by `transport_lock`). The removal from `transport_list` is the single event that makes `acquire_one_transport(transport_id)` start returning null. **(citation family verified conceptually via prior research's `acquire_one_transport` at `transport.cpp:912`; the exact `remove_transport`/`Kick` line numbers are unverified — confirm against source.)**
- **`BlockingConnectionAdapter`** (in `transport.cpp`/`adb_connection`) — wraps the platform connection; its read thread calls the registered error callback `→ transport->HandleError()` when `Read` fails, which kicks the transport. **(unverified — confirm.)**
- **Platform USB read primitive — `usb_osx.cpp` `usb_read` (macOS) / `usb_linux.cpp` `usb_read` / `usb_windows.cpp`.** On adbd restart the bulk-IN transfer completes with an error (`kIOReturnNotResponding` / `kIOReturnNoDevice` on macOS; `-EPIPE`/`-ENODEV`/short read on Linux). This is exactly the same low-level error family adboost already classifies as transient at *connect* time (`IOKIT_NOT_RESPONDING = 0xe00002ed`, `TransferError::Disconnected`; see `persistent.rs:101-146`). The key point: the host learns adbd is gone because **its in-flight bulk read fails**, immediately — not because a separate poller noticed the device left a list. **(usb_osx error codes corroborated by adboost's own `is_transient_connect_error`; exact `usb_read` lines unverified.)**

**Conclusion (confirms the task's key insight):** native adb's "disconnect" for `adb root` is detected at the **connection/transport I/O layer** — the read pump errors out the moment adbd closes the connection — NOT by polling a device list. `wait_service` then merely *observes* the already-completed teardown via `acquire_one_transport`→null on its ≤100 ms poll. The poll is the cheap *observation*; the *signal* is the I/O error.

#### 1b. Whether a kernel USB disconnect even occurs

Two sub-cases, both sub-second on the host side:

- **adbd restarts WITHOUT re-enumeration** (common on MTK and others): the USB device stays enumerated (serial never leaves the kernel's device list), but adbd closes/reopens its FunctionFS endpoints. The host's bulk-IN read still errors (the endpoint's pending transfer is cancelled / the pipe stalls), so the read pump dies anyway. **This is the case that breaks adboost's presence-poll** — the serial never disappears from `list_devices()`.
- **adbd restart WITH a brief re-enumeration**: a transient USB disconnect/reconnect occurs; the read also errors, plus a hotplug event fires. Either way the read-pump error is the prompt signal.

In **both** cases native unblocks on the transport-object teardown driven by the read error, so its timing does not depend on which sub-case occurred. (This is why native is robust where adboost's presence-poll is not.)

#### 1c. The exact timing chain and why it is sub-second

```
t0    client sends   root:        (commandline.cpp adb_root, ~:1056)
      adbd replies   "restarting adbd as root\n"   (~256 bytes, client reads it)
      adbd exec()s / restarts adbd → closes the USB function
t0+ε  host bulk-IN read for the OLD transport ERRORS  (usb_*.cpp usb_read fails)
      → BlockingConnectionAdapter read thread → transport->Kick()/HandleError()
      → remove_transport(): erase from transport_list under transport_lock
t0+ε  wait_service loop (services.cpp:158) on its NEXT iteration (≤100 ms poll):
      acquire_one_transport(transport_id=N) → nullptr (N no longer in list)
      → disconnect branch: if (!t) SendOkay(fd); return;   (the SECOND OKAY)
t0+δ  client's adb_command second adb_status read returns → adb_root returns
```

- `ε` (read error → list removal) is **milliseconds**: a cancelled/failed in-flight bulk transfer resolves promptly; `remove_transport` is a locked list erase.
- The ≤100 ms `wait_service` poll adds at most one poll interval of latency to *observe* the teardown.
- Measured 0.7–0.9s typ is dominated by: the `root:` round-trip + adbd printing its banner + adbd actually beginning to exec (closing the connection). The ~4s worst case is adbd being slow to restart/close. It is **never** 60s because nothing waits for the device to come back — see 1d.

#### 1d. Native waits only for DISCONNECT (GO), never for reconnect (COME BACK)

From the prior research (`adb_root` at `commandline.cpp:1056`, transcribed there):

```cpp
adb_get_transport(&previous_type, &previous_serial, &previous_id);
adb_set_transport(kTransportAny, nullptr, transport_id);   // PIN to the id we just used
wait_for_device("wait-for-disconnect");                    // host-transport-id:N:wait-for-any-disconnect

if (previous_id == 0) {                                     // ONLY if not pinned by id originally
    adb_set_transport(previous_type, previous_serial, 0);
    wait_for_device("wait-for-device", 12000ms);           // wait for it to come back
}
return true;
```

- When a transport id was in use (`previous_id != 0` — the multi-device `-s` case xdb hits), the second `wait-for-device` is **SKIPPED**. `adb root` returns as soon as the OLD transport is gone. It does **not** wait for adbd to finish restarting, for USB re-enumeration, or for the NEW transport to register.
- Even in the `previous_id == 0` path, the `wait-for-disconnect` itself completes on teardown; the *separate* follow-up `wait-for-device` is the only part that waits for return, and it has its own 12s client watchdog (it is not the disconnect wait).

**This is the design north star for adboost**: the disconnect wait must unblock on the OLD connection dying, and must NOT wait for the reopen/return.

---

### Part 2 — Mapping onto adboost (all citations verified against the working tree)

#### 2a. Where the reader task dies (the native-equivalent signal source)

`PersistentConnection::reader_loop` (`adboost/src/message_devices/usb/persistent.rs:1048-1219`):

- It loops on `read_or_control` → `read_message_with_timeout(1s)` (`persistent.rs:1230-1252`).
- A normal idle read timeout (`RustADBError::ReadTimeout`) just `continue`s (`persistent.rs:1086`) — it does NOT kill the reader. **Important for the design:** adbd-restart must surface as a *fatal* read error, not a timeout, for the reader to die promptly. The low-level USB transfer error on adbd close maps to a `RustADBError::UsbTransferError(Disconnected | Unknown(0xe00002ed))` (the same family `is_transient_connect_error` lists, `persistent.rs:138-146`), which is NOT `ReadTimeout`, so it lands in the fatal arm:
  ```rust
  // persistent.rs:1112-1117
  if matches!(e, RustADBError::InvalidIntegrity(..)) { ... continue; }
  tracing::warn!("PersistentUsb reader error (fatal): {e}");
  break;            // <-- reader task exits here on adbd close
  ```
  After `break`, the task ends (`persistent.rs:1218 "PersistentUsb reader task exiting"`), so `reader_handle.is_finished()` becomes true and `is_alive()` returns false (`persistent.rs:1804-1810`).
- The writer task can also die fatally (`writer_loop` `persistent.rs:1344-1347`); `is_alive()` requires BOTH alive (`persistent.rs:1809`).

**This `break` at `persistent.rs:1116-1117` is the precise analogue of native's read-pump-errors-out → `Kick()`.** It is the natural place to emit a connection-death signal.

#### 2b. Is the death observable as an event, or only by polling?

**Today: only by polling `is_alive()`.** There is no channel/callback fired when the reader breaks. Evidence:
- `is_alive()` (`persistent.rs:1803-1810`) is a pure poll over `JoinHandle::is_finished()`.
- The only callers of `is_alive()` are in `default_backend.rs` (`:302, :330, :355, :388, :632`) — all *lazy reaping* on the next `get_or_open`/`tcp_conn`/`device_capabilities`. None of them run on a timer; they only notice the death when someone next touches the cache. So a `wait-for-disconnect` that polled `is_alive()` would still need its own poll loop.
- The `LifecycleEvent::Disconnected` path is driven by **nusb hotplug** (`spawn_usb_disconnect_watch`, `default_backend.rs:437-467`) — physical unplug only. It re-enumerates serials on each hotplug event and diffs (`default_backend.rs:451-463`). On adbd restart without re-enumeration, **no hotplug event fires and the serial never leaves the set**, so `Disconnected` is never emitted. Confirmed: this is exactly why the current presence-poll hangs 60s.

**To make it event-like:** the reader task itself can emit an event on exit. The connection would need a sender (e.g. an `mpsc`/`broadcast` handle, or a callback) passed into `reader_loop`, fired right before/at the `break`. Because the connection layer (`persistent.rs`) is transport-generic and serial-agnostic (it does not know its own serial), the cleanest seam is: the connection exposes an **exit/closed notification** that the *backend* (which owns the serial→conn mapping) turns into a per-serial `LifecycleEvent`. Two concrete shapes:
  - The connection holds a `closed: tokio::sync::Notify` (or a `oneshot`/`broadcast`) that the reader/writer fire on exit; `default_backend` spawns a tiny task per cached connection that awaits it and calls `emit_*` with the serial. OR
  - `default_backend` spawns a watcher per connection that `await`s the `reader_handle` JoinHandle completing — but the handles live inside the connection and are private; the connection currently aborts them on Drop. A `Notify`-on-exit is cleaner than exposing handles.

#### 2c. Where a NEW connection-death signal would be emitted

- **Crate-internal source of truth:** the `reader_loop` fatal `break` (`persistent.rs:1116`) and the writer fatal `break` (`persistent.rs:1346`), i.e. "this connection's I/O died". A `Notify`/`broadcast` set on the `PersistentConnection` and signaled at both exit points captures "connection died" regardless of which half failed (matching `is_alive()`'s both-halves semantics).
- **Backend → frontend signal:** `DefaultDeviceBackend` maps the connection death to a per-serial lifecycle event. It already has `emit_disconnected(serial)` (`default_backend.rs:422-427`) publishing onto the `lifecycle` broadcast. A new `emit_transport_reset(serial)` (or reuse `Disconnected` — see trade-off in 2e) would publish a NEW `LifecycleEvent` variant. The backend knows the serial (it is the `conns` map key, `default_backend.rs:125`), so the per-connection watcher task it spawns can name the serial when the connection's `closed` notify fires.

#### 2d. How `serve_wait_for` currently consumes the signal (and how it would change)

Current disconnect branch (`frontend.rs:656-674`):
- 200 ms presence poll of `list_devices()` until the pinned serial is absent, 60s `MAX_WAIT` (`frontend.rs:638-639`). This is the broken path.
- It does NOT subscribe to lifecycle today; it has its own poll loop.

Event-driven replacement (Form 2):
```text
// disconnect branch:
let mut events = self.backend.subscribe_lifecycle().await;   // backend.rs:228 seam
let deadline = Instant::now() + DISCONNECT_FALLBACK;          // ~5-10s, not 60s
loop {
    select! {
        ev = events.recv() => match ev {
            Some(TransportReset(s)) | Some(Disconnected(s)) if matches(pinned_serial, &s) => break OKAY,
            Some(_) => continue,                 // other serial, keep waiting
            None => break,                       // stream closed (server teardown)
        }
        _ = sleep_until(deadline) => break OKAY-or-FAIL,   // bounded fallback (see 2e)
    }
}
// then okay_twice()  (Bug 1 fix, R1)
```
- Because `subscribe_lifecycle` returns an `mpsc::Receiver<LifecycleEvent>` (`backend.rs:228`, bridged from a `broadcast` in `default_backend.rs:487-512`), the frontend can `select!` it against a timeout with no polling. This is the architecture-consistent seam the PRD's Decision Q1 already leans toward (push model).
- **Race-free subscribe:** subscribe BEFORE the disconnect could be observed. In the `adb root` flow the `wait-for-disconnect` request arrives AFTER `root:` was sent on a (possibly different) connection, so there is a window where the reader could already have died. To be safe, after subscribing, do one initial `is_alive`/presence check (or have the backend expose a cheap `transport_epoch`/`is_present` read) so an already-completed teardown is not missed while waiting only for a future event. (This is the one place a cheap pull check complements the push event — see 2e.)

#### 2e. Form 1 (pull epoch) vs Form 2 (push event) — recommendation

| | Form 2 — push `LifecycleEvent::TransportReset{serial}` | Form 1 — pull `transport_epoch(serial)->Option<u64>` |
|---|---|---|
| Event-driven? | **Yes** — fires the instant the reader dies; frontend `select!`s, zero polling. Matches native (I/O-event-driven). | No — frontend must poll the epoch; latency = poll interval. Re-introduces the exact latency class the bug is about unless polled very fast. |
| Sub-second? | Yes (bounded only by broadcast delivery). | Only if polled at e.g. 50–100 ms; still a poll. |
| Architecture fit | Reuses existing `subscribe_lifecycle` broadcast seam (`backend.rs:228`, `default_backend.rs:487`) + `handle_disconnects` pattern. | New trait method; frontend grows a new poll loop (the thing we are removing). |
| Default-impl safety | Default `subscribe_lifecycle` returns a closed stream (`backend.rs:228-234`) → an un-adapted backend's wait-for-disconnect sees no event and falls through to the bounded fallback (old-ish behavior, bounded). Non-breaking. | Default could return `None` (epoch unknown) → frontend falls back to presence poll. Also non-breaking but keeps the poll. |
| Missed-already-happened race | Needs a one-shot initial check on subscribe (see 2d). | Naturally handles it: record entry epoch, unblock when it changes — no missed-edge problem. |

**Recommendation: Form 2 (push event).** It is the only one that is genuinely event-driven and sub-second like native, and it reuses the existing `subscribe_lifecycle` seam (matching PRD Decision Q1). Address Form 2's one weakness (an already-completed teardown before subscribe) with a single cheap initial check after subscribing — NOT a continuous poll. Form 1's only real advantage (no missed-edge) is better solved by that one-shot check than by a permanent poll loop.

A **hybrid that keeps the poll** (Form 1 polled fast) is explicitly NOT recommended: the PRD's whole point (Q2) is to stop polling and mirror native's event semantics.

**Naming:** prefer a NEW variant `LifecycleEvent::TransportReset(String)` (or `Reconnected`) distinct from `Disconnected`, because:
- `Disconnected` currently means "gone for good — release forward/reverse rules" and `handle_disconnects` (`frontend.rs:1466-1483`) RELEASES rules on it. An adbd restart is NOT a permanent disconnect (the forward/reverse rules should arguably survive the reconnect), so reusing `Disconnected` would wrongly tear down forwards on every `adb root`. A separate `TransportReset` variant lets `serve_wait_for` unblock on it WITHOUT `handle_disconnects` releasing rules. `LifecycleEvent` is a plain enum (`backend.rs:135-141`); adding a variant is a (minor, internal) change — `handle_disconnects`'s `while let Some(Disconnected(..))` (`frontend.rs:1466`) already ignores non-`Disconnected` variants by pattern, but note adding a variant makes that `while let` non-exhaustive-by-design (it silently drops other variants — acceptable, but call it out).

#### 2f. Bounded fallback timeout

- Native effectively never hangs and never times out the disconnect wait server-side (it loops forever bailing only if the client closes — prior research Q5). But adboost cannot loop forever safely (a leaked task / a `root:` that did NOT actually restart adbd). 
- **Recommendation: 5–10s bounded fallback** (PRD says native recovers within ~5s). On the event path it essentially never fires; it exists only for "adbd did not actually restart / signal never arrived". Far shorter than the current 60s.
- On fallback expiry, prefer to still send the **two OKAYs** (treat as "assume disconnected / give up waiting") rather than FAIL, to match the PRD acceptance criterion that `root`/`unroot` returns cleanly — UNLESS the team prefers FAIL to surface a stuck device. Flag for the main agent: this is a product decision (clean-return vs. honest-FAIL on the rare no-restart case).
- **Presence-poll as secondary fallback:** OPTIONAL. It is harmless to keep a single coarse presence check on the fallback path (if the serial is already absent, definitely disconnected), but it must NOT be the primary mechanism. Recommendation: DROP the continuous presence poll; the event is primary, the timeout is the only fallback. A one-shot presence/`is_alive` check at subscribe time covers the already-happened race.

#### 2g. `do_connect` retry interaction — signal fires on death, not reopen

Confirmed independence:
- The reader death (signal source, `persistent.rs:1116`) happens on the OLD cached connection when adbd closes. Nothing reopens at that moment.
- The reopen is LAZY and LATER: it happens only when the NEXT `open_local_service(serial, ..)` calls `get_or_open` (`default_backend.rs:566-595` → `:324-361`), which reaps the dead connection (`:330-336`) and opens a fresh one via `retry_within(OPEN_RETRY_BUDGET, ...)` (`:341-348`) — riding the re-enumeration window per `is_retryable_open_error` (`:99-115`).
- Therefore the disconnect signal (reader death) **fires before and independently of** the reopen. wait-for-disconnect unblocks on the death — matching native (unblock on disconnect, not reconnect). The reopen, when it happens, is for the *next* command's session, not for satisfying the wait.
- Edge to verify in implementation: if `serve_wait_for`'s subscribe happens after a different code path already reopened a fresh connection for the same serial (so a NEW live connection is cached), the OLD reader's death event must still have been emitted (and either consumed or the one-shot initial check must compare against a transport *generation*, not just `is_alive` of whatever is currently cached). This is the strongest argument for tagging the event/epoch with a **generation/transport-id** so a fast reopen cannot mask the death. (Form 2 with a `TransportReset(serial)` event delivered at death time avoids this if the subscriber is attached before the death; the one-shot initial check should therefore compare a cheap monotonic generation counter, i.e. a *minimal* Form-1 read used ONLY for the initial-edge check, not for polling.)

---

## Concrete integration points (adboost, verified)

| Location | Role in the fix |
|---|---|
| `adboost/src/message_devices/usb/persistent.rs:1116-1117` | Reader fatal `break` — native-equivalent "read pump errored out". Emit connection-death notify here. |
| `adboost/src/message_devices/usb/persistent.rs:1344-1347` | Writer fatal `break` — other half of connection death. |
| `adboost/src/message_devices/usb/persistent.rs:1803-1810` | `is_alive()` — current poll-only death observation; basis for an initial-edge check / generation. |
| `adboost/src/server/default_backend.rs:125` | `conns: Mutex<HashMap<String, Arc<PersistentUsbConnection>>>` — serial→conn map; where a per-connection death-watcher would be spawned and learn the serial. |
| `adboost/src/server/default_backend.rs:324-361` | `get_or_open` — reaps dead conn, reopens with `retry_within`; the LAZY reopen (independent of the death signal). |
| `adboost/src/server/default_backend.rs:407-427` | `lifecycle_tx` / `emit_disconnected` — the broadcast publish point; add `emit_transport_reset` here. |
| `adboost/src/server/default_backend.rs:437-467` | `spawn_usb_disconnect_watch` — hotplug-only; the reason adbd-restart is NOT seen today. |
| `adboost/src/server/backend.rs:135-141` | `enum LifecycleEvent` — add `TransportReset(String)` variant. |
| `adboost/src/server/backend.rs:228-234` | `subscribe_lifecycle` trait method (+ closed-stream default) — the push seam `serve_wait_for` would consume. |
| `adboost/src/server/frontend.rs:656-674` | `serve_wait_for` disconnect branch — replace presence poll with `subscribe_lifecycle` + `select!` + bounded timeout; emit `okay_twice` (Bug 1, R1). |
| `adboost/src/server/frontend.rs:665, 696` | the single `protocol::okay()` sites — change to two bare OKAYs (Bug 1). |
| `adboost/src/server/frontend.rs:1461-1485` | `handle_disconnects` — only matches `Disconnected`; a new `TransportReset` variant is (intentionally) ignored here so rules are NOT released on adbd restart. |
| `adboost/src/server/protocol.rs:128-130` | `okay_twice()` — the exact helper to reuse for the wait-for double-OKAY. |

---

## Caveats / Not Found

- **AOSP line numbers for the teardown path are partly unverified.** The MCP web/source tools were unavailable this session. The *mechanism* (read-pump error → `Kick`/`remove_transport` → out of `transport_list` → `acquire_one_transport`→null) is well established and consistent with the prior (fetched) research's `acquire_one_transport@transport.cpp:912` and `wait_service@services.cpp:158`. Confirm exact lines for `remove_transport` / `kick_transport` / `BlockingConnectionAdapter` read-thread error callback / `usb_osx.cpp usb_read` against `platform/packages/modules/adb` branch `main` before quoting them in the PRD as verified.
- **The reader must fail FATALLY (not idle-timeout) on adbd close** for the death to be prompt. This is true today for the `UsbTransferError(Disconnected|Unknown(0xe00002ed))` family (`persistent.rs:138-146`, fatal arm at `:1116`). If some adbd-restart variant instead yields repeated `ReadTimeout` (the read just stalls without erroring), the reader would NOT die and the event would not fire — verify on real MTK hardware that adbd close surfaces a fatal transfer error, not a silent stall. If it can stall, an explicit liveness probe may be needed (out of scope to design here, but flag it).
- **Already-happened race** (teardown completes before `serve_wait_for` subscribes): must be covered by a one-shot initial check at subscribe time, ideally against a monotonic transport-generation counter rather than `is_alive()` of the currently-cached connection (a fast reopen could otherwise mask the death). This argues for a tiny generation field even in a primarily-push design — used ONLY for the initial edge, never polled.
- **`okay_twice` for the disconnect path** (Bug 1, R1) is orthogonal to the signal redesign (Bug 2) but both touch the same `serve_wait_for` disconnect branch — sequence the edits so the FAIL/timeout path stays a SINGLE FAIL (R2).
- **Variant naming / rule-release semantics**: reusing `Disconnected` would make `handle_disconnects` release forward/reverse rules on every `adb root` (wrong). A distinct `TransportReset`/`Reconnected` variant avoids that; confirm with the maintainer whether forwards should survive an adbd restart (native keeps the host-side forward listeners; the device side is re-established).
- **Not found / not applicable**: no existing reader-exit event channel in `persistent.rs` (only `is_alive()` polling + Drop-time abort); no per-connection death watcher in `default_backend.rs` today (deaths are reaped lazily). These are net-new in the design.
