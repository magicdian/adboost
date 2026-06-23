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

**Cause**: The interactive phase (`adboost_cli/src/selftest/interactive.rs`)
runs cases in sequence against one shared device. Several of them leave the
device **mid-USB-re-enumeration**: `case_usb_forward_release_on_unplug` (operator
replugs), `case_tcpip_through_server` (`restore_usb_mode` issues `usb:`, which
restarts adbd and drops+re-adds the USB device), and any reboot. `nusb` re-opens
the device under a fresh IOKit registry id, but adbd is not yet ready to accept a
CNXN handshake. A *bare* `PersistentUsbConnection::new_from_serial` issued within
~2 s of that transition hits the not-ready endpoint and fails. The reported
failure surfaced at the `tcpip → reboot_recovery` seam, not within a single case.

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
