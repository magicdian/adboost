# Server Host Protocol — Transport Selection Contract

> Executable contract for the **server frontend** (`adboost/src/server/`,
> feature `server`) smartsocket *host protocol* — adboost acting *as* an adb
> server for native `adb` / `scrcpy` clients. This is the mirror of the USB
> wire-protocol contract: that doc covers the device transport; this one covers
> the host-side request routing in `frontend.rs`.

---

## Transport selection: every path must agree on AOSP error semantics

There are **multiple parallel transport-selection entry points** in
`frontend.rs`, and they must all report the *same* AOSP error wording for the
*same* condition. They are easy to drift apart because each is a separate
function with its own match:

| Service | Function | Reply on success |
|---|---|---|
| `host:transport-any` | `select_transport_any` | bare `OKAY` |
| `host:transport:<serial>` | `select_transport_by_serial` | bare `OKAY` |
| `host:transport-id:<N>` | `select_transport_by_id` | bare `OKAY` |
| `host:tport:<sel>` (`any`/`-any`/empty, `serial:<s>`/`<s>`, `id:<N>`/`-id:<N>`) | `select_tport` | `OKAY` + 8-byte LE id |
| `host:*forward*` (no serial) | `serve_host_forward` → `resolve_single_serial` | forward-family framing |

### Validation & Error Matrix (the canonical wording)

| Selector | 0 devices | >1 devices | serial/id not found | bad id |
|---|---|---|---|---|
| `*-any` / empty / `forward` (implicit) | `FAIL("no devices")` | `FAIL("more than one device")` | — | — |
| `:<serial>` | `FAIL("device not found")` | (n/a — serial is explicit) | `FAIL("device not found")` | — |
| `-id:<N>` | — | — | `FAIL("no device for transport id")` | `FAIL("invalid transport id")` |

`select_transport_any` and `resolve_single_serial` are the reference for the
`any` column; `select_transport_by_id` is the reference for the id column.

---

## `host-serial:<serial>:<sub>` parsing — anchor on the sub-service, NOT a colon position

`host-serial:` carries the serial *inside* the service string, then a
sub-service. The split point between serial and sub is **not** a fixed colon
position, because **both sides can contain colons**:

- The serial of a TCP/IP device is `ip:port` (e.g. `172.20.1.45:5555`).
- Some sub-services carry colons themselves: `forward:tcp:0;tcp:7777`,
  `killforward:tcp:7777`.

So neither `split_once(':')` (first colon) nor `rsplit_once(':')` (last colon)
is correct. `split_host_serial` (`frontend.rs`) instead **anchors on a known
sub-service**: it scans colon split points left-to-right and takes the first one
whose right-hand side satisfies `is_host_serial_sub` (the exact member set of
`dispatch_host_serial`'s `match sub`: `get-state` / `get-serialno` / `features`
/ `transport*` / `tport` / forward-family). Left-to-right scan yields the
longest serial that still leaves a valid sub-service — matching AOSP's
serial-prefix matching for `ip:port` serials. When nothing anchors, it falls
back to the first colon so a genuinely-unknown sub still reaches the precise
`unknown host-serial sub-service: <sub>` error rather than collapsing to
`malformed`.

> **Gotcha — first-colon split breaks TCP/IP serials.** The original
> `rest.split_once(':')` parsed `172.20.1.45:5555:features` as serial
> `172.20.1.45`, sub `5555:features` → `FAIL("unknown host-serial sub-service:
> 5555:features")`. USB serials (no colon) hid the bug; it surfaced once TCP
> devices were transport-selectable. `is_host_serial_sub` MUST stay in lockstep
> with `dispatch_host_serial`'s `match sub` — add a member to one, add it to the
> other. Covered by `split_host_serial_*` unit tests plus
> `host_serial_{features,get_state,forward}_with_tcp_ip_serial_routes_correctly`,
> and end-to-end by the `tcpip.shell_through_tcp_device` selftest parity case
> (modern `adb -s <ip:port>` sends `host-serial:<ip:port>:features` first).

## Gotcha: modern `adb` selects a transport via `host:tport:any` BEFORE the local service

> **Warning**: `adb shell` / `adb forward --list` / `adb reverse --list` **with
> no `-s`** do NOT send `shell:` (or the forward/reverse command) directly. The
> client first sends `host:tport:any` to pick a transport, *then* sends the local
> service on the same socket. So the error a multi-device user sees comes from
> **`select_tport`**, not from the local-service dispatch and not from
> `resolve_single_serial`.

### Common Mistake: collapsing all `tport` failures into one `Option`

**Symptom**: With multiple devices, `adb shell` (no `-s`) reports
`error: device not found` instead of the AOSP-correct `more than one device`.
`forward --remove/--list` and `reverse --list` show the same wrong wording —
same root cause, because they all go through `tport:any` first.

