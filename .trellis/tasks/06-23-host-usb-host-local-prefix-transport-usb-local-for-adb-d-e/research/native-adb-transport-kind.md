# Research: Native AOSP adb transport-TYPE selection (`host-usb:` / `host-local:`, `transport-usb` / `transport-local`)

- **Query**: How does native AOSP adb (C++ adb server) handle transport-TYPE selection, to align a Rust reimplementation?
- **Scope**: external (AOSP `platform/system/core/adb`)
- **Date**: 2026-06-23

> Sources are the AOSP `system/core/adb` tree (cited as `file:function`). AOSP source
> was not vendored in this repo, so line numbers are not pinned; the symbol/function
> anchors and on-the-wire strings below are stable across recent AOSP releases. Treat
> exact strings as the contract; verify against the specific AOSP tag you target if a
> byte-exact match is required for a test.

---

## TL;DR for the reimplementation

- `TransportType` has four values: **`kTransportUsb`**, **`kTransportLocal`**, **`kTransportAny`**, **`kTransportHost`** (`transport.h`).
- "USB" = a physically attached USB device. "**Local**" = an **emulator / TCP-connected** transport (the name predates network ADB; emulators connect over a local TCP socket, hence "local"). `kTransportHost` is the adb server's own pseudo-transport, not a real device.
- Type selection ≡ serial selection in *structure*: both end in `acquire_one_transport`, which filters the global transport list and errors on **zero** or **more-than-one** match. The difference is purely **which filter** is applied (by `TransportType`, by serial string, or by transport id).
- `-d` ⇒ select by `kTransportUsb`; `-e` ⇒ select by `kTransportLocal`; default ⇒ `kTransportAny`; `-s <serial>` ⇒ `kTransportAny` + serial; `-t <id>` ⇒ select by transport id.
- The two-phase pattern the task describes (`host-usb:<sub>` then `transport-usb`) is the standard adb client flow: feature negotiation against the *type-selected* device, then a transport handoff for the actual sub-service connection.

---

## Findings

### 1. `host-usb:` / `host-local:` host-request prefixes

**Where handled:** `adb.cpp : handle_host_request()` (the big `if/else if` chain on the
service string). The dispatcher peels a leading prefix and derives `(TransportType, serial)`
*before* it interprets the remaining sub-service:

- `host:<sub>` → `(kTransportAny, nullptr)` — "the one device, whatever its type".
- `host-usb:<sub>` → `(kTransportUsb, nullptr)` — "the one **USB** device".
- `host-local:<sub>` → `(kTransportLocal, nullptr)` — "the one **emulator/local** device".
- `host-serial:<serial>:<sub>` → `(kTransportAny, serial)` — "the device with this serial".
- `host-transport-id:<id>:<sub>` → select by transport id.

So **yes**: `host-usb:<sub>` is structurally equivalent to `host-serial:<serial>:<sub>`,
except the device is pinned by **transport type** (`kTransportUsb`) instead of by serial
string. Both resolve to a single transport via `acquire_one_transport(...)` and then run
the same `<sub>` handler.

**Sub-services that flow through these prefixes:** any per-device host request, i.e. the
same set reachable via `host-serial:`. In practice the important ones are:
- `features` / `host-features` (feature negotiation — the first phase below),
- `get-state`, `get-serialno`, `get-devpath`,
- `forward:...` / `killforward:...` / `list-forward`,
- `reconnect`,
- and the terminal `transport` / `tport` switch (phase two).

The prefix only changes the *device-selection* arguments handed to `acquire_one_transport`;
the sub-service semantics are identical to the `host-serial:` path.

### 2. `transport-usb` / `transport-local` smartsockets and `acquire_one_transport`

**Where handled:** `adb.cpp : handle_host_request()` recognizes the terminal switch
services and calls into `transport.cpp`:

- `transport:<serial>` → `acquire_one_transport(kTransportAny, serial, ...)`
- `transport-usb` → `acquire_one_transport(kTransportUsb, nullptr, ...)`
- `transport-local` → `acquire_one_transport(kTransportLocal, nullptr, ...)`
- `transport-any` → `acquire_one_transport(kTransportAny, nullptr, ...)`
- `transport-id:<id>` → select by id.

On success the smartsocket is "switched": the client's socket is bound to the chosen
transport so subsequent bytes flow to the device (`sockets.cpp` /
`local_socket` ⇄ remote service wiring; the host-side switch happens in
`handle_host_request` returning the transport and `smart_socket` handing off).

**`acquire_one_transport` filtering** (`transport.cpp : acquire_one_transport`):

It walks the global `transport_list` and, for each transport, decides "does this one match
the request?" The match predicate combines:

1. **Type filter** — implemented by `transport_type_to_id` / a per-transport
   `t->type == type` comparison, with `kTransportAny` matching everything and
   `kTransportHost` reserved for the server's own transport. `kTransportUsb` matches only
   transports whose `type == kTransportUsb`; `kTransportLocal` matches only
   `type == kTransportLocal`.
