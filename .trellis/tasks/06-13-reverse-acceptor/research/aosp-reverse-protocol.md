# Research: AOSP `adb reverse` protocol (host-side acceptor role)

- **Query**: How does AOSP implement host-side `reverse:` port-forwarding for an ADB server frontend? Exact wire strings, A_OPEN/A_OKAY/A_CLSE framing, delayed_ack windowing for the acceptor role, kill/list framing, overall lifecycle.
- **Scope**: external (AOSP `platform/packages/modules/adb`, branch `main`, fetched 2026-06-13)
- **Date**: 2026-06-13

## Source files fetched (android.googlesource.com `.../adb/+/refs/heads/main`)

| File | Role |
|---|---|
| `client/commandline.cpp` | adb CLI command parsing (`reverse` / `forward`) |
| `client/adb_client.cpp` | `_adb_connect` — transport selection + service send |
| `adb.cpp` | `handle_packet` (A_OPEN/A_OKAY/A_CLSE/A_WRTE), `send_ready`, `send_close`, `handle_forward_request` (shared host+adbd) |
| `sockets.cpp` | smart-socket routing, `connect_to_remote`, `create_remote_socket`, `local_socket_*`, windowing |
| `transport.cpp` | `UpdateReverseConfig` / `IsReverseConfigured` (host allow-list) |
| `daemon/services.cpp` | `reverse_service` (adbd side) |
| `adb_listeners.cpp` | `install_listener`, `listener_event_func`, `format_listeners` |
| `adb.h` | constants (`A_*`, `INITIAL_DELAYED_ACK_BYTES`, `A_VERSION_SKIP_CHECKSUM`) |

Key constants (`adb.h:33-58`):
```
MAX_PAYLOAD_V1            = 4 KiB
MAX_PAYLOAD              = 1 MiB
INITIAL_DELAYED_ACK_BYTES = 32 * 1024 * 1024   // 32 MiB
A_OPEN = 0x4e45504f  A_OKAY = 0x59414b4f  A_CLSE = 0x45534c43  A_WRTE = 0x45545257
A_VERSION_SKIP_CHECKSUM = 0x01000001  (== A_VERSION)
```

---

## 1. CLIENT SIDE — what `adb reverse tcp:5201 tcp:5201` sends

`client/commandline.cpp:1910-1966`. For `reverse`, `host_prefix = "reverse:"` (NOT `host:`). Argument order is `reverse [--no-rebind] REMOTE LOCAL` and the built command (commandline.cpp:1942-1948) is:

```cpp
// reverse <remote> <local>   ->   "forward:<argv[0]>;<argv[1]>"
cmd = std::string("forward:") + argv[0] + ";" + argv[1];
// --no-rebind:  "forward:norebind:<remote>;<local>"
```

So the full smartsocket **local service** string is:

```
reverse:forward:[norebind:]<remote>;<local>
```

- `<remote>` = the port **the device listens on** (REMOTE = device side).
- `<local>` = the **host-side connect target** (LOCAL = host side).
- Argument order is therefore **opposite** to `host:forward:<local>;<remote>` (forward lists host-local first; reverse lists device-remote first). For `adb reverse tcp:5201 tcp:5201` both happen to be `5201` so the string is `reverse:forward:tcp:5201;tcp:5201`.

This is sent **after** transport selection. `client/adb_client.cpp::_adb_connect` (lines 157-194):
```cpp
if (!service.starts_with("host") || force_switch) {
    switch_socket_transport(fd, error);   // sends host:transport-id:<N> (or host:tport:...)
}
SendProtocolString(fd, service);          // then sends "reverse:forward:..."
adb_status(fd, error);                    // reads OKAY/FAIL
```
`reverse:` does not start with `host`, and the reverse handler calls `adb_connect(nullptr, host_prefix+cmd, &error, /*force_switch=*/true)` (commandline.cpp:1956), so a transport is always selected first via `host:transport-id:<N>` (`switch_socket_transport`, adb_client.cpp:81-92). The `reverse:forward:...` is a **device-bound local service**, not a host service.

---

## 2. What the SERVER does on receiving `reverse:forward:...`

The server does NOT handle it locally — it **forwards the whole `reverse:forward:...` string to the device (adbd) as an A_OPEN destination** over the selected transport.