**Cause**: `select_tport` resolved every selector to a single
`Option<String>`, so *no devices*, *multiple devices*, and *serial/id not found*
all became `None` → one shared `FAIL("device not found")`. Only
`[one] => Some` was distinguished.

**Fix / Prevention**: each selector branch carries its own reason. Use a
`Result<String, &str>` per branch so the `any` branch can distinguish empty vs
multiple, matching `select_transport_any`:

### Wrong

```rust
let chosen = if rest.is_empty() || rest == "any" || rest == "-any" {
    match devices.as_slice() {
        [one] => Some(one.serial.clone()),
        _ => None,                       // [] AND multi-device collapse here
    }
} else { /* id / serial also -> Option */ };
// ...
} else {
    stream.write_all(&protocol::fail("device not found")).await?;  // wrong for 0 / >1
}
```

### Correct

```rust
let chosen: std::result::Result<String, &str> =
    if rest.is_empty() || rest == "any" || rest == "-any" {
        match devices.as_slice() {
            [] => Err("no devices"),
            [one] => Ok(one.serial.clone()),
            _ => Err("more than one device"),
        }
    } else if let Some(id_str) = rest.strip_prefix("id:").or_else(|| rest.strip_prefix("-id:")) {
        match id_str.parse::<u64>() {
            Ok(id) => protocol::transport_id_for_index(id, &serials).ok_or("no device for transport id"),
            Err(_) => Err("invalid transport id"),
        }
    } else {
        let serial = rest.strip_prefix("serial:").unwrap_or(rest);
        devices.iter().find(|d| d.serial == serial).map(|d| d.serial.clone()).ok_or("device not found")
    };
```

---

## Tests Required (assertion points)

Inline `#[cfg(test)] mod tests` in `frontend.rs`, driven by `round_trip` against
a `MockBackend`. Run with `--features "server,usb"` (the test module references
`crate::usb::MultiplexedSession`, so `server` alone will not compile the tests).

- `tport_any_with_multiple_devices_fails_more_than_one` → body contains `more than one device`
- `tport_any_with_no_devices_fails_no_devices` → `no devices`
- `tport_by_unknown_serial_fails_device_not_found` → `device not found`
- `tport_by_unknown_id_fails_no_device_for_transport_id` → `no device for transport id`
- `tport_by_invalid_id_fails_invalid_transport_id` → `invalid transport id`
- single-device happy path (`tport_any_with_single_device_replies_okay_plus_8byte_id`) unchanged

### Runtime selftest (device-backed, `adboost_cli selftest`)

Unit tests use a `MockBackend`; the *reported* regression only manifests with a
**real** multi-device setup driven by the **official `adb` client** (which is
what issues `host:tport:any` before `shell:`). Covered by a parity case:

- `adboost_cli/src/selftest/parity.rs::case_official_adb_ambiguous_shell` — runs
  `adb -P <port> shell echo …` with **no `-s`** against adboost's in-process
  server; `Passed` iff stderr contains `more than one device`, and explicitly
  flags `device not found` as a REGRESSION.
- Wired in `selftest/mod.rs::run_through_server_phase` under `if multi {}` —
  runs **once per run** (ambiguity is a whole-device-set property, not per
  serial), never emitted in single-device mode, `Skipped` when no `adb` on PATH.
  Note the rest of the multi-device suite selects by serial (the `-s`
  equivalent), so this is the one case that exercises the ambiguous path.

---

## Device control services are bridged verbatim like `shell:` v1

`map_local_service` (`frontend.rs`) recognizes a **control-service** family —
`tcpip:<port>`, `usb:`, `root:`, `reboot:[mode]`, `remount:`, `enable-verity:`,
`disable-verity:` — and forwards each as `ADBLocalCommand::Raw(service)` with
**no capability gating**. The justification (and the reason this is safe) is that
every one is structurally identical to bare `shell:` v1: a single OPEN, a short
textual reply, then CLSE. `bridge_tcp_session`'s half-close copy already handles
that request/response-then-close shape, so no separate "one-shot" path is
needed. `is_control_service()` is the pure predicate; unit-tested for the exact
member set (and against look-alikes like `usbfoo:` / `rebooting:` / `tcp:`).

> **Gotcha**: `tcpip:`/`usb:`/`reboot:` restart adbd, which drops the USB
> connection. The bridge observes EOF normally; the backend's `get_or_open`
> replaces the now-stale cached connection on the next open. Do **not** treat the
> post-`tcpip` connection drop as an error.

## `host:features` is **per-device** (capability negotiation is two-axis)

Capability advertising/gating has **two axes**, and a wire-framing-changing
feature (`shell_v2`, `sync_v2`) needs BOTH to be true before it is offered or
opened:

1. **Backend-can-bridge** (server-global): `DeviceBackend::capabilities()` →
   `ServerCapabilities::negotiated_with` at `serve()` time. This is what
   `adboost` *implements*.
2. **Device-supports** (per-device): the target device's own CNXN banner,
   parsed by `DeviceFeatureSet::from_banner` and exposed as
   `PersistentConnection::peer_features()`. Looked up on demand via
   `DeviceBackend::device_capabilities(serial, timeout)` (default impl `None`;
   `DefaultDeviceBackend` returns the connection's cached banner set, handshaking
   within the timeout if needed). `DeviceEntry.capabilities: Option<DeviceFeatureSet>`
   carries it through `list_devices`/`track-devices` (`None` = not yet known →
   conservative).

**Why this exists** (the bug it fixes): one backend can front devices of
differing capability — e.g. a full Android adbd (banner has `shell_v2`) and a
stripped adbd reached via `adb forward tcp:N tcp:M` + `adb connect`
(empty `features=` banner). A global-only `host:features` advertises `shell_v2`
to **all** of them; the client then opens `shell,v2,...,pty:` against the
stripped device, which `CLSE`s the OPEN (`open session failed`). Per-device
`host:features` makes the client pick v1 itself for the feature-less device.

**Where the two axes are consumed:**

| Site | `frontend.rs` | Rule |
|---|---|---|
| pre-transport `host:features` (no serial) | `host_data_query_payload` | global caps only (no device chosen yet — unavoidable) |
| post-transport `host:features` | after `TransportSelected` | `intersected_with_device(serial)` — **per-device** (native `adb -s … shell` gates v1/v2 on this) |
| `host-serial:<serial>:features` | `dispatch_host_serial` | `intersected_with_device(serial)` — **per-device** |
| `shell,v2` / `sync:` open gate | `map_local_service(svc, device_caps)` | `device_has_feature(feat, device_caps)` — **defense-in-depth fallback**: FAIL cleanly instead of passing an OPEN the device will `CLSE` |

**Banner → server-feature mapping**: `shell_v2`(server) ⟸ `shell_v2`(banner);
`sync_v2`(server) ⟸ `stat_v2`(banner) (the `STA2` opcode AOSP gates v2 sync on).
The always-safe defaults (`cmd,stat_v2,fixed_push_mkdir,apex`) never change the
client's wire framing, so they pass through regardless of device. `None` device
caps (unknown) drops both framing features — conservative.

> **Gotcha**: keep `PersistentConnection::device_features()` ("what *we*
> advertise to the device") distinct from `peer_features()` ("what the *device*
> advertised to us"). Per-device negotiation reads `peer_features()`. Conflating
> them is exactly the misread the original bug report made.

## `host:connect` / `host:disconnect` and the unified device table

`host:connect:<addr>` / `host:disconnect:<addr>` route to
`DeviceBackend::connect` / `disconnect`. The default backend
(`DefaultDeviceBackend`, formerly `UsbDeviceBackend` — kept as a `#[deprecated]`
alias) holds a TCP-device registry alongside the USB connection cache;
`list_devices` returns the **merged** set via the pure `merge_device_sets`
helper, so `host:devices`/`devices-l`/`track-devices`/transport-id all see USB +
TCP as one list (mirroring AOSP's single `transport_list`).

Reply framing: both are host **data queries** — `OKAY` + `%04x`+status, where the
status is the AOSP-style line the client prints (`connected to <addr>` /
`already connected to <addr>` / `disconnected <addr>` / `disconnected everything
(<n> device(s))`); a backend error is a single `FAIL`. `connect` is idempotent
(re-connect to a tracked serial returns `already connected to`), defaults a
missing port to 5555, and performs the full CNXN(+STLS) handshake synchronously
so an unreachable device fails the client's `connect` rather than appearing in
the list. Runtime-guarded by the `official_adb_connect_routing` parity case,
which locks against the original `unknown host service: connect:` regression.

> **Constraint for the TCP shell bridge (deferred follow-up)**: a `host:connect`d
> device is currently **listed and transport-selectable but its local services
> are not bridged** — `open_local_service` against a TCP serial returns a stable
> "not yet supported" error. The blocker is that `MultiplexedSession` and the
> whole `PersistentUsbConnection` multiplexer (`usb/persistent.rs`) are
> hard-typed to `USBTransport`; bridging `shell:`/`sync:` *through* the server to
> a TCP device needs that multiplexer generalized over `ADBMessageTransport`
> (which `TcpTransport` already implements). That refactor must preserve the
> three device-verified wire regressions documented in
> `adb-wire-protocol-contract.md` (delayed_ack/data_check coupling, CNXN no-NUL
> banner, CLSE routing).