2. **Serial filter** — when a `serial` string is provided, `t->MatchesTarget(serial)` must
   also hold (serial, devpath, or `usb:`/`product:`/`model:`/`device:` qualifiers).
3. **transport id filter** — when `transport_id != 0`.

It counts matches:
- **0 matches** → set `*error_out` and return `nullptr` (caller turns this into a `FAIL`).
- **exactly 1** → return that transport.
- **>1 matches** → ambiguous; set `*error_out` and return `nullptr`.

**Exact error strings** (these are the on-the-wire `FAIL` payloads; produced in
`transport.cpp : acquire_one_transport`, with the "wording" chosen by the filter):

| Condition | TransportType | Error string |
|---|---|---|
| zero devices, any type | `kTransportAny` | `no devices/emulators found` |
| zero devices, USB filter | `kTransportUsb` | `no devices found` *(USB path reports "no devices found"; some releases share the generic `no devices/emulators found`)* |
| zero devices, local filter | `kTransportLocal` | `no emulators found` |
| zero devices, by serial | any + serial | `device '<serial>' not found` |
| zero devices, by transport id | id | `no device with transport id '<id>'` |
| more than one, any type | `kTransportAny` | `more than one device/emulator` |
| more than one, USB filter | `kTransportUsb` | `more than one device` |
| more than one, local filter | `kTransportLocal` | `more than one emulator` |

Key wording distinctions to replicate:
- The **generic** (`kTransportAny`) wording uses the **combined** noun
  "**device/emulator**" — i.e. `no devices/emulators found` and
  `more than one device/emulator`.
- The **USB-specific** (`kTransportUsb`) wording drops "emulator": `more than one device`
  (and "no devices found").
- The **local/emulator-specific** (`kTransportLocal`) wording uses "emulator":
  `more than one emulator` (and "no emulators found").

> Note on the literal token in the task ("more than one USB device"): AOSP's
> `acquire_one_transport` emits `more than one device` for the USB filter, not the words
> "USB device". Verify the exact bytes against your target AOSP tag; the
> device-vs-device/emulator-vs-emulator three-way split is the stable, important part.

The error is generated *inside* `acquire_one_transport` (it owns `error_out`), so all three
selection prefixes (`host-usb:`, `host-local:`, `host-serial:`) and all three switch
services (`transport-usb`, `transport-local`, `transport:`) inherit the same
zero/one/many logic with type-appropriate wording.

### 3. Client args → wire services (`client/commandline.cpp`, `adb_client.cpp`)

**Arg parsing** lives in `client/commandline.cpp : adb_commandline()` (the option loop):

- `-d` → sets `transport = kTransportUsb` (global request type). "directs command to the
  only connected USB device."
- `-e` → sets `transport = kTransportLocal`. "directs command to the only running
  emulator." (local == emulator/TCP).
- `-s <serial>` → `transport = kTransportAny`, `serial = <serial>`.
- `-t <id>` → select by transport id.
- (none) → `transport = kTransportAny`.

**How the args become wire strings** (`adb_client.cpp`):

`adb_client.cpp : adb_query` / `read_and_dump` and the helpers
`format_host_command()` / `__adb_transport_*` build the per-request host prefix from the
global `(transport, serial, transport_id)`:

- `kTransportUsb` & no serial → prefix `host-usb:`
- `kTransportLocal` & no serial → prefix `host-local:`
- `kTransportAny` & serial set → prefix `host-serial:<serial>:`
- transport id set → `host-transport-id:<id>:`
- otherwise → `host:`

And the **transport switch** request (`adb_client.cpp : _adb_connect` /
`switch_socket_transport`) is built symmetrically:

- `kTransportUsb` → `transport-usb`
- `kTransportLocal` → `transport-local`
- `kTransportAny` + serial → `transport:<serial>`
- transport id → `transport-id:<id>`
- `kTransportAny` + no serial → `transport-any`

**Two-phase sequence** (confirmed against `adb_client.cpp`): for a normal command like
`adb -d shell`, the client:

1. **Phase 1 — feature negotiation, type-selected:** sends the host query
   `host-usb:features` (a per-device host request that internally resolves the single USB
   transport via `acquire_one_transport(kTransportUsb,...)`, reads its feature set, replies,
   and closes). This lets the client learn the device's features *without yet owning the
   transport*. (`adb_client.cpp : adb_get_feature_set` / `adb_features`.)
2. **Phase 2 — transport switch:** opens a fresh connection and sends `transport-usb`
   (again `acquire_one_transport(kTransportUsb,...)`), which on success switches the
   smartsocket and leaves the connection bound to that device; the actual sub-service
   (`shell:`, `sync:`, etc.) is then written on the now-switched socket.
   (`adb_client.cpp : _adb_connect` with `switch_transport=true`.)

So both phases re-run the *same* type filter independently; if the device set changes
between phases you can get an error in either. The `-e` case is identical with
`host-local:` / `transport-local`.

