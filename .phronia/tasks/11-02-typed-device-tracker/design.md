# Design: Typed, negotiated device tracker

## 1. Problem shape

The tracker has four separable concerns:

1. **Host-service wire**: connect to the adb server, send a service request,
   read `OKAY`/`FAIL`, and read 4-hex-length frames. This already exists in
   `proxy/tcp_proxy_transport.rs` but is not reusable and its reads are not
   cancel-safe.
2. **Wire codecs**: AOSP long text and `adb.proto.Devices` binary. These are
   pure functions over a frame body and must be testable without I/O.
3. **Negotiation**: `host:host-features` → proto or long; one-way fallback on
   proto `FAIL`.
4. **Stream lifecycle**: one connection, snapshot boundary, cancel-safe
   framing, bounded reconnect, health transitions.

These are separate layers; mixing them into one file would make the parser
tests require sockets and the socket tests require protobuf fixtures.

## 2. Layering

```
models/                     existing shared wire types (ADBCommand, HostFeatures, ...)
        │
proxy/host_service.rs       crate-internal smart-socket seam
        │                   encode request (4-hex len + service)
        │                   read OKAY/FAIL (+ framed reason)
        │                   FrameDecoder (cancel-safe 4-hex length frames)
        ▼
tracker/                    public client API (always compiled)
  ├── models/               DeviceSerial, ConnectionState, TransportKind,
  │                         DeviceInfo, DeviceSnapshot, ServerFeatures,
  │                         TrackerFormat, TrackerHealth, TrackerConfig,
  │                         ReconnectPolicy
  ├── error.rs              TrackerError
  ├── wire/                 pure codecs + format selection
  │   ├── long_text.rs      AOSP/space + adboost/tab + bare bus
  │   ├── proto.rs          adb.proto.Devices/Device (quick-protobuf)
  │   └── mod.rs            decode(format, body)
  ├── commands/             impl blocks: connect / next_snapshot / features
  └── device_tracker.rs     DeviceTracker struct + private connection state
```

`TCPProxyTransport` also uses `proxy/host_service.rs` for request encoding and
status parsing, so the smartsocket framing has one owner. The tracker uses the
same seam but owns its `TcpStream` so the stream can be held open and reconnected.

### Dependency rules

- `tracker` → `proxy::host_service` (wire primitives only). `tracker` never
  depends on `ADBProxyServer`, `usb`, or `server`.
- `proxy` does not depend on `tracker`. The legacy `DeviceLong`/`DeviceShort`
  models call the tracker's long-text parser through a `pub(crate)` seam; that
  is the only reverse edge and it is confined to `proxy/models/`.
- The server frontend is untouched: it must not advertise
  `devicetracker_proto_format` until it implements both proto services.

## 3. Public API

