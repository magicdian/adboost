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
function with its own match — so they all funnel through one core,
`resolve_single_by_kind(want: Option<TransportKind>)` (the analogue of AOSP's
single `acquire_one_transport`), which filters the device list by the requested
kind and applies the zero/one/many uniqueness with kind-correct wording.

| Service | Function | Reply on success |
|---|---|---|
| `host:transport-any` | `select_transport_any` → `select_transport_kind(None)` | bare `OKAY` |
| `host:transport-usb` | `select_transport_kind(Some(Usb))` | bare `OKAY` |
| `host:transport-local` | `select_transport_kind(Some(Local))` | bare `OKAY` |
| `host:transport:<serial>` | `select_transport_by_serial` | bare `OKAY` |
| `host:transport-id:<N>` | `select_transport_by_id` | bare `OKAY` |
| `host:tport:<sel>` (`any`/`-any`/empty, `usb`/`local`, `serial:<s>`/`<s>`, `id:<N>`/`-id:<N>`) | `select_tport` | `OKAY` + 8-byte LE id |
| `host:*forward*` (no serial) | `serve_host_forward` → `resolve_single_serial` → `resolve_single_by_kind(None)` | forward-family framing |
| `host-usb:<sub>` / `host-local:<sub>` | `dispatch_host_kind` → `resolve_single_by_kind(Some(kind))` then `dispatch_host_serial` | per-sub-service (mirrors `host-serial:`) |
| `host-transport-id:<N>:<sub>` | `dispatch_host_transport_id` → `serial_for_transport_id(N)` then `dispatch_host_serial` | per-sub-service (mirrors `host-serial:`) |

### Transport KIND filter — `adb -d` / `adb -e` (`TransportKind`)

Native `adb` encodes a transport-type filter into the request: `-d` (USB) →
`host-usb:` prefix then a transport switch; `-e` (emulator/TCP-local) →
`host-local:` then a transport switch. This is the wire-side mirror of AOSP's
`TransportType` (`kTransportUsb`/`kTransportLocal`/`kTransportAny`).

> **Modern `adb` phase 2 uses `tport:`, not `transport-usb`.** Confirmed via
> `ADB_TRACE=all adb -d shell true` against adb 35.0.2: phase 1 is
> `host-usb:features`, then phase 2 is **`host:tport:usb`** (it wants the 8-byte
> transport-id back), NOT the legacy `host:transport-usb`. `-e` mirrors with
> `host:tport:local`. So `select_tport` recognizes the bare `usb`/`local` kind
> tokens and routes them through the **same** shared kind resolver
> (`pick_single_by_kind`) as `transport-usb`/`transport-local` — same single-device
> resolution, same kind-specific wording. The `transport-usb`/`transport-local`
> `match svc` arms are kept for older / direct callers. Only the *bare* tokens are
> kinds; the explicit `serial:<s>` form still resolves a device literally named
> `usb`/`local`.

- `DeviceEntry.kind: Option<TransportKind>` carries each device's kind.
  **`None` = "this backend does not tag kind" → matches ANY requested kind** (the
  same conservative-default shape as `capabilities: Option<_>`). An untagged
  backend therefore never regresses: `-d`/`-e` degrade to transport-any over the
  full set (single device selected, multiple → ambiguous). `DefaultDeviceBackend`
  tags every device (`Usb` for `enumerate_usb`, `Local` for `tcp_device_entries`).
- `kind_matches(want, entry_kind)`: `None` on *either* side matches; two concrete
  kinds must be equal. This is the single predicate behind `resolve_single_by_kind`
  and `serve_wait_for` (`wait-for-usb-device` / `wait-for-local-device`).
- `pick_single_by_kind(devices, want)` is the **pure** filter + zero/one/many core
  over an already-fetched device slice (uses `kind_matches` /
  `no_devices_msg` / `ambiguous_msg`). `resolve_single_by_kind` is a thin async
  wrapper (fetch → delegate → clone serial); `select_tport`'s `usb`/`local` branch
  calls it directly on its own already-fetched slice (no second `list_devices()`).
  This keeps the "every kind-aware selection funnels through one core" invariant
  literally true with no duplicated wording.
- `host-usb:`/`host-local:` are structurally `host-serial:` with the device pinned
  by kind instead of serial: resolve the one matching device, then run the same
  sub-service set via `dispatch_host_serial`.
- **`DeviceEntry` is `#[non_exhaustive]`.** External backends construct it via
  `DeviceEntry::new(serial).with_kind(..).with_capabilities(..)`, never a struct
  literal — so future fields stay non-breaking downstream. (First
  `#[non_exhaustive]` in `server/`.)

### Validation & Error Matrix (the canonical wording — AOSP-exact)

Wording is byte-verified against the `adb` 35.0.2 client binary. The `0 devices`
and `>1 devices` columns are **kind-specific** (the column header is the requested
kind):

| Selector / kind | 0 devices | >1 devices | serial/id not found | bad id |
|---|---|---|---|---|
| any (`*-any`/empty/`forward`/`transport-any`, or untagged) | `FAIL("no devices/emulators found")` | `FAIL("more than one device/emulator")` | — | — |
| usb (`-d`: `host-usb:`/`transport-usb`) | `FAIL("no devices found")` | `FAIL("more than one USB device")` | — | — |
| local (`-e`: `host-local:`/`transport-local`) | `FAIL("no emulators found")` | `FAIL("more than one emulator")` | — | — |
| `:<serial>` | `FAIL("device not found")` | (n/a — serial is explicit) | `FAIL("device not found")` | — |
| `-id:<N>` | — | — | `FAIL("no device for transport id")` | `FAIL("invalid transport id")` |