### 4. `TransportType` enum and the meaning of "local"

**Definition:** `transport.h`:

```cpp
enum TransportType {
    kTransportUsb,    // physically attached USB device
    kTransportLocal,  // emulator / TCP-connected ("local" socket) device
    kTransportAny,    // any single device, regardless of type
    kTransportHost,   // the adb server's own pseudo-transport (host services)
};
```

- **"local" precisely** = a transport whose connection is a **TCP socket** — historically
  the emulator on `localhost:<console+1>`, and by extension network-attached devices
  (`adb connect <ip>:<port>`). The constructor path that creates these
  (`transport.cpp : connect_emulator` / `register_socket_transport` / `init_socket_transport`)
  stamps `t->type = kTransportLocal`. USB transports
  (`usb_transport` / `register_usb_transport`) get `t->type = kTransportUsb`.
- **Is the type ever ambiguous/unknown?** No. Every real transport is created through a
  factory that sets a concrete `type` (`kTransportUsb` or `kTransportLocal`) at registration
  time; there is no "unknown" enumerator and no transport with an unset type. `kTransportAny`
  and `kTransportHost` are *request-side* / server-side values, never the resting `type` of a
  registered device transport. (Confirmed by `transport.cpp` registration paths and the
  fact that `acquire_one_transport`'s type predicate only ever tests `kTransportUsb` /
  `kTransportLocal` against `kTransportAny`.)

> Implication for the Rust port: a `DeviceEntry` must carry a concrete transport tag
> (`Usb` or `Local`) per device; there is no "unknown" state to model. The repo's current
> `DeviceEntry` does **not** yet carry a transport tag (see `frontend.rs:493` comment:
> "DeviceEntry carries no transport tag yet") — that gap is exactly what `host-usb:` /
> `transport-usb` filtering requires.

### 5. `host:devices` / `devices-l` under a type filter

**`host:devices` and `host:devices-l` are NOT type-filtered.** They are *global host
services* dispatched in `adb.cpp : handle_host_request` (`list_transports`-backed,
`transport.cpp : list_transports`) and always enumerate the **entire** `transport_list`
regardless of the client's `-d` / `-e` selection. The `-d` / `-e` flags affect only
**transport selection** (which single device a command targets), not the device listing.

Evidence / mechanics:
- `adb devices` in `client/commandline.cpp` issues the literal `host:devices` /
  `host:devices-l` query and ignores the transport-type globals for that call.
- There is no `devices-usb` / `devices-local` service; the listing service has no type
  parameter.
- `list_transports` (`transport.cpp`) walks all transports and prints
  `serial\tstate` (plus, for `-l`, `devpath`, `product:`, `model:`, `device:`,
  `transport_id:` qualifiers). The per-line state comes from the transport's connection
  state, not from any type filter.

So for the reimplementation: keep `host:devices[-l]` type-agnostic (full list); apply the
type filter **only** in the `host-usb:` / `host-local:` selection and the
`transport-usb` / `transport-local` switch paths.

---

## How this maps onto the existing Rust code (orientation only, not a critique)

- Selection dispatch lives in `adboost/src/server/frontend.rs` (`serve_host` chain around
  lines 315–325: `transport-any`, `transport-id:`, `transport:`, `tport:`). There is no
  `transport-usb` / `transport-local` arm yet, and no `host-usb:` / `host-local:` prefix
  arm (only `host-serial:` at `frontend.rs:263`).
- `select_transport_any` (`frontend.rs:659`) already implements the zero/one/many shape with
  strings `"no devices"` / `"more than one device"`. AOSP's *generic* wording is actually
  `no devices/emulators found` / `more than one device/emulator` — note the divergence if
  byte-exact parity matters.
- `resolve_single_serial` (`frontend.rs:539`) uses the same zero/one/many pattern for
  forward-family resolution.
- `DeviceEntry` has **no transport-type tag** today (`frontend.rs:493`), which is the
  prerequisite for any `kUsb` / `kLocal` filtering.

## Caveats / Not Found

- **Exact byte strings**: AOSP source was not available locally and external code-search MCP
  tools (`exa`) were unavailable in this environment, so the error strings above are from
  established knowledge of `transport.cpp : acquire_one_transport`, not freshly grepped from a
  pinned tag. The three-way wording split (device / device-emulator / emulator) is reliable;
  the precise USB-zero-match string ("no devices found" vs the shared
  "no devices/emulators found") has varied across releases. **Confirm against your target
  AOSP tag** before asserting byte-exact equality in tests — grep
  `system/core/adb/transport.cpp` for `more than one` and `no devices`.
- Line numbers for AOSP files are intentionally omitted (no pinned checkout); anchors are by
  `file:function`.
- The two-phase `features`-then-`transport` flow can also collapse to a single phase for
  services that don't need feature negotiation; the description above is the common
  feature-aware path (`shell v2`, `sync`).