`sockets.cpp::smart_socket_enqueue` (lines 820-934): the service is checked for `host`/`host-serial:`/`host-transport-id:`/etc. prefixes. `reverse:...` matches none, so `service` is cleared and control falls through to:
```cpp
s->peer->ready    = local_socket_ready_notify;   // will SendOkay(fd) when device connects
s->peer->close    = local_socket_close_notify;   // SendFail(fd,"closed") on failure
s->peer->transport = s->transport;
connect_to_remote(s->peer, std::string_view(s->smart_socket_data).substr(4)); // sends A_OPEN("reverse:forward:...")
```

`sockets.cpp::connect_to_remote` (lines 560-590) snoops the request on the host before sending:
```cpp
#if ADB_HOST
    s->transport->UpdateReverseConfig(destination);  // record reverse_forwards_[remote] = local
#endif
    p->msg.command = A_OPEN; p->msg.arg0 = s->id;
    if (transport->SupportsDelayedAck()) { p->msg.arg1 = INITIAL_DELAYED_ACK_BYTES; s->available_send_bytes = 0; }
    payload = destination + '\0';
```

On the device, `daemon/services.cpp::reverse_service` (lines 69-83) runs the **shared** `handle_forward_request` against a socketpair and streams the reply back through the tunnel.

**Reply framing to the client.** Two distinct OKAYs reach the client, from two different places:
1. The server's smart-socket sends one bare `OKAY` via `local_socket_ready_notify` → `SendOkay(s->fd)` (sockets.cpp:597-603) once the device service connects — this is the "connect" status the client reads in `adb_status`.
2. The device's `handle_forward_request` for the `forward:` branch (adb.cpp:1196-1219) replies — and on the DEVICE side `#if ADB_HOST` is FALSE, so it sends **only ONE** `SendOkay(reply_fd)` (plus an optional resolved port). The host-only double-OKAY (`adb.cpp:1209-1211`) does NOT fire on adbd.

So over the wire the client sees: `OKAY` (server connect) then `OKAY` (device status), optionally followed by `%04x`+ASCII-decimal port when the device-listen endpoint was `tcp:0`:
```cpp
SendOkay(reply_fd);                       // device status (single, adbd side)
if (resolved_tcp_port != 0)
    SendProtocolString(reply_fd, StringPrintf("%d", resolved_tcp_port));
```
The client reads status via `adb_status` then `ReadProtocolString(fd, &resolved_port, ...)` and prints the port if non-empty (commandline.cpp:1958-1964). So: effectively **OKAY (connect) + OKAY (status) [+ %04x<decimal-port>]**.

> Implication for our server frontend: when WE are the server, we must (a) send our own bare connect-OKAY to the client once we've opened the `reverse:forward:...` service on the device, and (b) relay the device's status OKAY (and optional resolved-port protocol string) verbatim. Do NOT synthesize the host-style double-OKAY ourselves on top of the device's reply; the second OKAY comes FROM the device.

---

## 3. DEVICE-INITIATED OPEN — exact message when something connects to the reversed port

The listener lives on the device. `adb_listeners.cpp::install_listener(local_name=<remote>, connect_to=<local>, ...)` (lines 190-242) binds `local_name` (the device port, `pieces[0]`) and stores `connect_to` = `pieces[1]` (the host target). On an inbound connection, `listener_event_func` (lines 97-113):
```cpp
unique_fd fd(adb_socket_accept(_fd, ...));
asocket* s = create_local_socket(std::move(fd));
s->transport = listener->transport;
connect_to_remote(s, listener->connect_to);   // connect_to == <local> == host target
```

`connect_to_remote` (sockets.cpp:560-590) then sends to the host:

```
A_OPEN(arg0 = device_local_id, arg1 = window, payload = "<local>\0")
```

- **arg0** = the device's newly-allocated local socket id (`s->id`, nonzero; allocator `install_local_socket` sockets.cpp:81-92, `local_socket_next_id++`, never 0). This is the device's **local-id** and becomes the host's **remote-id**.
- **arg1** = `INITIAL_DELAYED_ACK_BYTES` (33554432 = 32 MiB) **iff** the transport negotiated `delayed_ack`; otherwise `0`.
- **payload** = the destination string, which is the **host-connect target** `<local>` (e.g. `tcp:5201` or `tcp:<hostport>`), **NUL-terminated** (`payload.resize(size+1); payload[size]='\0'`). It is NOT the device port and it is NOT `reverse:...`-prefixed — it is the raw `connect_to` endpoint string.