`resolve_single_by_kind` + the pure helpers `no_devices_msg(want)` /
`ambiguous_msg(want)` are the single reference for the kind columns;
`select_transport_by_id` is the reference for the id column.

> **Gotcha — the kind columns are NOT "device" everywhere.** AOSP uses three
> different nouns: combined `device/emulator` for transport-any, bare `USB device`
> for `-d`, and `emulator` for `-e`. Do not "simplify" them to one string — the
> native `adb` client prints these verbatim and the parity tests assert the exact
> bytes. `host:devices`/`devices-l` are **never** kind-filtered (AOSP lists all
> transports regardless of `-d`/`-e`); only *selection* is filtered.

## Bare `host:get-state` / `host:get-serialno` — transport-any single-device *data queries*

Two second-class "bare" *host data queries* (`host:get-state` /
`host:get-serialno`) carry **no serial prefix** and so must resolve the single
connected device by *transport-any* semantics, mirroring the pinned
`host-serial:<serial>:get-state` / `:get-serialno` forms exactly:

| Service | Resolution | Reply on success | 0 devices | >1 devices |
|---|---|---|---|---|
| `host:get-state` | `host_data_query_payload` → `resolve_single_serial()` (= `resolve_single_by_kind(None)`) → `dispatch_host_serial` get-state | `OKAY` + `%04x`+`device` (or the device state) | `FAIL("no devices/emulators found")` | `FAIL("more than one device/emulator")` |
| `host:get-serialno` | same funnel → get-serialno | `OKAY` + `%04x`+`<serial>` | same | same |

Both live in **`host_data_query_payload`** (`frontend.rs`) — the same single
dispatch point as `version`/`features`/`devices`/`devices-l` — and return
`Option<Result<String, String>>`: `None`=not a data-query service; `Ok(payload)`
= single-device reply (framed `okay_data`); `Err(reason)` = AOSP FAIL wording for
zero / multiple devices. This is deliberate: `host:get-state`/`:get-serialno`
are shape `OKAY`+framed-payload host queries with **no routing**, so they belong
to the data-query single entry point, NOT as extra `match svc` routing arms in
`dispatch_host_service`. That keeps `dispatch_host_service`'s `match` focused on
routing and future bare single-device data queries land on the same point.

> **Gotcha — the bare single-device reply MUST stay byte-identical to the pinned
> `host-serial:<serial>:<sub>` form.** `get-state`/`get-serialno` share the
> device-state/serial lookup logic via the same
> `entry.map_or("offline", |d| d.state.as_wire())` / raw-serial payload that
> `dispatch_host_serial` produces, so the two forms cannot drift. This is the
> contract AOSP `adb root`/`unroot` rely on: they call `adb_get_state()` (which
> sends `host:get-state`) before issuing `root:`/`unroot:`, and abort the whole
> flow on a `FAIL unknown host service: get-state` — the pre-fix regression this
> entry removes. Locked by the `bare_get_*_matches_pinned_*` unit tests.
> `host:get-state` is **not** kind-filtered (it resolves transport-any over the
> full set, like transport-any selection), and `host:get-devpath` is intentionally
> **not** implemented (`DeviceEntry` has no devpath field — honest-capability
> principle; a real field would need a separate feature).

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
/ `transport*` / `tport` / forward-family / `wait-for-*`). Left-to-right scan yields the
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

## The `adb root` / `adb tcpip` reconnect handshake (`wait-for-...-disconnect`)

After a control service that restarts adbd (`root:` / `unroot:` / `tcpip:` /
`usb:`), a modern `adb` client runs a **reconnect handshake** so it can wait out
the daemon bounce. When the client was pinned by transport-id (the `adb -s` /
multi-device case), it sends:

```
host-transport-id:<N>:wait-for-any-disconnect      # block until that transport is gone
```

and then — only if it was NOT originally pinned by id (`previous_id == 0`, i.e.
a single-device no-`-s` invocation) — a follow-up `host:wait-for-<transport>-device`
to wait for the daemon to come back. In the pinned case AOSP **skips** the
second wait entirely, so the frontend only has to answer the disconnect wait.

> **Gotcha — the wire prefix is `host-transport-id:<N>:`, NOT `host:transport-id:N:`.**
> `host:transport-id:<N>` (with the `host:` prefix) is the *transport-switch*
> service handled by `select_transport_by_id`. The reconnect handshake uses the
> `host-transport-id:` **family prefix** (a sibling of `host-serial:` /
> `host-usb:` / `host-local:`), emitted by AOSP `format_host_command`
> (`host-transport-id:%llu:%s`). These are different code paths; routing one does
> not route the other. `dispatch_host_transport_id` strips the family prefix,
> `split_once(':')` into `(N, sub)` (N is a bare `u64`, never colon-bearing — so
> unlike `host-serial:` it does NOT need `split_host_serial`), resolves N→serial
> via `serial_for_transport_id`, then funnels into `dispatch_host_serial` so the
> pinned-by-id and pinned-by-serial paths share identical sub-service semantics.

### `disconnect` state in `serve_wait_for`