```rust
pub const DEFAULT_SERVER_ADDR: SocketAddr; // 127.0.0.1:5037

pub struct ServerFeatures(BTreeSet<String>);
impl ServerFeatures {
    pub async fn query(addr: SocketAddr) -> Result<Self, TrackerError>;
    pub async fn query_default() -> Result<Self, TrackerError>;
    pub fn contains(&self, feature: &str) -> bool;
    pub fn iter(&self) -> impl Iterator<Item = &str>;
    pub fn supports_device_tracker_proto(&self) -> bool;
}

pub struct DeviceSerial(String);
impl DeviceSerial {
    pub fn new(raw: impl Into<String>) -> Result<Self, TrackerError>;
    pub fn as_str(&self) -> &str;
}
// Display + AsRef<str>

pub enum ConnectionState { Any, Connecting, Authorizing, Unauthorized,
    NoPermission, Detached, Offline, Bootloader, Device, Host, Recovery,
    Sideload, Rescue, Unknown }

pub enum TransportKind { Usb, Local, Tcp }

pub struct DeviceInfo {
    pub serial: DeviceSerial,
    pub state: ConnectionState,
    pub transport: TransportKind,
    pub transport_id: Option<u32>,
    pub bus_address: Option<String>,
    pub product: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
    pub negotiated_speed: Option<u32>,
    pub max_speed: Option<u32>,
}

pub struct DeviceSnapshot {
    pub devices: Vec<DeviceInfo>,
    pub malformed_rows: usize,
}

pub enum TrackerFormat { ProtoBinary, LongText }
pub enum TrackerHealth { Connected, Disconnected { reason: String } }

pub struct ReconnectPolicy {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub max_attempts: Option<u32>, // None = retry forever
}

pub struct TrackerConfig {
    pub addr: SocketAddr,
    pub format: Option<TrackerFormat>, // None = negotiate
    pub reconnect: Option<ReconnectPolicy>, // None = caller-managed
}

pub struct DeviceTracker { /* private */ }
impl DeviceTracker {
    pub async fn connect(addr: SocketAddr) -> Result<Self, TrackerError>;
    pub async fn connect_default() -> Result<Self, TrackerError>;
    pub async fn connect_with_format(
        addr: SocketAddr,
        format: Option<TrackerFormat>,
    ) -> Result<Self, TrackerError>;
    pub async fn connect_with_config(config: TrackerConfig) -> Result<Self, TrackerError>;
    pub fn addr(&self) -> SocketAddr;
    pub fn format(&self) -> TrackerFormat;
    pub async fn next_snapshot(&mut self) -> Result<Option<DeviceSnapshot>, TrackerError>;
    pub fn health(&self) -> watch::Receiver<TrackerHealth>;
}

#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    #[error("unsupported adb service '{service}': {detail}")]
    Unsupported { service: &'static str, detail: String },
    #[error("tracker protocol error: {detail}")]
    Protocol { detail: String },
    #[error("tracker decode error at row {row}: {detail}")]
    Decode { row: usize, detail: String },
    #[error("invalid device serial {serial:?}: {reason}")]
    InvalidSerial { serial: String, reason: &'static str },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
```

No root re-export: callers use `adboost::tracker::*`. This deliberately avoids
colliding with the existing `adboost::HostFeatures` (per-transport
`host:features`).

## 4. Host-service seam (`proxy/host_service.rs`)

Crate-internal types:

```rust
pub(crate) enum HostStatus { Okay, Fail(String) }

pub(crate) enum HostServiceError {
    Io(io::Error),
    Protocol(String),
}

pub(crate) async fn send_request<W: AsyncWrite + Unpin>(
    writer: &mut W, service: &str,
) -> Result<(), HostServiceError>;

pub(crate) async fn read_status<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<HostStatus, HostServiceError>;

pub(crate) struct FrameDecoder { /* len[4], len_filled, body, body_filled */ }
impl FrameDecoder {
    pub(crate) fn new() -> Self;
    pub(crate) async fn next_frame<R: AsyncRead + Unpin>(
        &mut self, reader: &mut R,
    ) -> Result<Option<Vec<u8>>, HostServiceError>;
}
```

- `send_request` validates `service.len() <= 0xFFFF`, then writes
  `format!("{:04x}{service}")`.
- `read_status` reads 4 bytes, maps `OKAY`/`FAIL`; a `FAIL` reason is one
  framed string. Unknown statuses are `HostServiceError::Protocol`.
- `FrameDecoder` keeps read progress in its own fields, so dropping the
  `next_frame` future mid-frame cannot lose bytes. Each `read()` is
  cancel-safe. A 4-hex length is at most 0xFFFF, so no larger cap is needed.
- `0x0000` returns `Some(vec![])`: an authoritative empty snapshot.
- EOF at a frame boundary → `Ok(None)`; EOF after a partial length/body →
  `HostServiceError::Protocol` (truncated frame).
- `TCPProxyTransport::send_adb_request` / `read_adb_response` delegate to the
  same seam; behavior and error text stay compatible
  (`HostServiceError` → `RustADBError`).

## 5. Codecs