Host receives it in `adb.cpp::handle_packet case A_OPEN:` (lines 500-552). It strips trailing NULs (`StripTrailingNulls`, adb.cpp:518) and validates via `IsReverseConfigured(address)` (see §7).

---

## 4. ACCEPTOR HANDSHAKE — how the host accepts (or rejects) the device A_OPEN

`adb.cpp::handle_packet case A_OPEN:` (lines 500-551):

```cpp
if (!t->online || p->msg.arg0 == 0) break;                 // ignore garbage / id 0
uint32_t send_bytes = (uint32_t)p->msg.arg1;
if (t->SupportsDelayedAck() != (bool)send_bytes) {          // arg1 must be 0 iff no delayed_ack
    send_close(0, p->msg.arg0, t);                          // mismatch -> reject
    break;
}
address = StripTrailingNulls(payload);
#if ADB_HOST
if (!t->IsReverseConfigured(address.data()))                // not an allow-listed reverse target
    LOG(FATAL) ...                                          // (host aborts; see §7 note)
#endif
asocket* s = create_local_service_socket(address, t);       // connect to host target (e.g. tcp:5201)
if (s == nullptr) { send_close(0, p->msg.arg0, t); break; } // can't connect -> reject

s->peer = create_remote_socket(p->msg.arg0, t);             // remote-id = device's arg0
s->peer->peer = s;

if (t->SupportsDelayedAck()) {
    s->available_send_bytes = send_bytes;                   // adopt device's grant as OUR send window
    send_ready(s->id, s->peer->id, t, INITIAL_DELAYED_ACK_BYTES);  // grant device 32 MiB
} else {
    send_ready(s->id, s->peer->id, t, 0);
}
s->ready(s);
```

**Accept reply** (`send_ready`, adb.cpp:269-282):
```
A_OKAY(arg0 = host_local_id, arg1 = device_local_id, payload = window_grant_or_empty)
```
- **arg0** = `s->id` = the host's freshly allocated local-id for the new local socket (allocator `install_local_socket`, monotonically increasing from 1, never 0). This is the host's **local-id**.
- **arg1** = `s->peer->id` = the device's `p->msg.arg0` echoed back (the **remote-id**). `create_remote_socket` (sockets.cpp:544-557) FATALs if id == 0.
- **payload**: present only under delayed_ack — a 4-byte LE `ack_bytes` (`INITIAL_DELAYED_ACK_BYTES` = 32 MiB) carried as the initial receive-window grant. Without delayed_ack the OKAY carries an empty payload (`send_ready` only resizes the payload when `t->SupportsDelayedAck()`).

So id assignment: device picks its local-id (arg0 of A_OPEN); host picks its own local-id and echoes the device's id as arg1; thereafter every WRTE/OKAY uses `(arg0=sender_local_id, arg1=receiver_local_id)`.

**Reject reply** — `send_close(local, remote, t)` (adb.cpp:284-291) sends:
```
A_CLSE(arg0 = local, arg1 = remote, payload = empty)
```
On A_OPEN rejection the host uses `send_close(0, p->msg.arg0, t)` — i.e. `A_CLSE(arg0=0, arg1=device_local_id)`. arg0==0 is the protocol's "failed OPEN" signal (adb.cpp:594-616, `case A_CLSE`). Two reject cases: (a) arg1/delayed_ack mismatch, (b) `create_local_service_socket` returned null (couldn't reach the host target).

> For OUR server frontend (acceptor): allocate a fresh nonzero `host_local_id`, remember the device's arg0 as the peer/remote-id, dial the host target, then reply `A_OKAY(host_local_id, device_local_id, [4-byte LE 32 MiB if delayed_ack])`. To reject, reply `A_CLSE(0, device_local_id)`.

---

## 5. DELAYED_ACK / windowed flow control — OPENER vs ACCEPTOR

Yes — under delayed_ack (transport version `>= A_VERSION_SKIP_CHECKSUM`, 0x01000001, with both banners advertising `delayed_ack`), the acceptor's accepting OKAY **must carry an initial receive-window grant** in its payload, exactly like the opener's first OKAY. Both roles call the same `send_ready(..., INITIAL_DELAYED_ACK_BYTES)` and emit a 4-byte LE payload.

The symmetry / asymmetry:

**OPENER role** (`sockets.cpp::connect_to_remote`, lines 571-577): the A_OPEN itself carries `arg1 = INITIAL_DELAYED_ACK_BYTES` as the opener's *advertised receive window to the peer*, and the opener sets its OWN `available_send_bytes = 0` (it has not yet been granted a send window — it must wait for the peer's OKAY payload before it may send).

**ACCEPTOR role** (`adb.cpp::handle_packet` A_OPEN, lines 506-547): the acceptor reads the device's `arg1` (`send_bytes`) and adopts it as its OWN `available_send_bytes = send_bytes` (so the acceptor may immediately send up to 32 MiB toward the device). It then sends back `send_ready(s->id, s->peer->id, t, INITIAL_DELAYED_ACK_BYTES)` to grant the device a 32 MiB receive window. So: the acceptor's initial send-window is set from the device's A_OPEN.arg1; the opener's initial send-window starts at 0 and is set from the peer's first OKAY payload.

**Windowed byte accounting** (same machinery both directions, `sockets.cpp`):
- On send: `local_socket_flush_outgoing` (lines 174-247) decrements `*s->available_send_bytes -= data.size()` per WRTE. When it drops `<= 0`, `fdevent_del(FDE_READ)` stops reading the local fd (backpressure).
- On receiving an A_OKAY with a 4-byte payload: `local_socket_ack` (lines 417-441) does `*s->available_send_bytes += acked_bytes` (acked_bytes is a **signed i32-LE delta, may be negative**) and re-arms reading if `> 0`.
- On the receive side, every time the local socket flushes bytes out to its fd, `local_socket_flush_incoming` (lines 122-172) emits `send_ready(s->id, s->peer->id, t, bytes_flushed)` — i.e. an A_OKAY whose payload is the number of bytes just drained, re-granting that much window to the peer. (Non-delayed_ack mode instead sends a payload-less OKAY only when the queue is below `MAX_PAYLOAD`.)
- `handle_packet` A_OKAY (adb.cpp:554-592) parses `payload.size()==sizeof(int32_t)` -> `acked_bytes`; `payload.size()==0` -> no delta; any other size is an error.

