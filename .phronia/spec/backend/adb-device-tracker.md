# ADB Device Tracker — Client Contract

> Client-side contract for `adboost::tracker` (always compiled, no `usb` /
> `server` feature dependency). The server frontend's device-list renderer is
> covered by [`server-host-protocol.md`](./server-host-protocol.md); the tracker
> is its client-side counterpart.

---

## Why the tracker exists

The upstream adb server emits one **length-prefixed frame per device-set
change**. Downstreams that read those frames themselves duplicate wire
knowledge and eventually parse a rendered line as a serial. The field incident
that created this module: AOSP's long format is `%-22s %s` (serial padded with
**spaces**), while a downstream parser assumed a tab. Every row became a
phantom serial; the real device disappeared from the map on each state change.

The tracker owns the wire family so downstreams consume typed snapshots:

- `host:host-features` negotiation;
- `host:track-devices-proto-binary` when the server advertises
  `devicetracker_proto_format`;
- `host:track-devices-l` otherwise (AOSP space-padded or adboost tab-padded);
- one frame = one complete `DeviceSnapshot`;
- every device identity is a validated `DeviceSerial`.

The legacy `ADBProxyServer::track_devices` callback API is unchanged.

---

## Layering

```
proxy/host_service.rs        crate-internal smart-socket seam
  send_request               4-hex length + service name
  read_status                OKAY / FAIL(+framed reason)
  FrameDecoder               cancel-safe 4-hex-length frame reader
        │
tracker/                      public client
  models/                    DeviceSerial, ConnectionState, TransportKind,
                             DeviceInfo, DeviceSnapshot, ServerFeatures,
                             TrackerFormat, TrackerHealth, TrackerConfig,
                             ReconnectPolicy
  error.rs                   TrackerError (not RustADBError)
  wire/long_text.rs          AOSP/adboost long-text decoder
  wire/proto.rs              adb.proto.Devices decoder
  commands/                  connect / next_snapshot / features impls
  device_tracker.rs          DeviceTracker + per-connection state
```

Rules:

- `tracker` may use `proxy::host_service`; `proxy` must not depend on `tracker`
  except the narrow legacy-parser delegation in `proxy/models/`.
- `TCPProxyTransport` and `tracker` share request/status/frame framing. Do not
  copy `%04x` handling into a new client.
- The tracker never depends on `usb` or `server`; those features may remain off.

---

## Wire services and framing

| Format | Host service | Frame body |
|---|---|---|
| `TrackerFormat::ProtoBinary` | `host:track-devices-proto-binary` | `adb.proto.Devices` |
| `TrackerFormat::LongText` | `host:track-devices-l` | UTF-8 text, `\n`-terminated rows |

All tracker services reply `OKAY` and then one or more `%04x`-length-prefixed
frames. `0000` is an authoritative empty snapshot. A clean EOF at a frame
boundary is `Ok(None)`; EOF inside a frame is a protocol error.

`host:host-features` is the **server-level** query. It is not the per-device
`host:features` used for shell v1/v2 gating (`crate::HostFeatures`). The two
are not interchangeable; `ServerFeatures` is deliberately named to prevent the
confusion.

---

## Long-text grammar

AOSP `transport.cpp::append_transport`:

```
<serial padded to 22 with spaces> <state>[ <bus_address>][ product:<p>]
[ model:<m>][ device:<d>] transport_id:<n>
```

