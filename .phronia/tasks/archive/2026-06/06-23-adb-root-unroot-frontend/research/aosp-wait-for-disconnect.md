# Research: AOSP `wait-for-...-disconnect` and the `adb root` reconnect handshake

- **Query**: How modern AOSP `adb` (35.x) implements `wait-for-<transport>-disconnect` and the `host:transport-id:<N>:wait-for-...` host-serial-style request used during `adb root` / `adb tcpip` reconnect; exact wire forms, reply framing, number of OKAYs, state/transport token sets, transport pinning, and timeout behavior.
- **Scope**: external (AOSP source, `platform/packages/modules/adb`, branch `refs/heads/main`, fetched 2026-06-23 from android.googlesource.com)
- **Date**: 2026-06-23

## TL;DR (answers to the 5 focus questions)

1. **Exact wire strings after `adb root`.** With a pinned transport id (the xdb case: `-s` resolved to a transport id), the client sends, in order:
   - `host-transport-id:<N>:wait-for-any-disconnect`
   - (then, only if `previous_id == 0`) `host-transport-id:` is NOT used; it would be `host:wait-for-any-device` / `host-serial:<serial>:wait-for-any-device`. **When a transport id was in use (`previous_id != 0`), the second `wait-for-...-device` is SKIPPED entirely** — see question 1 notes below. The xdb-observed `host:wait-for-any-device` corresponds to the `previous_id == 0` (no transport-id) path.
   - Note on prefix spelling: it is `host-transport-id:<N>:<sub>` on the wire (a host-serial-FAMILY prefix), **not** `host:transport-id:<N>:...`. `host:transport-id:N` (with a `host:` prefix) is the *transport-switch* service, a different thing. The wait sub-service uses the `host-transport-id:` family prefix. (xdb's report wrote it as `host:transport-id:N:...`; the real wire form is `host-transport-id:N:...`.)
   - `<transport>` is always explicitly inserted by the client (`any`/`usb`/`local`) — the client never sends a bare `wait-for-disconnect`; it expands to `wait-for-<transport>-disconnect`.

2. **Server reply and timing for `wait-for-...-disconnect`.** TWO bare `OKAY`s are sent, by two different code paths:
   - **First OKAY (immediate):** the smartsocket dispatcher, after deciding the service is a `host_service_to_socket` local service (wait-for is one), sends `SendOkay(s->peer->fd)` as the connection ack (sockets.cpp ~line 894). This happens immediately, before any disconnect.
   - **Second OKAY (on disconnect):** the `wait_service` thread sends `SendOkay(fd)` only once the wait condition is satisfied (the pinned transport is gone).
   - Both are **bare 4-byte `"OKAY"`** with NO length-prefixed payload (`SendOkay` = `WriteFdExactly(fd, "OKAY", 4)`).
   - Client reads exactly two statuses: `_adb_connect`'s internal `adb_status` reads the first OKAY; then `adb_command` calls `adb_status` again to read the second; then `ReadOrderlyShutdown` reads EOF. See "Client read sequence" below — this is the framing adboost must match exactly.

3. **Token sets.** `<transport>` ∈ {`any`, `usb`, `local`}. `<state>` ∈ {`device`, `recovery`, `rescue`, `sideload`, `bootloader`, `any`, **`disconnect`**}. `disconnect` IS a valid state token. It maps to the internal sentinel `kCsOffline` but is handled specially: it means "block until the (pinned) transport is torn down (gone from `transport_list`), regardless of USB vs TCP". (Note `rescue` exists too, in addition to the four the task listed.)

4. **Pinned to a specific transport?** YES. `wait_service` calls `acquire_one_transport(transport_type, serial-or-null, transport_id, ...)`. When `transport_id != 0` the match is strictly `t->id == transport_id`; when that transport is no longer in `transport_list`, `acquire_one_transport` returns `nullptr`, and the disconnect branch unblocks (`if (!t) SendOkay`). So semantics = "block until the exact transport I just talked to (transport-id N) is gone", NOT "any device disconnected".

5. **Timeout behavior.** The SERVER never times out — `wait_service` loops forever (100 ms poll) until the condition is met. The poll uses `adb_poll` on the client fd so it bails (SendFail) if the CLIENT closes the socket. The CLIENT imposes a timeout only for the *device-present* wait (`wait_for_device("wait-for-device", 12000ms)` spawns a 12 s watchdog thread that `_exit(1)`s). The `wait-for-disconnect` call has **NO client timeout** — `wait_for_device("wait-for-disconnect")` waits indefinitely.

## Findings

### Files Found (AOSP `packages/modules/adb`, branch `main`)

| File | Function / lines | Role |
|---|---|---|
| `services.cpp` | `wait_service` (~158–245), `host_service_to_socket` (`ConsumePrefix "wait-for-"`, ~260) | Server-side wait-for state machine and dispatch |
| `sockets.cpp` | smartsocket dispatch (~810–905): strips `host-serial:`/`host-transport-id:`/`host-usb:`/`host-local:`/`host:`, then `handle_host_request` → `host_service_to_socket`; sends immediate `SendOkay(s->peer->fd)` (~894) | Prefix parsing + first (connect) OKAY |
| `adb.cpp` | `handle_host_request` (1275+); transport-id parsing for the *transport-switch* service (1319) | Returns `Unhandled` for `wait-for-*` so it falls through to `host_service_to_socket` |
| `client/commandline.cpp` | `wait_for_device` (1021–1054), `adb_root` (1056–1145), `send_shell_command` (uses `wait-for-device`) | Client-side root/unroot reconnect handshake |
| `client/adb_client.cpp` | `format_host_command` (415), `switch_socket_transport` (81), `_adb_connect` (158), `adb_command` (~382), `adb_status` (137), `adb_get/set_transport` (60–67) | Client wire-form construction and status reads |
| `adb_io.cpp` | `SendOkay` (68), `SendFail` (72), `SendProtocolString` (37), `ReadOrderlyShutdown` (151) | Framing primitives |
| `adb.h` | `enum ConnectionState` (105–123) | State constants incl. `kCsOffline` (113) |
| `transport.cpp` | `acquire_one_transport` (912) | Transport selection / pinning by id |

### Code Patterns

**Wire-form construction — `format_host_command` (adb_client.cpp:415):**
```cpp
std::string format_host_command(const char* command) {
    if (__adb_transport_id) {
        return StringPrintf("host-transport-id:%" PRIu64 ":%s", __adb_transport_id, command);
    } else if (__adb_serial) {
        return StringPrintf("host-serial:%s:%s", __adb_serial, command);
    }
    const char* prefix = "host";
    if (__adb_transport == kTransportUsb)   prefix = "host-usb";
    else if (__adb_transport == kTransportLocal) prefix = "host-local";
    return StringPrintf("%s:%s", prefix, command);
}
```
So `wait_for_device("wait-for-any-disconnect")` with a pinned id yields exactly `host-transport-id:<N>:wait-for-any-disconnect`.

**Client `wait_for_device` (commandline.cpp:1021):** splits on `-`, requires `>=3` components, and INSERTS the transport token (`usb`/`local`/`any`) derived from the current transport if the caller didn't supply one. So the client always emits the `<transport>-<state>` form. `wait-for-disconnect` → `wait-for-<transport>-disconnect`.

**Root reconnect handshake — `adb_root` (commandline.cpp:1056):**
```cpp
unique_fd fd(adb_connect(&transport_id, StringPrintf("%s:", command), &error)); // "root:" / "unroot:"
// ... read up to 256 bytes of output ("restarting adbd as root" / "adbd is already running as root") ...
if (cur != buf && strstr(buf, "restarting") == nullptr) return true;  // no restart → done

adb_get_transport(&previous_type, &previous_serial, &previous_id);
adb_set_transport(kTransportAny, nullptr, transport_id);   // PIN to the transport id we just used
wait_for_device("wait-for-disconnect");                    // -> host-transport-id:N:wait-for-any-disconnect

if (previous_id == 0) {                                     // only if NOT pinned by id originally
    adb_set_transport(previous_type, previous_serial, 0);
    wait_for_device("wait-for-device", 12000ms);           // -> host[/host-serial]:wait-for-...-device
}
return true;
```
Key consequence for the xdb scenario (two devices, forced `-s`, so a transport id was in use): `previous_id != 0`, so the SECOND wait (`wait-for-...-device`) is NOT sent. The standalone `host:wait-for-any-device` xdb observed is the `previous_id == 0` path (single device, no `-s`).

Note: `command` is `"root"` or `"unroot"`; `adb_root` is the shared implementation for BOTH. The first service sent is `root:` / `unroot:` (a device-side local service, transparently forwarded), NOT a host service.

**Server wait state machine — `wait_service` (services.cpp:158):**
```cpp
// spec is the suffix after "wait-for-", e.g. "any-disconnect"
components = Split(spec, "-");                       // ["any","disconnect"]
// components[0] -> transport_type: local/usb/any
// components[1..] -> states:
//   device->kCsDevice, recovery->kCsRecovery, rescue->kCsRescue,
//   sideload->kCsSideload, bootloader->kCsBootloader, any->kCsAny,
//   disconnect->kCsOffline   <-- disconnect IS valid
while (true) {
    atransport* t = acquire_one_transport(transport_type, serial?:nullptr,
                                          transport_id, &is_ambiguous, &error);
    if (is_ambiguous) { SendFail(fd, error); return; }
    for (state : states) {
        if (state == kCsOffline) {            // the wait-for-disconnect special case
            if (!t) { SendOkay(fd); return; } // transport torn down -> unblock
        } else {
            if (t && (state == kCsAny || state == t->GetConnectionState())) {
                SendOkay(fd); return;
            }
        }
    }
    adb_pollfd pfd = {.fd = fd.get(), .events = POLLIN};
    if (adb_poll(&pfd, 1, 100) != 0) { SendFail(fd, error); return; } // client closed -> bail
}
```
The disconnect unblock condition is `!t` (the pinned transport is no longer in `transport_list`), NOT `state == kCsOffline` on a live transport. The `kCsOffline` mapping is only a sentinel to select the special branch. The comment in source explains TCP devices can transiently go `offline` and auto-reconnect; they only unblock when the transport object is fully torn down.

**Prefix stripping — sockets.cpp (~826):**
```cpp
if (ConsumePrefix(&service, "host-serial:")) { parse_host_service(&serial, &service, service); }
else if (ConsumePrefix(&service, "host-transport-id:")) {
    ParseUint(&transport_id, service, &service);    // N
    ConsumePrefix(&service, ":");                    // strip the ':' before the sub-service
}                                                    // service is now "wait-for-any-disconnect"
else if (ConsumePrefix(&service, "host-usb:"))   type = kTransportUsb;
else if (ConsumePrefix(&service, "host-local:")) type = kTransportLocal;
else if (ConsumePrefix(&service, "host:"))       type = kTransportAny;
// handle_host_request(...) -> Unhandled for wait-for-* -> host_service_to_socket(...)
// then: SendOkay(s->peer->fd);   // <-- FIRST (connect-ack) OKAY
```

**Framing primitives — adb_io.cpp:**
```cpp
bool SendOkay(borrowed_fd fd) { return WriteFdExactly(fd, "OKAY", 4); }        // bare 4 bytes
bool SendFail(borrowed_fd fd, std::string_view r) {                            // "FAIL" + %04x len + reason
    return WriteFdExactly(fd, "FAIL", 4) && SendProtocolString(fd, r); }
bool SendProtocolString(borrowed_fd fd, std::string_view s) {                  // %04x length prefix + payload
    auto str = StringPrintf("%04x", (unsigned)s.size()).append(s);
    return WriteFdExactly(fd, str); }
```

### Client read sequence (the contract adboost must satisfy for `host-transport-id:N:wait-for-...-disconnect`)

1. `adb_command(svc)` → `adb_connect` → `_adb_connect(svc)`.
2. In `_adb_connect`: because `svc.starts_with("host")` and not `force_switch`, **NO** `switch_socket_transport` step (no separate `host:tport:` round-trip). It does `SendProtocolString(fd, svc)` then `adb_status(fd)` — this consumes the **FIRST bare OKAY** (the smartsocket connect ack). `_adb_connect` returns the fd.
3. Back in `adb_command`: it calls `adb_status(fd)` AGAIN — this consumes the **SECOND bare OKAY** (sent by `wait_service` when the transport disconnects). This is the call that actually blocks for the duration of the wait.
4. `adb_command` then calls `ReadOrderlyShutdown(fd)` — expects the server to close the socket (read returns 0 / EOF).

So adboost's frontend, for a `wait-for-...-disconnect` request, must: (a) send a bare `OKAY` immediately as the connect ack, (b) send a second bare `OKAY` once the pinned device is gone, (c) then close the connection (orderly shutdown). No length-prefixed payload on either OKAY. On error (ambiguous/unknown), send `FAIL` + `%04x`-prefixed reason. This matches the existing `serve_wait_for` "two bare OKAYs" note in adboost; the new piece is the `disconnect` state and the pinned-serial semantics.

### Related internal code (adboost, for cross-reference)

| File | Relevance |
|---|---|
| `adboost/src/server/frontend.rs:543` `serve_wait_for` | Current impl: only `state == "device"`, polls by transport KIND, no pinned-serial/disconnect. Comment at 526–542 already documents the two-bare-OKAY framing. |
| `adboost/src/server/frontend.rs:857` `serial_for_transport_id` | Existing N→serial resolver (per PRD, reuse for host-transport-id routing). |
| `adboost/src/proxy/models/wait_for_device.rs` | `WaitForDeviceState` / `WaitForDeviceTransport` enums (no `Disconnect` variant yet). |
| `adboost/src/models/adb_host_command.rs:23` | `WaitForDevice(state, transport)` host command. |

## Caveats / Not Found

- **No live ADB_TRACE capture was obtained** (no captured `ADB_TRACE=all` log was available to fetch). The wire forms and OKAY counts above are derived directly from AOSP source on branch `main`, which is authoritative for adb 35.x behavior. The relevant trace tags are `SERVICES` (`VLOG(SERVICES) << "service request: '" << service << "'"` in sockets.cpp) and `RWX`.
- **Wire prefix correction:** the disconnect wait uses the `host-transport-id:<N>:` family prefix (no `host:` before it). xdb's report transcribed it as `host:transport-id:N:...`; verify against an actual capture, but source is unambiguous (`format_host_command` emits `host-transport-id:%llu:%s`). adboost must route the `host-transport-id:` top-level prefix (PRD R2) — consistent with this finding.
- **`disconnect` unblock is `!t` (transport gone), not a live `offline` state.** If adboost models "device present in list" the way native models `transport_list`, the analogue is "the pinned serial is no longer in `list_devices()`". A TCP device that merely goes offline but stays listed would NOT unblock native adb until its transport object is removed.
- **Server has no timeout; client has no timeout for the disconnect wait.** adboost's current `serve_wait_for` uses a 60 s `MAX_WAIT`; for the `disconnect` path that bound diverges from native (native waits forever, bailing only when the client closes the socket). This is the substance of open question [Q1] — flagging for the main agent / user, not prescribing a change.
- **`rescue` state token** exists in addition to device/recovery/sideload/bootloader/any (services.cpp). Not in the task's listed set; included for completeness.