### 5.1 Long text (`wire/long_text.rs`)

Pure function over one frame body:

```
decode(body: &[u8]) -> DeviceSnapshot
```

Algorithm per `\n`-terminated row (a trailing `\r` is stripped):

1. Skip empty rows.
2. Decode the row as UTF-8; on error, `malformed_rows += 1`.
3. Split the serial from the remainder at the first whitespace run (space or
   tab). AOSP pads with spaces; adboost's frontend uses a tab.
4. Validate the serial with `DeviceSerial::new`; on error, `malformed_rows += 1`.
5. Detect the state as the **longest known prefix** of the remainder. Known
   prefixes are sorted by length (so `no permissions` wins over `no`), and
   `no permissions (user in plugdev group; ...)` maps to `NoPermission`.
   Unknown text → `Unknown`.
6. Parse attributes from the remainder after the state:
   - `usb:<bus>` or a bare first token matching `^\d+-\d+(\.\d+)*$`;
   - `product:`, `model:`, `device:`, `transport_id:`.
   Attribute values run to the next known attribute marker, so spaces inside
   values (e.g. `model:M425 PLUS A`) are preserved.
7. Derive `TransportKind`: USB if a bus address was present; otherwise TCP if
   the serial is `host:port`-shaped; otherwise Local.
8. Normalize `bus_address` by stripping an optional `usb:` prefix.

`transport_id` is optional. A non-numeric `transport_id` marks the row
malformed rather than silently dropping the device.

Legacy `DeviceLong`/`DeviceShort` reuse this parser and convert
`ConnectionState` → `DeviceState`; this removes the two regex parsers and
guarantees the tracker and the legacy API agree on the wire grammar.

### 5.2 Protobuf (`wire/proto.rs`)

Manual `MessageRead` impls (same style as `proxy/models/server_status.rs`):

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
  int64 transport_id = 10;            // added after the initial proto
}
```

- Unknown fields are skipped with `read_unknown`.
- Unknown enum values → `ConnectionState::Unknown`.
- Empty strings → `None`.
- Negative/overflowing int64 → `None` for `u32` fields.
- `transport` = USB for `USB`; for `SOCKET`, `Tcp` when the serial is
  `host:port`-shaped, else `Local`; for `UNKNOWN`, infer from `bus_address`
  then serial.
- `bus_address` is normalized like the text decoder.
- An invalid serial increments `malformed_rows` and skips the device.
- A whole-message parse failure is `TrackerError::Decode`; the frame boundary
  is still intact, so the connection remains usable.

## 6. Negotiation and lifecycle

### 6.1 Open sequence

```
TcpStream::connect(addr)
set_nodelay(true)
send_request("host:host-features")
read_status()
  OKAY -> FrameDecoder::next_frame -> CSV -> ServerFeatures
  FAIL/IO/protocol -> log debug, use LongText
pick:
  explicit config.format                    -> that format
  features.supports_device_tracker_proto()  -> ProtoBinary
  otherwise                                 -> LongText
send_request(format.service())
read_status()
  OKAY -> connected
  FAIL for ProtoBinary in auto mode -> warn, set proto_downgraded, retry LongText once
  FAIL otherwise -> TrackerError::Unsupported
```

The initial connect does not retry; a failure returns to the caller. The
reconnect policy applies to failures after a successful initial connection.

### 6.2 `next_snapshot`

```
if no connection:
    ensure_connected()          // bounded backoff; policy None -> one attempt
read one frame:
    Some(body) -> decode(format, body)
                  reset backoff attempts
                  publish Connected (transition only)
                  return Ok(Some(snapshot))
    None       -> drop connection; publish Disconnected
                  reset backoff attempts
                  return Ok(None)          // clean close is caller-visible
    IO/truncated -> drop connection; publish Disconnected
                  policy Some -> loop (ensure_connected backs off)
                  policy None -> return Err
    payload Decode error -> return Err; keep the connection (frame boundary intact)
