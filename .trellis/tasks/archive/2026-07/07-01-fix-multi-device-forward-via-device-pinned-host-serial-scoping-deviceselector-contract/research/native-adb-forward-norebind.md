# Research: Native adb `forward` / `norebind` wire protocol & CLI semantics

- **Query**: Exact AOSP wire-protocol and CLI semantics of `adb forward` re: the `norebind` flag — default behavior, wire strings, device-pinned forms, killforward.
- **Scope**: external (AOSP source) + minor internal cross-reference
- **Date**: 2026-07-01

## Verdict (TL;DR)

- **DEFAULT is REBIND.** `adb forward tcp:1111 tcp:2222` (no `--no-rebind`) sends
  `forward:<local>;<remote>` with **NO** `norebind:` segment. The server silently
  **replaces** any existing rule for the same `<local>`.
- `--no-rebind` is the opt-in that inserts the `norebind:` segment, producing
  `forward:norebind:<local>;<remote>`, which makes the server **FAIL** if a rule
  for `<local>` already exists.
- Therefore a client that emits a **bare `host:forward:<local>;<remote>` (no
  `norebind`) already matches native adb's default.** Native does NOT send
  `norebind` by default; switching the default to `norebind` would DIVERGE from
  native behavior (it would start failing on re-forward of the same local port,
  which native silently rebinds).

## Findings

### 1. Default (no `--no-rebind`) — bare `forward:`, rebind semantics

`client/commandline.cpp` (main branch), `forward`/`reverse` handler:

```cpp
} else {
    // forward <local> <remote>
    if (argc != 2) error_exit("forward takes two arguments");
    if (forward_targets_are_valid(argv[0], argv[1], &error_message) &&
        forward_dest_is_featured(argv[1], &error_message)) {
        cmd = std::string("forward:") + argv[0] + ";" + argv[1];   // <-- no "norebind:"
    }
}
```

So `adb forward tcp:1111 tcp:2222` → service `host:forward:tcp:1111;tcp:2222`.

SERVICES.TXT documents rebind as the default and no-rebind as the exception:

> `<host-prefix>:forward:norebind:<local>;<remote>`
> Same as `<host-prefix>:forward:<local>;<remote>` **except that it will fail if
> there is already a forward connection from `<local>`.**
> Used to implement `'adb forward --no-rebind <local> <remote>'`

(The plain `<host-prefix>:forward:<local>;<remote>` entry carries no such
"fail if exists" caveat — i.e. it overwrites/rebinds.)

### 2. `--no-rebind` flag — inserts `norebind:` after `forward:`

`client/commandline.cpp`:

```cpp
} else if (strcmp(argv[0], "--no-rebind") == 0) {
    // forward --no-rebind <local> <remote>
    if (argc != 3) error_exit("--no-rebind takes two arguments");
    if (forward_targets_are_valid(argv[1], argv[2], &error_message) &&
        forward_dest_is_featured(argv[2], &error_message)) {
        cmd = std::string("forward:norebind:") + argv[1] + ";" + argv[2];
    }
}
```

So `adb forward --no-rebind tcp:1111 tcp:2222` → `host:forward:norebind:tcp:1111;tcp:2222`.

**Wire form summary**

| CLI | service string (host prefix) |
|---|---|
| `adb forward tcp:1111 tcp:2222` | `host:forward:tcp:1111;tcp:2222` |
| `adb forward --no-rebind tcp:1111 tcp:2222` | `host:forward:norebind:tcp:1111;tcp:2222` |
| `adb forward --remove tcp:1111` | `host:killforward:tcp:1111` |
| `adb forward --remove-all` | `host:killforward-all` |
| `adb forward --list` | `host:list-forward` |

The optional `norebind:` segment sits **between** `forward:` and the
`<local>;<remote>` payload: `forward:[norebind:]<local>;<remote>`. Colon
placement: `forward` `:` `norebind` `:` `<local>` `;` `<remote>`.

### 3. Device-pinned forms — TWO mechanisms (important nuance)

SERVICES.TXT defines the legacy grammar with `<host-prefix>` substituted by the
device selector:

```
host-serial:<serial>:forward[:norebind]:<local>;<remote>
host-usb:forward[:norebind]:<local>;<remote>
host-local:forward[:norebind]:<local>;<remote>
```

