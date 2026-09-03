# PRD: adb-server frontend — `host:track-devices-l` + unknown-service logging

- **Source**: external feature request from xdb (xpeng-debug-bridge), 2026-09-03
- **Severity**: High — new Android Studio (adblib path) sees **zero USB devices**
  when xdb owns `:5037`, because its `SessionDeviceTracker` sends
  `host:track-devices-l` and the frontend replies `FAIL unknown host service`.
- **Scope**: `adboost` crate server frontend (`adboost/src/server/frontend.rs`)
  + `adboost_cli` selftest cases.

---

## 1. Background (from the xdb report)

- New Android Studio device tracking (adblib `SessionDeviceTracker.pickBestFormat`)
  picks its service from `host:features`: no `devicetracker_proto_format` feature
  → `LONG_FORMAT` → it sends **`host:track-devices-l`**. There is **no fallback**
  to legacy `host:track-devices` on FAIL, so AS shows an empty device list.
- adboost currently implements the streaming device-list family only as legacy
  `host:track-devices` (short format). One-shot `host:devices` / `host:devices-l`
  both exist.
- Diagnosis was expensive because the three unknown-service FAIL branches emit
  **no log line at all** — the server side was a black hole; xdb had to
  decompile AS to locate the gap.

## 2. Requirements

### R1 — `host:track-devices-l` (P0, core)

Wire behavior (AOSP `SERVICES.TXT`): reply `OKAY`, then a `%04x`-framed
**long-format** device listing (byte-identical to what `host:devices-l` renders)
pushed immediately and on **every device-set change**; the connection stays open
until the client hangs up or the backend change stream closes.

- Long-format lines: `<serial>\t<state>[ key:value …] transport_id:<N>` —
  reusing the existing `format_devices(.., long)` renderer, which adblib's
  `DeviceListTextParser(LONG_FORMAT)` already parses.
- **Not** in scope (explicitly deferred, P1): `host:track-devices-proto-binary`
  / `-proto-text` and the `devicetracker_proto_format` feature flag (they must
  ship together; flag-without-service would re-break AS on the proto path).
- Long-format `product/model/device` fields stay optional/absent (getprop
  backfill is out of scope, same as `devices-l` today).

### R2 — Unknown/unroutable service requests must WARN-log (P0)

Every FAIL path that exists because the frontend **does not know how to route a
service** must emit one `tracing::warn!` line carrying:

- the service string the client sent (as complete as locally available), and
- the requesting peer address.

Covered branches (all client-triggerable unknown-service FAILs):

1. `dispatch_host_service` — request without `host:`/family prefix →
   `unknown service: {service}`
2. `dispatch_host_service` — `host:` service matching no arm →
   `unknown host service: {other}`
3. `dispatch_host_serial` — unknown pinned sub-service →
   `unknown host-serial sub-service: {other}`
4. `serve_local_service` — post-transport service rejected by
   `map_local_service` (unknown local service / framing not supported) →
   `service not supported: {svc}`
5. `serve_reverse` — unknown `reverse:` sub-service →
   `unsupported reverse service: {service}`

Invariants:

- The **wire FAIL wording must stay byte-identical** (AOSP-exact; existing
  parity behavior locked by tests). Logging is additive only.
- One shared helper funnels all five sites so the diagnostic wording cannot
  drift (project rule: every path agrees through one core).
- Log style follows `logging-guidelines.md`: fully-qualified `tracing::warn!`,
  lowercase message, warn level (recoverable/degraded condition).

### R3 — Non-goals

- No `host:features` changes (no `devicetracker_proto_format`).
- No proto serialization.
- No USB takeover TOCTOU fix (separate issue, called out by xdb as independent).
- No change to `track-devices` (short) wire behavior.

## 4. Design decisions

- **`DeviceListFormat` enum** (`Short` / `Long`), private to `frontend.rs`:
  models the rendering axis shared by the one-shot (`devices`/`devices-l`) and
  streaming (`track-devices`/`track-devices-l`) families. `format_devices`
  refactors from `long: bool` to this enum (self-documenting call sites,
  extends naturally to a proto variant in P1). Both families MUST render
  identically — locked by a parity unit test and a real-device case.
- `serve_track_devices(stream, format)` parameterized — the two streaming
  variants share one implementation and cannot drift.
- `track-devices-l` routes as an exact match arm in `dispatch_host_service`
  (streaming services live in the match, NOT in `host_data_query_payload`,
  which is only for one-shot `OKAY`+framed data queries).
- Unknown-service logging via one free helper `warn_unsupported_service(service,
  stream)` + pure `unsupported_service_log_line(service, peer)` (unit-testable
  message shape without a tracing subscriber).

## 5. Tests

### Unit (`frontend.rs` inline `#[cfg(test)]`, `--features server,usb`)

- `track_devices_l_streams_long_format_snapshot` — OKAY + framed payload with
  `transport_id:N`, byte-exact payload lock.
- `track_devices_l_payload_matches_devices_l` — streaming long snapshot ==
  one-shot `devices-l` payload over the same device set.
- `track_devices_streams_short_format_snapshot` — legacy short format
  regression (no `transport_id`).
- `unknown_service_*` / `unknown_host_service_*` / `unknown_host_serial_sub_*`
  — lock the exact AOSP FAIL wording bytes (pre-condition for "logging is
  additive only").
- `unsupported_service_log_line_includes_service_and_peer` — pure log-message
  shape.

### Real-device (`adboost_cli selftest`)

- New raw-protocol module (what Android Studio's adblib actually does —
  complementing `parity.rs`'s official-CLI driver):
  - automated, non-destructive: `track_devices_l` first snapshot is long format,
    contains the serial + `transport_id`, and **byte-equals** the
    `host:devices-l` one-shot payload; legacy `track-devices` snapshot
    byte-equals `host:devices` payload (regression).
  - interactive (operator unplug/replug): a live `track-devices-l` stream pushes
    an updated snapshot when the device vanishes and when it returns —
    the streaming contract AS depends on.

## 6. Acceptance criteria

1. `host:track-devices-l` answers OKAY + framed long listing, matching
   `devices-l` bytes, streaming on change (unit + real-device).
2. All five unknown-service FAIL paths log one warn line with service + peer;
   FAIL replies unchanged.
3. `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`
   (default features), `cargo clippy -p adboost --features server,usb
   --all-targets -- -D warnings`, `cargo test` (incl. `--features server,usb`
   for the frontend tests) all green.
4. Spec doc `.trellis/spec/backend/server-host-protocol.md` updated with the
   new service contract + logging rule.