```

`Disconnected` is published only on a `Connected → Disconnected` transition;
repeated failed reconnect attempts do not spam the watch channel. `Connected`
is published when a new connection is established. `watch::Sender::send_replace`
keeps the latest value even with no receivers.

### 6.3 Reconnect policy

- Default: `initial_backoff = 250 ms`, `max_backoff = 5 s`,
  `max_attempts = None` (retry forever).
- Delay for attempt `n` (0-based): `min(initial * 2^n, max)` with saturating
  arithmetic; no jitter (deterministic tests).
- `max_attempts = Some(0)` = no retries.
- Attempts reset after a successful snapshot.
- Reconnect re-runs the negotiation unless `config.format` is explicit or
  `proto_downgraded` is set.

## 7. Decisions and rejected alternatives

| Decision | Alternative | Why |
|---|---|---|
| `ServerFeatures`, not `HostFeatures` | Reuse the FR name | `adboost::HostFeatures` is the per-transport set; the naming trap is documented in `server-host-protocol.md`. |
| Tracker-local `TrackerError` | Add variants to `RustADBError` | Keeps the 45-variant crate error and the CLI exhaustive match stable; the tracker error is cohesive. |
| `TrackerConfig` + `ReconnectPolicy` | Hard-code reconnect in `next_snapshot` | Policy becomes explicit and testable; callers can disable it. |
| Shared long-text parser reused by `DeviceLong`/`DeviceShort` | Keep the regexes | Avoids a third parser; existing tests gate compatibility. |
| `proxy/host_service` seam | Copy the smartsocket framing into tracker | One owner for request/status/frame framing; `TCPProxyTransport` benefits too. |
| Client-only proto services | Implement server proto + advertise the flag now | The server frontend's proto services are a separate change; advertising without them breaks adb clients. |
| Clean EOF returns `Ok(None)` even with reconnect | Auto-reconnect on clean close | Preserves the FR's explicit "stream closed" signal; the caller can call again to reconnect. |
| `bus_address` without `usb:` | Preserve the raw `usb:` prefix | Makes text/proto snapshots comparable; `TransportKind` already carries the scheme information. |
| 0xFFFF frame cap | 1 MiB cap | The 4-hex prefix cannot express more; enforcing it at the framing layer is the real contract. |

## 8. Compatibility / rollout / rollback

- Additive public module; existing types and signatures unchanged.
- No new dependencies/features/MSRV change. `quick-protobuf`, `tokio`,
  `thiserror`, and `tracing` are already normal dependencies.
- Downstream: xdb bumps the pinned rev, replaces
  `track_devices_long_protocol` + `parse_track_devices_long` with
  `adboost::tracker::DeviceTracker`, and keeps its fallback poll on
  `TrackerHealth::Disconnected`.
- Rollback: delete `adboost/src/tracker/`, revert the `proxy/host_service`
  refactor and the `DeviceLong`/`DeviceShort` delegation, and remove the spec
  entries. No data migration.

## 9. Risks

1. **Legacy parser behavior drift.** `DeviceLong`/`DeviceShort` now delegate to
   the shared parser. Their existing success tests are the regression gate;
   error-kind changes for unknown states are documented (the tracker maps
   unknown states to `Unknown`; legacy conversion returns
   `UnknownDeviceState`).
2. **Reconnect hides errors.** With the default policy, persistent I/O failures
   are retried forever and surfaced only through `health()`. This is the
   requested long-lived behavior; `ReconnectPolicy::max_attempts` and
   `reconnect: None` are the escape hatches.
3. **Proto schema drift.** Unknown fields/enums are tolerated. A future field
   that changes semantics (not just adds data) needs a spec update.
4. **Cancel-safety depends on `FrameDecoder` owning all progress.** Any future
   refactor that moves partial-frame state into the future re-introduces the
   bug; the cancel-safety test locks it.
5. **Test flakiness.** Fake-server tests bind `127.0.0.1:0` and use timeouts;
   no hardware or key generation is involved.