i.e. `norebind:` follows `forward:` inside the pinned prefix too:
`host-serial:<serial>:forward:norebind:tcp:1111;tcp:2222`.

**However, MODERN adb (main branch) does NOT build that pinned string for
`forward`.** In `commandline.cpp` the `host_prefix` is the literal `"host:"`,
and the command is sent with `force_switch = true`:

```cpp
host_prefix = "host:";                       // NOT host-serial:<serial>:
...
cmd = std::string("forward:") + argv[0] + ";" + argv[1];
...
unique_fd fd(adb_connect(nullptr, host_prefix + cmd, &error_message, true));
//                                                                    ^^^^ force_switch_device
```

`_adb_connect` (client/adb_client.cpp) then forces a **transport switch first**
(because `force_switch` overrides the normal "service starts with host → no
switch" rule):

```cpp
if (!service.starts_with("host") || force_switch) {
    std::optional<TransportId> transport_result = switch_socket_transport(fd.get(), error);
    ...
}
```

`switch_socket_transport` sends the device selector as a *separate* service on
the same connection BEFORE `host:forward:...`:

```cpp
if (__adb_transport_id) {
    service += "host:transport-id:"; service += std::to_string(__adb_transport_id);
} else if (__adb_serial) {
    service += "host:tport:serial:"; service += __adb_serial;   // -s <serial>
} else {
    service += "host:tport:"; service += transport_type;         // any/usb/local
}
```

So on the wire, modern `adb -s SERIAL forward tcp:1111 tcp:2222` is:

```
1) host:tport:serial:SERIAL      (switch/select device, returns transport id)
2) host:forward:tcp:1111;tcp:2222 (bare "host:" prefix, on the now-pinned conn)
```

NOT `host-serial:SERIAL:forward:...`.

**By contrast**, `format_host_command` (used by *other* per-device queries like
`get-state`, `features`, `get-serialno`) DOES build the pinned prefix form:

```cpp
std::string format_host_command(const char* command) {
    if (__adb_transport_id)   return "host-transport-id:<id>:" + command;
    else if (__adb_serial)    return "host-serial:<serial>:"   + command;
    // else host / host-usb / host-local
}
```

**Conclusion for device-pinned forward:** an adb *server* must still ACCEPT the
legacy `host-serial:<serial>:forward[:norebind]:<local>;<remote>` form (it is in
SERVICES.TXT and older/other clients send it), but the reference CLIENT pins the
device via a preceding `host:tport:serial:<serial>` (or `host:transport-id:<id>`)
switch and then sends a **bare `host:forward:...`**. Both are valid; colon
placement of `norebind:` is identical (`...forward:norebind:<local>;<remote>`).

### 4. killforward (device-pinned)

- Bare / switched: `host:killforward:<local>`  (e.g. `host:killforward:tcp:1111`)
- Legacy pinned:   `host-serial:<serial>:killforward:<local>`
- transport-id:    `host-transport-id:<id>:killforward:<local>`
- Remove-all:      `<host-prefix>:killforward-all`  (no payload)

`killforward` takes ONLY the `<local>` endpoint (no `;<remote>`, no `norebind`).
From `commandline.cpp`: `cmd = std::string("killforward:") + argv[1];`
SERVICES.TXT: "Remove any existing forward local connection from `<local>`."

### 5. killforward-all scoping — GLOBAL, ignores the selected device

**Verdict: `killforward-all` is process-GLOBAL. It removes EVERY forward rule
across ALL devices, regardless of any `-s <serial>` / device selection. The
client's `forward_remove_all()` should keep the bare global `host:killforward-all`
(a serial-scoped `host-serial:<serial>:killforward-all` gains nothing — a
compliant server still wipes all devices' rules).**

#### 5.1 What the client sends

`client/commandline.cpp` (~lines 1928-1930) builds the SAME `killforward-all`
command regardless of `-s`:

```cpp
} else if (strcmp(argv[0], "--remove-all") == 0) {
    if (argc != 1) error_exit("--remove-all doesn't take any arguments");
    cmd = "killforward-all";
}
...
// host_prefix is the literal "host:" for forward (NOT host-serial:<serial>:)
unique_fd fd(adb_connect(nullptr, host_prefix + cmd, &error_message, true));
//                                = "host:killforward-all"        force_switch=true
```

So both `adb forward --remove-all` and `adb -s SERIAL forward --remove-all` send
the service string **`host:killforward-all`**. Because `force_switch=true`, when
`-s` is present the client first performs a transport switch
(`host:tport:serial:SERIAL`) on the connection — BUT that selected transport is
irrelevant to the handler (see 5.3): `killforward-all` never reads it.

Answer to Q3 (plain `adb forward --remove-all`, no `-s`): also
`host:killforward-all` (still preceded by a `host:tport:any` switch due to
`force_switch=true`, which just picks/validates a device but does not scope the
removal).

#### 5.2 Server dispatch — killforward-all takes NO transport

`adb.cpp` `handle_forward_request` (~lines 1093-1101, tag `android-14.0.0_r1`):

```cpp
if (!strcmp(service, "killforward-all")) {
    remove_all_listeners();          // <-- NO transport argument passed
#if ADB_HOST
    SendOkay(reply_fd);              // host: 1st OKAY = connect
#endif
    SendOkay(reply_fd);              // 2nd OKAY = status  (double-OKAY)
    return true;
}
```

Contrast with the per-rule branch (~lines 1103-1151) which DOES acquire and pass
a transport:

```cpp
if (!strncmp(service, "forward:", 8) || !strncmp(service, "killforward:", 12)) {
    atransport* transport = transport_acquirer(&error);   // per-device
    ...
    r = remove_listener(pieces[0].c_str(), transport);            // killforward:<local>
    ... install_listener(pieces[0], pieces[1].c_str(), transport, ...);  // forward:...
}
```

So only `forward:` / `killforward:<local>` are transport-aware at dispatch;
`killforward-all` and `list-forward` are handled before any transport is
acquired and are inherently global.

#### 5.3 Listener registry is process-global (one list, not per-transport)

`adb_listeners.cpp` (tag `android-14.0.0_r1`):

- Single global registry: `static ListenerList& listener_list` guarded by one
  process-wide mutex (~lines 70-74). NOT keyed/partitioned by transport.

- `remove_all_listeners()` (~lines 156-167) takes **no parameter** and erases
  **every** entry except smart-sockets — it never inspects `->transport`:

```cpp
void remove_all_listeners() EXCLUDES(listener_list_mutex) {
    std::lock_guard<std::mutex> lock(listener_list_mutex);
    auto iter = listener_list.begin();
    while (iter != listener_list.end()) {
        if ((*iter)->connect_to[0] == '*') {  // never remove smart sockets
            ++iter;
        } else {
            iter = listener_list.erase(iter);  // removes regardless of transport
        }
    }
}
```

- Each listener records its owning transport (`listener->transport = transport;`
  ~line 246) only so an individual rule can be re-pointed on rebind
  (`install_listener` ~lines 207-210) and torn down on that transport's
  disconnect (`listener_disconnect` ~lines 114-120). It is NOT used to scope
  `remove_all_listeners`.

- Note also `remove_listener(local_name, transport)` (~lines 144-154) matches by
  `local_name` ALONE inside the loop; the `transport` arg is not consulted to
  filter — a `killforward:<local>` removes the rule for that local endpoint
  whichever device owns it.

- `format_listeners()` (list-forward, ~lines 126-141) likewise walks the one
  global list and prints each entry's OWN serial (`l->transport->serial`) —
  confirming rules from different devices coexist in a single global registry,
  and `list-forward` reports all of them.

Answers:
- **Q1:** `adb -s <serial> forward --remove-all` sends `host:killforward-all`
  (global), NOT `host-serial:<serial>:killforward-all`. It removes ALL forwards
  across ALL devices, not just that serial's.
- **Q2:** Forward rules are process-global (single `listener_list`); each rule
  merely *tags* its owning transport for rebind/disconnect bookkeeping. There is
  no per-transport partition, so `remove_all_listeners()` is inherently global.
- **Q3:** Plain `adb forward --remove-all` also sends `host:killforward-all`
  (identical service string).

#### 5.4 Recommendation for our client `forward_remove_all()`

Emit the bare global **`host:killforward-all`** (matches native for both the
`-s` and no-`-s` cases). Do NOT switch to `host-serial:<serial>:killforward-all`:
native never sends that, and even if a server accepted the pinned prefix, the
AOSP-defined semantics of `killforward-all` are unconditionally global — so a
serial-scoped variant would be a non-standard string with no behavioral upside
for parity. (If a genuine per-device "remove only this serial's forwards" is
ever desired, that is NOT `killforward-all`; it would require iterating
`list-forward`, filtering by serial, and issuing individual
`killforward:<local>` — a capability native adb does not expose.)


### External References

- **SERVICES.TXT** (AOSP `packages/modules/adb`, tag `android-14.0.0_r1`,
  lines 63–130): `<host-prefix>` grammar, `forward`, `forward:norebind`,
  `killforward`, `killforward-all`, `list-forward`. Authoritative statement that
  `norebind` "will fail if there is already a forward connection from `<local>`",
  and is used to implement `--no-rebind`. (main-branch `?format=TEXT` fetch
  returned empty; the `android-14.0.0_r1` tag has identical wording.)
- **client/commandline.cpp** (main branch, ~lines 117–133 help text; ~lines
  1910–1966 forward/reverse handler): builds `forward:` vs `forward:norebind:`;
  `killforward:`, `killforward-all`, `list-forward`; sends via
  `adb_connect(nullptr, "host:"+cmd, err, /*force_switch=*/true)`.
- **client/adb_client.cpp** (main branch):
  - `_adb_connect` (~line 158) — `force_switch` forces `switch_socket_transport`.
  - `switch_socket_transport` (~line 80) — emits `host:transport-id:<id>` /
    `host:tport:serial:<serial>` / `host:tport:<type>`.
  - `format_host_command` (~line 415) — emits `host-serial:<serial>:<cmd>` /
    `host-transport-id:<id>:<cmd>` / `host[-usb|-local]:<cmd>` for non-forward
    per-device queries.
- **adb.cpp** (`handle_forward_request`, tag `android-14.0.0_r1`):
  - `list-forward` (~line 1083) and `killforward-all` (~lines 1093-1094 →
    `remove_all_listeners()` with NO transport) handled BEFORE any transport is
    acquired — global.
  - `forward:` / `killforward:` (~lines 1103-1151) call `transport_acquirer` and
    pass the transport to `install_listener` / `remove_listener` — per-device.
- **adb_listeners.cpp** (tag `android-14.0.0_r1`):
  - single global `listener_list` (~lines 70-74);
  - `remove_all_listeners()` (~lines 156-167) — no param, erases all non-smart
    sockets regardless of transport;
  - `remove_listener(local_name, transport)` (~lines 144-154) — matches by
    `local_name` only;
  - `install_listener` (~lines 188-254) — tags `listener->transport` for
    rebind/disconnect only;
  - `format_listeners()` (~lines 126-141) — global list, prints each rule's own
    serial.

### Related internal code (cross-reference only — not modified)

- `adboost/src/server/forward.rs:43` `parse_forward` — already strips optional
  `norebind:` prefix and records `norebind: bool`. Grammar comment (line 11):
  `host:forward:[norebind:]<local>;<remote>`. Matches AOSP.
- `adboost/src/server/frontend.rs:850-905` `serve_forward` — enforces `norebind`
  against existing rule BEFORE binding; default (no norebind) rebinds via
  `ForwardRegistry::insert` which aborts+replaces the old rule
  (`forward.rs:99-120`). Matches AOSP default rebind semantics.
- `adboost/src/server/frontend.rs:481-493` routes
  `host-serial:<serial>:forward:...` / `killforward:...` (legacy pinned form) —
  server accepts it, consistent with SERVICES.TXT.

## Caveats / Not Found

- Could not fetch `main`-branch `SERVICES.TXT` via `?format=TEXT` (returned 0
  bytes; the tagged `android-14.0.0_r1` copy was used — the forward/norebind
  wording is stable across releases).
- The `forward.rs` grammar/`frontend.rs` routing above is the adboost **server**
  side. This research answers the **native client** wire form. If the current
  task's bug is on the *adboost client* side, note the two valid device-pinning
  strategies (tport-switch + bare `host:forward` vs. legacy
  `host-serial:<serial>:forward`) — either is accepted by a compliant server,
  but the reference adb client uses the tport-switch path for forward.
- `killforward-all` scoping (Q from coordinator) is answered against
  `android-14.0.0_r1` adb.cpp / adb_listeners.cpp (main-branch `?format=TEXT`
  returned empty; the registry design — one global `listener_list`,
  parameterless `remove_all_listeners()` — is long-standing and unchanged). No
  AOSP release scopes `killforward-all` by transport.