- The serial/state separator is one or more **spaces** (AOSP) or a **tab**
  (adboost's frontend). Never assume a tab.
- The state is the longest known prefix. AOSP renders
  `no permissions (user in plugdev group; ...)`; the parenthetical help text is
  not part of the state and is ignored.
- The bus address may be `usb:4-1.2` (modern) or bare `1-5` (older adb);
  `DeviceInfo::bus_address` stores it **without** the `usb:` scheme so the text
  and proto decoders agree.
- `product:`, `model:`, `device:`, and `transport_id:` values run to the next
  known attribute marker, so spaces inside values survive. Unknown attributes
  are ignored.
- `TransportKind`: USB when a bus address is present; TCP when the serial is
  `host:port`-shaped; otherwise Local.
- A row that fails UTF-8, serial validation, or `transport_id` parsing
  increments `DeviceSnapshot::malformed_rows` and is skipped. It is never
  inserted under a raw line. AOSP's `(no serial number)` placeholder is
  rejected explicitly (the first token, `(no`, would otherwise look valid).

`DeviceLong` / `DeviceShort` delegate to this parser; the tracker is not a
third grammar.

---

## Protobuf schema

`adb.proto.Devices` (from AOSP `proto/adb_host.proto`):

```proto
message Devices { repeated Device device = 1; }
message Device {
  string serial = 1;
  ConnectionState state = 2;
  string bus_address = 3;
  string product = 4;
  string model = 5;
  string device = 6;
  ConnectionType connection_type = 7; // UNKNOWN=0, USB=1, SOCKET=2
  int64 negotiated_speed = 8;
  int64 max_speed = 9;
  int64 transport_id = 10;            // added after the first proto release
}
```

- Unknown fields are skipped (`read_unknown`); unknown enum values map to
  `ConnectionState::Unknown` / an inferred `TransportKind`, never a dropped
  device.
- Empty strings become `None`; negative/overflowing int64 values become `None`
  for unsigned fields.
- The frame decoder caps a body at `0xFFFF` bytes (four hex digits). A
  whole-message decode failure is `TrackerError::Decode`; the frame boundary is
  intact, so the connection remains usable.

---

## Negotiation and fallback

1. Query `host:host-features` on its own connection.
2. If the reply contains `devicetracker_proto_format`, open
   `host:track-devices-proto-binary`; otherwise open `host:track-devices-l`.
3. If the proto service returns `FAIL` despite the flag, fall back **once** to
   long text for that tracker's lifetime and log at `warn`.
4. If the features query fails, use long text and log at `debug`. Never fail
   the tracker solely because features could not be queried.
5. An explicit `TrackerFormat` (via `connect_with_format` /
   `TrackerConfig::format`) bypasses negotiation and does not fall back.

---

## Snapshot boundary and cancellation

`DeviceTracker::next_snapshot` returns `Ok(Some(snapshot))` per frame,
`Ok(None)` on a clean close, or an error. Progress for a partial frame lives in
`FrameDecoder` inside the tracker, not in the future, so dropping
`next_snapshot` mid-frame loses no bytes. All reads are
`AsyncReadExt::read` calls, which are cancel-safe.

Payload decode failures (`TrackerError::Decode`) keep the connection: the frame
boundary is intact. IO/truncated-frame failures drop the connection and enter
the reconnect policy.

---

## Reconnect and health

`TrackerConfig` defaults to `127.0.0.1:5037`, auto-negotiation, and a
`ReconnectPolicy` of 250 ms initial backoff, 5 s cap, unlimited attempts.
`ReconnectPolicy::max_attempts` bounds an episode; `reconnect: None` returns
stream failures to the caller.

- `TrackerHealth::Disconnected` is published **once per failure episode**, not
  once per failed reconnect attempt; `Connected` is published on recovery.
- A clean close returns `Ok(None)` and publishes `Disconnected`. A later
  `next_snapshot()` call reconnects (caller-driven).
- Reconnect re-negotiates unless the format is explicit or the proto path was
  already downgraded.
- The initial `connect` is a single attempt; the policy applies after a
  successful initial connection.

---

## Tests required

- Serial validation (empty/whitespace/control/overlong).
- AOSP space-padded and adboost tab-padded rows decode identically; the whole
  line is never a serial.
- `no permissions` with help text, unknown states, bare/`usb:` bus addresses,
  TCP/emulator serials, CRLF, empty body.
- Protobuf: live-capture shape, unknown fields/enums, empty strings, negative
  ints, invalid serial, truncated message, field 10.
- Text/proto contract equality (speeds pinned to `None` for text).
- Fake-server negotiation: long, proto, proto→long fallback, explicit no
  fallback, server-level features service.
- Clean EOF, truncated-frame reconnect, reconnect disabled, edge-triggered
  health, cancel-safety mid-frame.