So the only opener/acceptor difference is the *source* of the initial send-window (opener: peer's first OKAY payload; acceptor: device A_OPEN.arg1); everything afterward is the same signed-delta windowing. Both must echo `delayed_ack` consistently or the peer's `SupportsDelayedAck()` mismatch triggers rejection (adb.cpp:507-511; `local_socket_ack` also guards `available_send_bytes.has_value() != acked_bytes.has_value()`, sockets.cpp:423-427).

---

## 6. `killforward`, `killforward-all`, `list-forward` — request strings + reply framing

All sent as **device-bound** local services under the `reverse:` prefix (commandline.cpp:1924-1934):

| CLI | Smartsocket service string |
|---|---|
| `reverse --remove <remote>` | `reverse:killforward:<remote>` |
| `reverse --remove-all` | `reverse:killforward-all` |
| `reverse --list` | `reverse:list-forward` |

These reach adbd's `reverse_service` → shared `handle_forward_request` (adb.cpp). On the **device** (`#if ADB_HOST` false):

- `list-forward` (adb.cpp:1136-1144): `SendProtocolString(reply_fd, format_listeners())` — i.e. `%04x`+body. Body lines are `<serial> <local_name> <connect_to>\n` per `format_listeners` (adb_listeners.cpp:129-145). For reverse entries the serial column is the literal **`(reverse)`** because reverse listeners on the device have an empty `transport->serial`:
  ```cpp
  "%s %s %s\n",
  !serial.empty() ? serial : "(reverse)", local_name, connect_to
  ```
  So a reverse rule lists as e.g. `(reverse) tcp:5201 tcp:5201`. (`local_name` = device port, `connect_to` = host target.)
- `killforward-all` (adb.cpp:1146-1153): `remove_all_listeners()` then a single `SendOkay(reply_fd)` (the host-only extra OKAY at line 1150 is compiled out on adbd).
- `killforward:<remote>` (adb.cpp:1181-1187, 1196-1219): validates one piece, `remove_listener(pieces[0], transport)`, then single `SendOkay` on success or `SendFail` on `INSTALL_STATUS_LISTENER_NOT_FOUND`.

Because these are tunneled, the client first reads the server's smart-socket connect `OKAY`, then the device's reply. `reverse --list` uses `adb_query_command` (commandline.cpp:1921), which reads `OKAY` then a length-prefixed string. `--remove`/`--remove-all` go through the same `adb_connect`+`adb_status`+optional `ReadProtocolString` path as `forward` (commandline.cpp:1956-1964).

> For OUR server frontend: relay these to the device verbatim and pass the device's framed reply back, plus our own leading connect-OKAY. The `(reverse)` marker is produced BY the device's `format_listeners`, not synthesized by the host.

---

## 7. OVERALL lifecycle — listener lives on the DEVICE; host binds NOTHING for reverse

Confirmed. The reverse listening socket is bound and held by **adbd on the device** (`install_listener` → `socket_spec_listen`, adb_listeners.cpp:218-242, executed inside `reverse_service` on the device). The host/server:

1. Tunnels `reverse:forward:...` to the device (it is a device local service, never a host service) and records the mapping via `UpdateReverseConfig` (transport.cpp:1640-1677) into `reverse_forwards_[remote] = local`.
2. Does **NOT** bind any TCP listener for reverse (unlike `host:forward:` which binds a host-side listener via the host's own `install_listener`). The host's only job is to **service inbound device-initiated A_OPENs**.
3. For each device A_OPEN it validates the destination against the allow-list with `IsReverseConfigured(address)` (transport.cpp:1680-1690) — which matches the A_OPEN payload (the host target `<local>`) against the *values* of `reverse_forwards_`. If it does not match, the upstream host LOG(FATAL)s (adb.cpp:524-527) — a hardening measure against a compromised adbd opening arbitrary host connections. (A from-scratch server frontend should treat an unconfigured target as a reject → `A_CLSE(0, device_local_id)` rather than aborting the process.)
4. On a valid A_OPEN it connects to the host target (`create_local_service_socket`, e.g. opens `tcp:<hostport>`), sends the accept `A_OKAY` (§4), and bridges bytes with windowed flow control (§5). Teardown is via `A_CLSE` from either side.

This is the mirror image of forward: in forward the HOST binds the listener and issues A_OPEN to the device; in reverse the DEVICE binds the listener and issues A_OPEN to the host. Our frontend therefore needs an **inbound-A_OPEN acceptor** on the device transport, not a TcpListener.

---

## Local repo cross-references (for the implementor)

- Forward (host-binds) registry/parse already done: `adb_client/src/server/forward.rs` (note its `local;remote` order — reverse is the opposite `remote;local`).
- Reverse proxy client commands (string building today): `adb_client/src/proxy/device_commands/reverse.rs` uses `ADBLocalCommand::Reverse(remote, local)` / `ReverseRemove` / `ReverseRemoveAll`.
- Host-protocol reply framing helpers: `adb_client/src/server/protocol.rs` (`okay`, `okay_twice`, `okay_twice_with_port`, `okay_data`, `fail`). Note `okay_twice*` is the HOST forward semantics; for reverse the second OKAY/port comes from the device, so the frontend should send ONE connect-OKAY and relay the device's bytes.
- Opener-side OPEN today: `adb_client/src/message_devices/adb_message_device.rs::open_session` sends `A_OPEN(local_id, arg1=0, dest)` and expects `OKAY` with `arg1==local_id`. It currently sets `arg1=0` (no delayed_ack window on OPEN) even though the banner advertises `delayed_ack` — relevant when reconciling §5 for the acceptor path.
- Session ids: `adb_client/src/message_devices/adb_session.rs` (`local_id`/`remote_id`), `recv_and_reply_okay` shows the existing OKAY-reply pattern (payload-less); the acceptor path needs the windowed 4-byte-LE payload variant under delayed_ack.

## Caveats / Not Found

- All citations are from branch `main` (fetched 2026-06-13); line numbers may drift across AOSP releases. Behavior (string formats, arg semantics, double-OKAY being host-only) has been stable across Android 12–15.
- The device-side `service_to_fd` dispatch (`reverse:` → `reverse_service`) is at `daemon/services.cpp:361-362`; the host-side `services.cpp` is a separate (smaller) file and does not contain the reverse listener.
- `MAX_PAYLOAD` for delayed_ack-capable transports is 1 MiB; the per-stream window is 32 MiB. The exact runtime `get_max_payload()` negotiation is in `transport.cpp` (`max_payload = min(payload, MAX_PAYLOAD)`, ~line 1174) and not fully traced here — only the window/ack-delta accounting is.