`serve_wait_for(arg, pinned_serial)` parses `<transport>-<state>` and supports
two states: `device` (wait until a matching device is **present**) and
`disconnect` (wait until the target's transport is **torn down**). The target is:

- `pinned_serial = Some(s)` (from `host-serial:`/`host-transport-id:` routing):
  the specific serial `s`'s transport is dead. This mirrors AOSP, where
  `wait-for-disconnect` unblocks when the *exact* pinned transport
  (`t->id == transport_id`) is torn down, not when "any device" disconnects.
- `pinned_serial = None` (top-level `host:wait-for-*`): no device of the
  requested kind (`kind_matches(want, kind)`) is present.

Both states reply **two bare `OKAY`s** on satisfaction (`protocol::okay_twice()`),
because AOSP's client reads two OKAYs for `wait-for-*` (accept + satisfied) — the
SAME contract the `forward` family follows. adboost does **not** emit a blanket
accept OKAY at the smartsocket layer (`handle_client` dispatches straight to the
service), so each service needing two emits them itself. Sending only one desyncs
modern clients (`error: protocol fault (couldn't read status)`).

> **`disconnect` is EVENT-DRIVEN, not presence-polling (mirrors AOSP timing).**
> Native adb detects a disconnect at the connection I/O layer (the read pump
> errors out the instant adbd closes the socket → transport torn down), so `adb
> root`/`unroot` returns sub-second and never hangs. adboost mirrors this:
> `serve_wait_for`'s `disconnect` branch does NOT poll `list_devices()`. Instead
> it (1) subscribes to `subscribe_lifecycle()`, (2) does an entry
> `DeviceBackend::transport_alive(serial)` check — the **primary** path, because
> the cached connection's reader/writer routinely die *before* the
> `wait-for-disconnect` request even arrives (PR0 real-hardware data) — and
> (3) `select!`s a matching `LifecycleEvent::TransportReset`/`Disconnected` event
> against a **bounded 10 s fallback**. `transport_alive` reports a cached
> connection's `is_alive()`, NOT mere enumeration: a dead-but-still-enumerated
> device (an adbd restart that does not re-enumerate USB — the MTK case) reads as
> NOT alive, which the old presence poll could never see (the serial never left
> `list_devices()` → hung the full 60 s).
>
> **Why a separate `TransportReset` event, not `Disconnected`.** An adbd restart
> is not a permanent unplug, so `handle_disconnects` must NOT release the device's
> `forward`/`reverse` rules on it (native keeps the host-side listeners across a
> restart). `TransportReset` is therefore distinct from `Disconnected`;
> `handle_disconnects` ignores it (and KEEPS looping — it uses a `match`+`continue`,
> never `while let Some(Disconnected(..))`, which would terminate on the first
> reset and silently disable all later cleanup).
>
> **Bounded fallback (10 s) — deliberate divergence from AOSP.** Native waits
> *forever* (it watches the client fd). adboost's `serve_wait_for` does not watch
> the client socket, so the `disconnect` branch caps the wait at 10 s (PR0: the
> connection died within 250 ms max on every real restart, so 10 s is generous
> and far shorter than the old 60 s). The fallback fires only when adbd did NOT
> actually restart (a no-op `root`); on expiry it still sends **two OKAYs** (assume
> disconnected, clean return — matching native), with a WARN log so a
> never-restarting adbd stays diagnosable. The `device`-present branch keeps its
> 200 ms poll / 60 s `MAX_WAIT` and a single `FAIL("wait-for timed out")` on
> expiry — a device that never appears is a genuine failure (unlike a disconnect
> we can safely assume).

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
            // transport-any wording via the shared helpers (kind == None).
            [] => Err(no_devices_msg(None)),       // "no devices/emulators found"
            [one] => Ok(one.serial.clone()),
            _ => Err(ambiguous_msg(None)),          // "more than one device/emulator"
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

- `tport_any_with_multiple_devices_fails_more_than_one` → `more than one device/emulator`
- `tport_any_with_no_devices_fails_no_devices` → `no devices/emulators found`
- `tport_by_unknown_serial_fails_device_not_found` → `device not found`
- `tport_by_unknown_id_fails_no_device_for_transport_id` → `no device for transport id`
- `tport_by_invalid_id_fails_invalid_transport_id` → `invalid transport id`
- `transport_any_with_no_devices_fails` → `no devices/emulators found`
- `forward_no_device_fails` → `no devices/emulators found`
- single-device happy path (`tport_any_with_single_device_replies_okay_plus_8byte_id`) unchanged

Transport-KIND (`adb -d`/`-e`) assertion points (all in `frontend.rs` tests):

- `transport_usb_selects_the_single_usb_device` / `transport_local_selects_the_single_local_device` → bare `OKAY`
- `transport_usb_in_mixed_topology_picks_usb_not_tcp` → `-d` picks USB, `-e` picks TCP in a mixed set
- `transport_usb_with_two_usb_devices_fails_more_than_one_usb_device` → `more than one USB device`
- `transport_local_with_two_local_devices_fails_more_than_one_emulator` → `more than one emulator`
- `transport_usb_with_only_a_tcp_device_fails_no_devices_found` → `no devices found`
- `transport_local_with_only_a_usb_device_fails_no_emulators_found` → `no emulators found`
- `host_usb_features_resolves_and_answers_features` → phase-1 `host-usb:features` is answered (the reported bug)
- `host_local_get_state_resolves_local_device`, `host_usb_features_with_no_usb_device_fails_no_devices_found`
- `transport_usb_untagged_backend_degrades_to_transport_any` +
  `transport_usb_untagged_multi_device_is_ambiguous_with_usb_wording` → `kind: None` backward-compat
- Pure helpers: `parse_transport_kind_maps_tokens`, `kind_matches_treats_none_as_wildcard_on_both_sides`,
  `error_wording_matches_aosp_per_kind` (locks the exact AOSP strings)

Modern phase-2 `host:tport:usb`/`host:tport:local` assertion points (the path the
real client actually uses; same shared resolver / wording as `transport-usb`):

- `tport_usb_selects_single_usb_device_okay_plus_id` / `tport_local_selects_single_local_device_okay_plus_id` → `OKAY` + 8-byte id
- `tport_usb_in_mixed_topology_picks_usb_local_picks_tcp` → `-d` picks USB, `-e` picks TCP
- `tport_usb_with_two_usb_devices_fails_more_than_one_usb_device` → `more than one USB device`
- `tport_usb_with_only_a_tcp_device_fails_no_devices_found` → `no devices found`
- `tport_local_with_only_a_usb_device_fails_no_emulators_found` → `no emulators found`
- `tport_usb_untagged_single_device_replies_okay_plus_id` → `kind: None` backward-compat

`adb root` reconnect-handshake assertion points (`host-transport-id:` routing +
`wait-for-...-disconnect`, all in `frontend.rs` tests):

- `host_transport_id_routes_to_dispatch_host_serial` → `host-transport-id:1:get-state` over `["aaa","zzz"]` resolves N→serial and answers `device`
- `host_transport_id_invalid_id_fails` → `invalid transport id`
- `host_transport_id_out_of_range_fails` → `no device for transport id`
- `host_wait_for_disconnect_with_no_devices_returns_okay_immediately` → empty list → immediate `OKAY` (exercises the `disconnect` absent-target path without a 60 s wait)
- `is_host_serial_sub_recognizes_wait_for_family` → proves a TCP `ip:port` serial with a `wait-for-*` sub splits correctly (lockstep with `dispatch_host_serial`)

Bare `host:get-state` / `host:get-serialno` transport-any data-query assertion
points (`frontend.rs` tests):

- `bare_get_state_with_single_device_replies_data` → single device → `OKAY0006device`
- `bare_get_serialno_with_single_device_replies_serial` → `OKAY` + the serial
- `bare_get_state_with_no_devices_fails_no_devices` → `no devices/emulators found`
- `bare_get_state_with_multiple_devices_fails_more_than_one` → `more than one device/emulator`
- `bare_get_state_matches_pinned_get_state` / `bare_get_serialno_matches_pinned_get_serialno` → bare byte-equals the pinned `host-serial:<serial>:<sub>` form

### Runtime selftest (device-backed, `adboost_cli selftest`)

Unit tests use a `MockBackend`; the *reported* regression only manifests with a
**real** multi-device setup driven by the **official `adb` client** (which is
what issues `host:tport:any` before `shell:`). Covered by a parity case:

- `adboost_cli/src/selftest/parity.rs::case_official_adb_ambiguous_shell` — runs
  `adb -P <port> shell echo …` with **no `-s`** against adboost's in-process
  server; `Passed` iff stderr contains `more than one device` (the stable
  substring of the AOSP `more than one device/emulator`), and explicitly flags
  `device not found` as a REGRESSION.
- Wired in `selftest/mod.rs::run_through_server_phase` under `if multi {}` —
  runs **once per run** (ambiguity is a whole-device-set property, not per
  serial), never emitted in single-device mode, `Skipped` when no `adb` on PATH.
  Note the rest of the multi-device suite selects by serial (the `-s`
  equivalent), so this is the one case that exercises the ambiguous path.

---

## The device-list family: `devices`/`devices-l`/`track-devices`/`track-devices-l` share ONE renderer

The four device-list services differ along exactly two axes — one-shot vs
streaming, and short vs long rendering — so both axes are modeled once and
every service is a thin combination of shared pieces:

| Service | Shape | Format |
|---|---|---|
| `host:devices` | one-shot `OKAY`+framed (`host_data_query_payload`) | `Short` |
| `host:devices-l` | one-shot (`host_data_query_payload`) | `Long` |
| `host:track-devices` | streaming (`serve_track_devices`) | `Short` |
| `host:track-devices-l` | streaming (`serve_track_devices`) | `Long` |

- `DeviceListFormat` (`frontend.rs`, private enum `Short`/`Long`) is the
  rendering axis; `format_devices(devices, format)` is the single renderer;
  `serve_track_devices(stream, format)` is the single streaming loop. A new
  device-list service = one `dispatch_host_service` arm + (at most) one enum
  variant — never a second renderer.
- **The invariant**: the one-shot and streaming variants of a format MUST
  render byte-identical lines. A `track-devices-l` client (Android Studio's
  adblib `SessionDeviceTracker`) parses the stream with the same
  `DeviceListTextParser(LONG_FORMAT)` that handles `devices-l` output. Locked
  by `track_devices_l_payload_matches_one_shot_devices_l` and the runtime
  `protocol.track_devices_family` selftest case.
- Streaming services live in `dispatch_host_service`'s `match svc` arms, NOT
  in `host_data_query_payload` — that funnel is only for one-shot
  `OKAY`+framed-payload (or FAIL) data queries.

> **Why `track-devices-l` exists (the reported outage)**: modern Android
> Studio's adblib `SessionDeviceTracker.pickBestFormat` picks its tracking
> service from `host:features`: without the `devicetracker_proto_format`
> feature it chooses LONG and sends `host:track-devices-l`, with **no FAIL
> fallback** to legacy `track-devices`. A missing arm therefore reads as an
> EMPTY device list in the IDE while `adb devices` works fine (reported by
> xdb: AS 2026.1.4 against an xdb-owned `:5037`).

> **Gotcha — the proto variants (P1) must ship together with their feature
> flag.** `track-devices-proto-binary`/`-proto-text` and the
> `devicetracker_proto_format` `host:features` entry are ONE unit:
> advertising the flag without the service steers adblib onto the proto path
> and re-breaks AS with `unknown host service: track-devices-proto-binary`;
> shipping the service without the flag is merely feature-less (AS keeps
> using `track-devices-l`, which works). Implement both in one change or
> neither.

### Tests Required (assertion points)

Inline in `frontend.rs` (run with `--features "server,usb"`):

- `track_devices_streams_short_format_snapshot` → `OKAY` + framed
  `serial\tstate` (legacy regression: NO `transport_id`)
- `track_devices_l_streams_long_format_snapshot` → byte-locked long payload
  (`zzz\tdevice transport_id:2\naaa\tdevice transport_id:1`)
- `track_devices_l_payload_matches_one_shot_devices_l` → the streaming first
  snapshot byte-equals the `host:devices-l` reply
- `format_devices_short_and_long` (the pure renderer, both formats)

Runtime selftest (`adboost_cli selftest`):

- `adboost_cli/src/selftest/protocol_cases.rs::case_track_devices_family` —
  speaks the smartsocket protocol DIRECTLY over TCP (the
  adblib/Android-Studio-shaped client; `adb` CLI invocations never send
  `track-devices-l`), wired once per serial in `run_through_server_phase`
  under the `protocol` suite: the first `track-devices-l` snapshot is
  long-format, contains the serial + `transport_id`, and byte-equals
  `host:devices-l`; the legacy `track-devices` snapshot byte-equals
  `host:devices`. `protocol_cases::TrackStream` (open + `next_snapshot` with
  timeout) is the reusable raw-protocol driver.
- `interactive.rs::case_track_devices_l_hotplug` — operator unplug/replug with
  the stream open; fresh snapshots must arrive on the SAME connection without
  re-requesting (the push-on-change contract AS's device list depends on).

## Unknown-service requests WARN-log through one funnel

**The bug this prevents**: every unknown-service FAIL branch used to emit no
log at all, so a protocol gap was invisible server-side — the AS
blank-device-list outage above could only be diagnosed by decompiling the
client. Since then, every client-triggerable "cannot route this service" FAIL
funnels through `warn_unsupported_service(service, stream)` (`frontend.rs`),
which logs one line with the exact service string and the requesting peer:

```
WARN adboost::server::frontend: unsupported adb service: host:track-devices-l (peer: 127.0.0.1:60583)
```

The five funneled sites (keep in lockstep when adding a new rejection path):

| Site | FAIL wording (AOSP-exact, unchanged) |
|---|---|
| `dispatch_host_service` (no `host:`/family prefix) | `unknown service: {service}` |
| `dispatch_host_service` (`other` arm) | `unknown host service: {other}` |
| `dispatch_host_serial` (`other` arm) | `unknown host-serial sub-service: {other}` |
| `serve_local_service` (`map_local_service` reject) | `service not supported: {svc}` (the client-facing reason may be rewritten by the `local_service_reject_reason` backend hook; the log records the frontend's routing decision) |
| `serve_reverse` (unknown `reverse:` sub) | `unsupported reverse service: {service}` |

Rules:

- **Logging is additive only**: the FAIL reply bytes are the AOSP wire
  contract and must not change (locked by the
  `unknown_{service,host_service,host_serial_sub}_fails_with_aosp_wording`
  tests).
- The pinned-prefix dispatchers (`dispatch_host_serial` /
  `dispatch_host_kind` / `dispatch_host_transport_id`) carry the client's
  ORIGINAL `service` string down to the log — the parsed `serial`/`sub` have
  lost their family prefix, and the diagnostic must show exactly what the
  client sent.
- The message shape is built by the pure `unsupported_service_log_line`
  (unit-tested via `unsupported_service_log_line_includes_service_and_peer`);
  the project has no log-assertion machinery in unit tests, so the pure
  builder is the wording lock.

## Device control services are bridged verbatim like `shell:` v1

`map_local_service` (`frontend.rs`) recognizes a **control-service** family —
`tcpip:<port>`, `usb:`, `root:`, `unroot:`, `reboot:[mode]`, `remount:`,
`enable-verity:`, `disable-verity:` — and forwards each as
`ADBLocalCommand::Raw(service)` with **no capability gating**. The justification
(and the reason this is safe) is that every one is structurally identical to bare
`shell:` v1: a single OPEN, a short textual reply, then CLSE.
`bridge_tcp_session`'s half-close copy already handles that
request/response-then-close shape, so no separate "one-shot" path is needed.
`is_control_service()` is the pure predicate; unit-tested for the exact member
set (and against look-alikes like `usbfoo:` / `rebooting:` / `tcp:`).

> **Gotcha**: `tcpip:`/`usb:`/`reboot:`/`root:`/`unroot:` restart adbd, which
> drops the USB connection. The bridge observes EOF normally; the backend's
> `get_or_open` replaces the now-stale cached connection on the next open. Do
> **not** treat the post-restart connection drop as an error. After such a
> restart, a modern client runs the reconnect handshake (see *The `adb root` /
> `adb tcpip` reconnect handshake* above).

> **Known-acceptable: a back-to-back control service can return SILENTLY.** When
> a control service (e.g. `adb root`) is issued immediately after a prior one
> that is still restarting adbd, its `OPEN` can land on the connection in the
> instant adbd tears it down — adbd closes the stream before emitting its
> `"restarting adbd as root"` / `"adbd is already running as root"` text, so the
> bridge sees a clean EOF and the client gets an **empty** reply (exit 0, no
> message). Real-hardware back-to-back `adb root; adb unroot` ×4 confirmed this is
> occasional (~3/16), the control service still TOOK EFFECT (a follow-up `root`
> reports `already running as root`), and native `adb` shows the same multi-second
> latency under the same race. This is **not a regression** — do not "fix" it by
> failing the command or retrying the control service (re-running a restart-class
> service is not idempotent-free). The connect-layer re-enumeration retry
> (`is_retryable_open_error` reopen-window family + small in-place
> `CONNECT_TRANSIENT_MAX_ATTEMPTS`) ensures the command SUCCEEDS; surfacing the
> reply text across an adbd-teardown race would need bridge-layer work (tracked
> separately), not a control-service retry.

> **`unroot:` is bridged but also a first-class `ADBLocalCommand::Unroot`.** As a
> client library, adboost exposes `ADBDeviceExt::unroot()` mirroring `root()`
> across every device type (proxy / message / USB / TCP); as a server frontend it
> bridges a client's `unroot:` verbatim via `is_control_service`. The two paths
> are independent — the frontend never constructs the `Unroot` variant (it uses
> `Raw`), and the library API never goes through `is_control_service`.

**Runtime coverage**: `adboost_cli/src/selftest/cases.rs::case_root_unroot_cycle`
exercises a real `root:` → `unroot:` round-trip. It is an **automated** case (no
operator prompt) that runs **THROUGH the in-process server** via an
`ADBProxyDevice` (NOT a direct USB connection), wired by
`mod.rs::run_root_unroot_through_server` as the LAST through-server case, ONCE on
the first serial.

> **Why through the server, not a direct USB connection (the fixed bug).** USB
> allows exactly ONE exclusive interface claim per device. The through-server
> phase's backend already holds that single cached claim. The previous version of
> this case opened its OWN direct `PersistentUsbConnection` in a separate
> post-through-server phase, so it contended for the same claim and failed with
> `Device is busy` forever (`DeviceBusy`, retried 20× then `Failed`). Worse, the
> direct path never exercised the backend code (`get_or_open` /
> `open_session_with_reopen` retry) that the `adb root` reconnect actually rides.
> Routing it through the SAME server (a) reuses the backend's single cached claim
> (no competing claim) and (b) exercises the real production `adb root` reconnect
> path: frontend bridge → `DefaultDeviceBackend::get_or_open` /
> `open_session_with_reopen` retry, which rides out adbd's restart + USB
> re-enumeration.

**Behavioral production-build detection (no reply-text parsing).**
`ADBProxyDevice::root()`/`unroot()` return `Result<()>` with NO reply text, so
the case detects build policy by BEHAVIOR: it reads `id -u` (via the pure,
unit-tested `uid_is_root`), calls `root()`, then re-reads `id -u`. If the uid is
now `0` → root gained. If `root()` returned `Ok(())` BUT the post-root uid is
still non-zero → production/`user` build where adbd cannot run as root →
`Outcome::Skipped` (NOT `Failed`). Only a genuine transport/protocol error from
`root()`/`unroot()` is `Failed`. After gaining root it calls `unroot()` and
asserts the uid returns to non-zero. All shell calls go through the proxy/server,
so the backend's `get_or_open` / `open_session_with_reopen` retry handles the
post-restart not-ready window — no direct-USB settle waits are added.

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

### Customizing the FAIL reason on a `map_local_service` rejection

When `map_local_service` rejects a service (a wire-framing service the device's
banner lacks, an unbridged service, or a malformed `tcp:` port), the reason is
otherwise **frontend-hardcoded** (`service not supported: <svc>` /
`invalid tcp port: <svc>`). `serve_local_service` consults
`DeviceBackend::local_service_reject_reason(serial, service, default_reason)`
immediately before the single `protocol::fail`, so an injected backend that
bridges a non-adbd endpoint (SSH/serial/proxy/sim) can substitute or **wrap** an
actionable reason (e.g. point the user at its own transfer path). Invariants:

- **Reason only, never routing/gating.** It rewrites the FAIL text of an
  *already-decided* rejection; it cannot accept an otherwise-rejected service,
  change the opened `ADBLocalCommand`, or alter advertised features (honest
  banner intact).
- **One seam, scoped to the map-rejection path.** It fires on *every*
  `map_local_service` `Err`; the backend self-selects by `service` and returns
  `None` for the rest. It is deliberately **not** on the `open_local_service`
  failure path — there the reason is already the backend's own error (`open
  session failed: {e}`), so a hook would be redundant.
- **Default `None` → byte-identical.** A backend that does not override emits the
  exact same single FAIL frame as before (locked by
  `reject_reason_hook_default_backend_is_byte_identical`).

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

## Disconnect cleanup: a device's `forward` / `reverse` rules are released when its transport vanishes

**The bug this fixes**: a `forward` rule is registered in the **server-global**
`ForwardRegistry` (`forward.rs`), which is keyed by local port and bound to no
device lifetime. Unplugging the USB device (or `host:disconnect`ing a TCP one)
left the host-side listener bound and the rule visible in `forward --list`
forever — standard `adb` releases it. `reverse` had the mirror leak: the engine's
data pump stopped when the connection died, but its entry lingered in the
backend's `reverse` map until `shutdown()`.

**The seam** (backend is the device-lifecycle source of truth; frontend owns the
forward registry — so cleanup is a cross-layer event flow, NOT a local patch):

| Piece | Location | Role |
|---|---|---|
| `LifecycleEvent::Disconnected(serial)` | `backend.rs` | the internal event; **distinct from** `subscribe_changes` (that serves `track-devices` clients and fires only when one is attached) |
| `DeviceBackend::subscribe_lifecycle()` | `backend.rs` (default: closed stream) | the stream the frontend drains; `DefaultDeviceBackend` overrides it |
| USB hotplug-diff watcher | `default_backend.rs::spawn_usb_disconnect_watch` | keeps a `HashSet` of present serials; on each nusb hotplug event, emits `Disconnected` for every serial that left. Separate from the `subscribe_changes` watcher because cleanup needs the **diff**, not a snapshot |
| TCP `disconnect` emit | `default_backend.rs::disconnect` | `emit_disconnected` for each removed serial (single + empty-target-all paths) |
| `handle_disconnects` | `frontend.rs` | spawned by `serve()` (unless `Retain`); applies the policy per event |
| `ForwardHandle::{release,release_all}` | `forward_handle.rs` | the caller-facing active-cleanup API; `serve()` consumes `self`, so obtain it via `frontend.handle()` **before** serving |

**`OnDisconnect` policy** (`on_disconnect.rs`, mirrors `ReversePolicy`'s
enum-plus-closure shape) — set via `AdbServerFrontendBuilder::on_disconnect`:

| Variant | Behavior | Notes |
|---|---|---|
| `ReleaseAll` (**default**) | release the serial's forward listeners **and** reverse rules | aligns with standard `adb`; the default so existing callers get correct behavior |
| `Retain` | keep everything; caller releases via `ForwardHandle` | `serve()` does not even spawn `handle_disconnects` for this variant |
| `Notify(Arc<dyn Fn(&str)>)` | invoke callback with the serial; release **nothing** | pure notification; callback decides via `ForwardHandle` |

**Unified semantics**: one policy governs **both** forward and reverse for a
serial — a disconnected device loses everything it was forwarding.
`ForwardHandle::release` clears the serial's forward rules (`remove_by_serial`)
**and** its reverse rules.

> **Gotcha — the disconnect path must NOT reopen the dead connection.** Reverse
> cleanup on disconnect uses `DeviceBackend::release_reverse`, which (in
> `DefaultDeviceBackend`) just drops the `reverse` map entry. Do **not** route it
> through `reverse_remove_all` → `reverse_engine` → `get_or_open`: that re-opens
> the just-unplugged device to reach its engine, which fails and re-leaks. The
> data pump is already stopping (its connection's reader died), so only the
> in-memory rule entry needs dropping.

> **Gotcha — `release_all` can't see reverse-only serials.** It fans reverse
> cleanup over the serials present in the *forward* registry. A serial with
> reverse rules but no forward rule is invisible to it; release such a serial
> explicitly via `release(serial)` (the per-serial disconnect path does this
> correctly because the event carries the serial directly).

### Tests Required (assertion points)

Inline `#[cfg(test)] mod tests`, run with `--features server`:

- `on_disconnect.rs`: `default_is_release_all`, `debug_does_not_leak_closure`,
  `notify_callback_receives_serial`
- `forward.rs`: `registry_remove_by_serial_only_drops_that_serial`
- `forward_handle.rs`: `release_drops_only_that_serial_forward_and_its_reverse`,
  `release_all_clears_forwards_and_fans_reverse_over_serials`
- `frontend.rs`: `release_all_policy_drops_forward_and_reverse_on_disconnect`,
  `notify_policy_invokes_callback_and_releases_nothing`,
  `retain_policy_releases_nothing` — the handler is **source-agnostic** (USB
  hotplug and TCP `host:disconnect` both arrive as
  `LifecycleEvent::Disconnected(serial)`), so these cover both transports.

### Common Mistake: opening a USB device right after a case that re-enumerated it (selftest)

**Symptom**: An interactive selftest case fails to open the device with
`USB transfer error: unknown (error 0xe00002ed)` — e.g.
`case_reboot_recovery` reporting `cannot open device to reboot: …0xe00002ed`,
even though the device is physically present.

**Cause**: The selftest phases run cases in sequence against one shared device.
Several of them leave the device **mid-USB-re-enumeration**:
`case_usb_forward_release_on_unplug` (operator replugs),
`case_tcpip_through_server` (`restore_usb_mode` issues `usb:`, which restarts
adbd and drops+re-adds the USB device), the automated `case_root_unroot_cycle`
(`root:`/`unroot:` each restart adbd; it now runs THROUGH the server as the last
through-server case, so its restart is absorbed by the backend's reconnect retry
rather than a direct re-open), and any reboot. `nusb` re-opens
the device under a fresh `IOKit` registry id, but adbd is not yet ready to accept a
CNXN handshake. A *bare* `PersistentUsbConnection::new_from_serial` issued within
~2 s of that transition hits the not-ready endpoint and fails. The reported
failure surfaced at the `tcpip → reboot_recovery` seam, not within a single case.

> **`IOKit` code decode (corrected).** Verified against the pinned
> `io-kit-sys 0.5.0`: `0xe00002ed` = **`kIOReturnNotResponding`** ("device not
> responding"), NOT `kIOReturnAborted` (which is `0xe00002eb`). The sibling
> transient `0xe00002c0` = **`kIOReturnNoDevice`** ("no device"). nusb 0.2.3 maps
> `NoDevice → TransferError::Disconnected` and has no named variant for
> `NotResponding`, so it surfaces as `TransferError::Unknown(0xe000_02ed)`. Both
> are the genuine *not-ready-yet* family right after re-enumeration; the code layer
> alone CANNOT distinguish a transient `NoDevice` from a real unplug — only a
> **bounded retry budget** keeps a retry honest.

> **Expected log noise during re-enumeration (NOT a bug).** When the retry rides
> out the not-ready window you WILL see, per failed attempt: nusb's own
> `ERROR ... Failed to submit Out transfer ... e00002c0` / `failed to create IOKit
> PlugInInterface ... 0xe00002be` (third-party, severity not ours to set), and
> adboost's `WARN PersistentUsb reader/writer error (fatal): ...0xe00002ed` +
> `could not enqueue connection CLSE on drop: writer task gone` (the connection
> legitimately died on the adbd restart; the next open reconnects). These are the
> *visible cost of the bounded retry succeeding*, confirmed benign on real hardware
> (selftest `through_server.root_unroot_cycle` and `tcpip.shell_through_tcp_device`
> both pass through exactly this noise). Do NOT treat these WARN/ERROR lines as a
> regression. To quiet them in a deployment, set `RUST_LOG=nusb=warn,adboost=info`
> (or lower the per-attempt teardown to DEBUG) — a deliberately-untaken cosmetic
> change, since the words "fatal"/"error" are accurate *for that one connection*
> even though the layer above recovers.

**Fix**: Two layers (both applied):
1. **Open-with-retry at the consumer** (the durable fix): open via
   `open_device_with_retry(serial, budget)` — retry `new_from_serial` on
   `POLL_INTERVAL` within a ~20 s budget — rather than a bare call. This does not
   depend on the previous case's behavior.
2. **Hand the device back stable** (reduces downstream waiting): a case that
   re-enumerates the device should, best-effort, `wait_for_presence` + then
   confirm openability (`open_device_with_retry` / `verify_shell_after_recovery`)
   before returning, so the next case starts from a ready device. This step MUST
   NOT change the case's own `Outcome` (the core conclusion was already computed)
   — it only `tracing::warn!`s on failure.

**Prevention**: Never issue a bare device open immediately after an operation
that restarts adbd or re-enumerates USB (unplug/replug, `tcpip:`/`usb:` mode
switch, `reboot:`). Use `open_device_with_retry`. Keep the "reboot_recovery runs
last" ordering invariant — but do not rely on ordering alone, since any
re-enumerating case can precede another.

### The backend now retries the same transients (the production `adb root` path)

The selftest's `open_device_with_retry` is a *consumer* discipline; the
production `adb root` reconnect path goes through the server backend, which had
ZERO retry until now. Two complementary, bounded retries close that gap so a
client going **through the server** (the path PR2's `wait-for-disconnect`
handshake feeds into) no longer needs its own retry:

| Layer | Where | Bound | Covers |
|---|---|---|---|
| Handshake (all consumers) | `PersistentConnection::do_connect` (`usb/persistent.rs`) | `CNXN_MAX_ATTEMPTS` (8) + 100 ms settle | the **CNXN race**: a transient transfer error on the CNXN write/read is settled + retried in the existing bounded loop instead of propagating |
| Backend open (server only) | `DefaultDeviceBackend::get_or_open` + `open_session_with_reopen` (`server/default_backend.rs`) | ~10 s budget / 500 ms poll | the **first-OPEN race** (a connection that dies on its first OPEN is dropped + reopened) **and** brief `DeviceNotFound` (device momentarily absent from enumeration — which the handshake layer structurally cannot see) |

- **Transient classification = `TransferError` variant + a bounded budget, NEVER
  code-only.** `is_transient_connect_error` matches exactly
  `TransferError::Unknown(0xe000_02ed)` (`NotResponding`) and
  `TransferError::Disconnected` (`NoDevice`); `Stall` is deliberately excluded
  (it can be a real endpoint fault). The bound is what makes retrying `NoDevice`
  safe — a real unplug never recovers within the window, so it still fails fast.
- The backend additionally retries `RustADBError::DeviceNotFound` (re-enumeration
  gap) but NOT `DeviceBusy` (another process holds the single USB claim — waiting
  won't clear it).
- `get_or_open` releases the `conns` mutex across the multi-second retry (it only
  guards the cache lookup/insert), so one device's re-enumeration window never
  serializes other callers.
- `open_session_with_reopen` is wired into `open_local_service` (the
  `adb root` → `shell:` path). `open_sync_session` / `open_shell_v2` still get the
  CNXN-race retry via `get_or_open` but not the first-OPEN reopen (their distinct
  session types preclude the shared helper); extend them if a first-OPEN race is
  ever observed there.
- This aligns with AOSP's transport reconnect handler (bounded retry + backoff;
  a single transient (re)open is not surfaced to the user) and with the
  `prefer-root-cause-fix-at-contract-layer` / `tcp-async-path-missing-usb-guarantees`
  principles (fix the shared handshake + the lifecycle-owning layer, not a local
  patch).
