# PRD: Typed, negotiated device tracker

- **Source**: downstream feature request (`xpeng-debug-bridge`, task
  `11-02-adb-upstream-tracker-terminal`) at
  `/private/tmp/adboost-typed-device-tracker-feature-request.md`
- **Related review**: `/private/tmp/adboost-framed-stream-neutral-design.md` and
  `/private/tmp/adboost-streamtransport-feature-request.md` (already landed in
  `d7e33e1`, task `10-30-stream-transport-framed-stream-core`)
- **Kind**: additive client API + shared host-service wire primitives
- **Priority**: P2
- **Scope**: `adboost` crate — new `tracker` module, a small `proxy/host_service`
  seam, and the legacy `DeviceLong`/`DeviceShort` parsers

---

## 1. Problem

`xdb` reads the upstream adb server's `host:track-devices-l` stream with a
hand-written parser in its own tree. That parser assumed tab-separated rows;
AOSP has always rendered the long format as `%-22s %s` (serial padded with
**spaces**, then state). On a real adb 37.0.0 server every row was therefore
inserted into xdb's device map as a phantom serial (the whole line). A device
state change removed the real serial; a 30 s fallback poll re-added it, so
target resolution failed for a window.

The root cause is ownership: the ADB device-tracker wire knowledge is
duplicated in xdb and adboost, with no shared typed model and no negotiated
format. The tracker must move into adboost so downstreams never parse adb text
again.

## 2. Evaluation of the downstream request

The request is **reasonable and worth implementing**. Its core invariants are
correct and match the incident:

- one length-prefixed frame = one complete snapshot (not a callback per device);
- a validated `DeviceSerial` is the hard invariant (whitespace/control/empty/
  overlong rows are never inserted);
- AOSP long text and the protobuf tracker are both owned by adboost;
- proto/format negotiation uses the server-level `host:host-features`, not the
  per-transport `host:features`;
- unknown enum values/fields and malformed rows degrade without desyncing the
  stream;
- the existing `track_devices()` API stays unchanged.

The request needs the following corrections before it is a good long-term API:

1. **Rename `HostFeatures`.** `adboost::HostFeatures` already means the
   **per-transport** `host:features` set (`shell_v2`, `cmd`). The tracker's set
   is the **server-level** `host:host-features` set. The new type is therefore
   `ServerFeatures`; the two queries must not be conflated.
2. **Do not duplicate the state enum blindly.** `ConnectionState` remains the
   tracker's canonical, proto-complete type, but it converts to the legacy
   `proxy::DeviceState` so the existing `DeviceLong`/`DeviceShort` parsers can
   delegate to the new long-text parser instead of becoming a third parser.
3. **`DeviceInfo` cannot derive `Default`.** `DeviceSerial` is a validated
   non-default type. Only `DeviceSnapshot` is default-constructible.
4. **Keep the error taxonomy local.** A `TrackerError` (`Unsupported`,
   `Protocol`, `Decode`, `InvalidSerial`, `Io`) is cohesive and avoids
   expanding the ~45-variant `RustADBError` and the CLI's exhaustive match.
5. **Make reconnect an explicit policy.** A long-lived tracker benefits from
   reconnect, but hidden retry behavior is a library-wide liability. The policy
   lives in `TrackerConfig`/`ReconnectPolicy` with a documented default, and
   `health()` reports one `Disconnected` per failure episode.
6. **The wire cap is 0xFFFF, not 1 MiB.** The host protocol length prefix is
   four hex digits; the decoder enforces that at the framing layer. The proto
   decoder still validates its own frame size defensively.
7. **`bus_address` is normalized** to the transport address without the `usb:`
   scheme (text `usb:1-5` and bare `1-5`, proto `usb:1-5` all become `1-5`) so
   the text/proto contract test can compare snapshots.
8. **Server-side proto services stay out of scope.** The adboost server
   frontend must not advertise `devicetracker_proto_format` until it implements
   both proto services; the client tracker works against real adb (proto or
   long) and against adboost's long-format frontend.

## 3. Requirements

### R1 — Always-compiled tracker module

- New public `adboost::tracker` module, always compiled (no `usb`/`server`
  feature dependency).
- No new crate dependencies, no new cargo features, no MSRV change.
- Public types are namespaced under `adboost::tracker`; no root re-export that
  collides with the existing `HostFeatures`.

### R2 — Typed model and hard serial invariant

- `DeviceSerial`: rejects empty, any Unicode whitespace, control characters,
  and >255-byte input. Used by every tracker producer.
- `ConnectionState`: AOSP `ConnectionState` values (including `Any` and an
  `Unknown` fallback for unknown proto values).
- `TransportKind`: `Usb` / `Local` / `Tcp`, inferred from the proto connection
  type or the text bus/serial shape.
- `DeviceInfo`: serial, state, transport, optional transport id, bus address,
  product, model, device, negotiated/max speed.
- `DeviceSnapshot`: one frame = one complete snapshot; `malformed_rows` counts
  rows that were skipped and never inserted.

### R3 — Snapshot-boundary, cancel-safe stream

- `next_snapshot()` returns `Ok(Some(snapshot))`, `Ok(None)` on a clean close,
  or an error.
- Dropping the future mid-frame must not lose buffered bytes; the length/body
  progress lives in the tracker connection state, not in the future.
- One frame is never split into multiple snapshots.

### R4 — Negotiation

- Query `host:host-features` (server-level). If the response contains
  `devicetracker_proto_format`, open `host:track-devices-proto-binary`;
  otherwise open `host:track-devices-l`.
- If the proto service returns FAIL despite the advertised flag, fall back once
  to `host:track-devices-l` for the lifetime of the tracker and log a warning.
- If the features query fails, fall back to long text and log at debug; never
  fail the tracker solely because features could not be queried.
- An explicit `TrackerFormat` bypasses negotiation and does not silently fall
  back.

### R5 — Long-text decoder

- Parse AOSP space-padded long lines (`%-22s %s`), adboost frontend tab-padded
  lines, and legacy bare bus addresses (`1-5` vs `usb:1-5`).
- Accept one or more spaces or a tab between serial and state.
- Treat the longest known state prefix as the state, so
  `no permissions (user in plugdev group; ...)` maps to `NoPermission` rather
  than failing the row.
- Unknown state text degrades to `ConnectionState::Unknown`.
- Parse `usb:`/bare bus, `product:`, `model:`, `device:`, and `transport_id:`
  attributes, allowing spaces inside values and ignoring unknown attributes.
- Empty body (`0000`) is an authoritative empty snapshot.
- Invalid UTF-8 or invalid serial increments `malformed_rows` and skips the
  row; a row is never inserted under a raw line.

### R6 — Proto decoder

- Decode `adb.proto.Devices` / `Device` field numbers 1–10 with
  `quick-protobuf` (already a dependency).
- Skip unknown fields; unknown enum values map to `Unknown`, not a dropped
  device.
- Validate each serial; invalid serials increment `malformed_rows`.
- Normalize empty strings to `None`; clamp negative/overflowing int64 fields to
  `None` for unsigned fields.
- A whole-message decode failure returns `TrackerError::Decode`; it does not
  desync the frame stream.

### R7 — Reconnect and health

- `TrackerConfig { addr, format, reconnect }`; default address
  `127.0.0.1:5037`.
- `ReconnectPolicy { initial_backoff, max_backoff, max_attempts }` with bounded
  exponential backoff; the default retries indefinitely with a capped delay.
- `health()` returns a `watch::Receiver<TrackerHealth>`; `Disconnected` is
  published exactly once per failure episode, `Connected` on recovery.
- A clean close returns `Ok(None)` and publishes `Disconnected`. Calling
  `next_snapshot()` again reconnects (subject to the policy).
- With reconnect disabled, an I/O or framing failure is returned to the caller.
- Reconnect re-negotiates unless an explicit format was requested or the proto
  path was already downgraded.

### R8 — Compatibility

- Existing `ADBProxyServer::track_devices` / `devices_long` / `DeviceLong` /
  `DeviceShort` signatures and behavior stay compatible; the legacy parsers
  delegate to the shared long-text parser and keep their existing tests green.
- No new error variant in `RustADBError`.
- No wire-protocol change for adboost's server frontend.

### R9 — Tests and docs

- Unit tests for serial validation, state/text parsing, attribute parsing,
  proto decode (unknown fields/enums, invalid serial, empty snapshot), framing
  cancel-safety.
- Fake-server integration tests for long-text negotiation, proto negotiation,
  proto→long fallback, snapshot boundaries, malformed rows, clean EOF, and
  reconnect.
- Text/proto contract test: the same logical device set produces equal
  snapshots modulo text-unexpressible fields (speeds pinned to `None` for text).
- Spec update: new `adb-device-tracker.md`; directory-structure module map;
  server-host-protocol cross-reference.

## 4. Non-goals

- No `host:track-devices-proto-text` decoder (binary only).
- No server-side proto tracker services or `devicetracker_proto_format`
  advertising.
- No automatic adb server start, no TLS, no mDNS integration.
- No removal or deprecation of `DeviceLong` / `DeviceShort`.
- No reconnect policy inside the legacy `ADBProxyServer::track_devices`
  callback API.

## 5. Acceptance criteria

- [x] `adboost::tracker::{ServerFeatures, DeviceSerial, ConnectionState,
      TransportKind, DeviceInfo, DeviceSnapshot, TrackerFormat, TrackerHealth,
      ReconnectPolicy, TrackerConfig, DeviceTracker, TrackerError}` compile and
      are documented.
- [x] `ServerFeatures::query` sends `host:host-features` (not `host:features`).
- [x] `DeviceTracker::connect` negotiates binary proto on a server advertising
      `devicetracker_proto_format`, and long text otherwise.
- [x] AOSP space-padded and adboost tab-padded long lines decode identically;
      the whole line is never a serial.
- [x] `no permissions`, `unauthorized`, `offline`, `unknown` states are
      represented, never dropped.
- [x] A malformed row increments `malformed_rows` and is not inserted.
- [x] `DeviceSerial` rejects whitespace/control/empty/overlong input.
- [x] One snapshot per frame; empty frames are authoritative empty snapshots.
- [x] Proto decoder skips unknown fields and maps unknown enum values to
      `Unknown`.
- [x] Dropping `next_snapshot()` mid-frame does not lose bytes.
- [x] A stream failure publishes `Disconnected` exactly once and reconnects
      with bounded backoff (or returns the error when reconnect is disabled).
- [x] Existing `track_devices()` and `DeviceLong`/`DeviceShort` tests stay
      green.
- [x] `cargo fmt --all --check`, `cargo clippy --all-targets -D warnings`,
      `cargo test -p adboost`, and `cargo test -p adboost --all-features` pass
      (repository-wide pre-existing clippy findings in untouched files are
      recorded, not fixed).
