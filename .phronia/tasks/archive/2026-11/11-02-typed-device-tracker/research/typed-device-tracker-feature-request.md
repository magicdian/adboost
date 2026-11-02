# Feature Request: typed, negotiated device tracker for adboost

Status: proposed by xdb (`11-02-adb-upstream-tracker-terminal`)
Target repo: `magicdian/adboost`
Target crate: `adboost` (`adboost::proxy` / `adboost::usb` tracker surface)

## 1. Problem

xdb's daemon currently reads the upstream adb server's device tracker in its own
code (`xdb-core/src/adb.rs`). That code assumes `host:track-devices-l` frames are
`serial<TAB>state ...`. Real AOSP adb has always emitted the **long** format as
`%-22s %s` — serial padded to 22 columns with **spaces**, then state. The tab
format comes from adboost's own adb *frontend* renderer, not from AOSP.

Consequence in the field (mtk_8676, adb 37.0.0): every tracker snapshot parsed
into phantom serials such as:

```
"YTGUSCNFMFAIK7ZP       device usb:4-1.2 product:vext_f57aes model:f57aes
 device:f57aes transport_id:164"
```

The real serial was removed from the daemon's device map on every device-state
change; a 30 s fallback poll periodically re-added it. During that window
`xdb shell --target tbox` resolved a phantom row with `platform = None` and
reported `EC_TARGET_INVALID layer 'tbox' is not available (platform unknown)`.
Two consecutive attempts failed; an attempt ~2 minutes later succeeded.

The root cause is not "one bad split". The wire knowledge is duplicated in xdb
(blocking raw reader + hand-written text parser) and adboost (its own
`DeviceLong` parser and frontend renderer), with no capability negotiation and
no shared typed model. Any future adb format change (or another renderer) can
poison xdb's device map again.

## 2. Requested capability

adboost becomes the single owner of the adb device-tracker wire family and
exposes a typed, snapshot-boundary API. xdb consumes typed snapshots and never
parses adb text again.

### 2.1 Public API (additive; do not break existing `track_devices()`)

```rust
pub mod tracker {
    use std::time::Duration;

    /// Server-level feature set from `host:host-features`.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct HostFeatures { /* BTreeSet<String> */ }

    impl HostFeatures {
        pub async fn query(addr: std::net::SocketAddr) -> Result<Self>;
        pub async fn query_default() -> Result<Self>;
        pub fn contains(&self, feature: &str) -> bool;
        /// `devicetracker_proto_format` (platform-tools >= 35.0.2).
        pub fn supports_device_tracker_proto(&self) -> bool;
    }

    /// Validated device serial. Rejects empty strings, any ASCII/Unicode
    /// whitespace, control characters, and strings longer than 255 bytes.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct DeviceSerial(String);

    impl DeviceSerial {
        pub fn new(raw: impl Into<String>) -> Result<Self, TrackerError>;
        pub fn as_str(&self) -> &str;
    }

    /// Mirrors AOSP `ConnectionState` (proto enum values in parentheses).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum ConnectionState {
        Any,          // 0
        Connecting,   // 1
        Authorizing,  // 2
        Unauthorized, // 3
        NoPermission, // 4  ("no permissions" in text)
        Detached,     // 5
        Offline,      // 6
        Bootloader,   // 7
        Device,       // 8
        Host,         // 9
        Recovery,     // 10
        Sideload,     // 11
        Rescue,       // 12
        Unknown,      // proto enum value not known to this build
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TransportKind { Usb, Local, Tcp }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
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

    /// One length-prefixed tracker frame = one COMPLETE snapshot.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct DeviceSnapshot {
        pub devices: Vec<DeviceInfo>,
        /// Rows that failed validation/decode. They are NEVER inserted as
        /// devices. Callers can log/count them.
        pub malformed_rows: usize,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TrackerFormat { ProtoBinary, LongText }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum TrackerHealth { Connected, Disconnected { reason: String } }

    pub struct DeviceTracker { /* connection + negotiated format */ }

    impl DeviceTracker {
        /// Query `host:host-features`, pick ProtoBinary when advertised,
        /// otherwise LongText, open the stream, publish an initial snapshot.
        pub async fn connect(addr: std::net::SocketAddr) -> Result<Self>;
        pub async fn connect_default() -> Result<Self>;

        /// Explicit format (tests / field triage). `None` = auto negotiate.
        pub async fn connect_with_format(
            addr: std::net::SocketAddr,
            format: Option<TrackerFormat>,
        ) -> Result<Self>;

        pub fn format(&self) -> TrackerFormat;

        /// Waits for the next frame. `Ok(None)` = stream closed cleanly.
        pub async fn next_snapshot(&mut self) -> Result<Option<DeviceSnapshot>>;

        /// Event-driven health for a poll fallback: `Disconnected` is published
        /// exactly once per stream failure, before reconnect attempts.
        pub fn health(&self) -> tokio::sync::watch::Receiver<TrackerHealth>;
    }

    #[derive(Debug, thiserror::Error)]
    pub enum TrackerError {
        #[error("unsupported adb service '{service}': {detail}")]
        Unsupported { service: &'static str, detail: String },
        #[error("tracker protocol error: {detail}")]
        Protocol { detail: String },
        #[error("tracker decode error at row {row}: {detail}")]
        Decode { row: usize, detail: String },
        #[error(transparent)]
        Io(#[from] std::io::Error),
    }
}
```

Notes:

- The API must return **one snapshot per frame**, not a callback per device.
  xdb's current raw reader exists precisely because the older `track_devices`
  API has no snapshot boundary.
- `DeviceSerial` is the hard invariant that prevents this incident class. It
  must be used by every producer of device identities in the tracker.
- `next_snapshot()` must be cancel-safe (`tokio::select!` friendly): dropping
  the future must not corrupt the framed stream. Prefer a small buffered reader
  that owns the socket and completes a frame before returning.

### 2.2 Format negotiation

1. Query `host:host-features` (server-level; succeeds on old adb, just without
   the flag). Parse the comma-separated feature list.
2. If `devicetracker_proto_format` is present → use
   `host:track-devices-proto-binary`.
3. Else → use `host:track-devices-l`.
4. If the proto service returns `FAIL` even though the flag was advertised,
   fall back to `host:track-devices-l` once for the lifetime of that tracker and
   surface `TrackerFormat::LongText`. Log the degradation at `warn!`.
5. Never fail the tracker because the feature query failed; fall back to
   `LongText` (old servers) and log at `debug!`.

### 2.3 Wire formats (verified)

`host:track-devices-proto-binary` (adb >= 35.0.2):

- 4-byte lowercase hex length, then one serialized `adb.proto.Devices` message.
- Schema:

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
  int64 transport_id = 10;
}

enum ConnectionState { ANY=0; CONNECTING=1; AUTHORIZING=2; UNAUTHORIZED=3;
  NOPERMISSION=4; DETACHED=5; OFFLINE=6; BOOTLOADER=7; DEVICE=8; HOST=9;
  RECOVERY=10; SIDELOAD=11; RESCUE=12; }
enum ConnectionType { UNKNOWN=0; USB=1; SOCKET=2; }
```

- Decoder requirements: skip unknown fields (forward compatibility), cap
  message size (e.g. 1 MiB), reject a frame whose declared length overflows the
  cap, and treat an unknown enum value as `ConnectionState::Unknown` (do not
  drop the device).
- `transport`: `USB` → `TransportKind::Usb`; `SOCKET` with a `host:port`-shaped
  serial → `Tcp`; otherwise `Local`.

`host:track-devices-l` (all adb versions since at least Android 9):

- 4-byte hex length, then a UTF-8 text body of `\n`-terminated lines.
- AOSP line grammar (`transport.cpp::append_transport`):

```
<serial padded to 22 with spaces> <state>[ <bus_address>][ product:<p>]
[ model:<m>][ device:<d>] transport_id:<n>
```

- The separator between serial and state is **one or more spaces**, never a
  tab. Short `host:track-devices` is the only tab-separated format
  (`serial\tstate`).
- State strings may contain a space: `no permissions`. Treat the longest known
  state prefix as the state, then the remainder as attributes. Unknown state
  text must degrade to `ConnectionState::Unknown`, not fail the row.
- Older adb emits a bare bus address (`1-5`) instead of `usb:1-5`; accept both.
  A bare address means USB.
- `transport_id` is always present on modern adb; treat it as optional.
- Empty body (`0000`) = an authoritative empty snapshot.
- A row that cannot produce a valid `DeviceSerial` must increment
  `malformed_rows` and be skipped. It must never be inserted under a raw line.

### 2.4 Compatibility matrix (evidence)

| adb / platform-tools | long text | proto tracker | source |
|---|---|---|---|
| Android 9 (`android-9.0.0_r61`) | `%-22s %s` | absent | `system/core/adb/transport.cpp` |
| platform-tools 33.0.3 | `%-22s %s` | absent | `packages/modules/adb/transport.cpp` |
| platform-tools 34.0.5 | `%-22s %s` | absent | same |
| platform-tools 35.0.2 | `%-22s %s` | present (`devicetracker_proto_format`) | same |
| platform-tools 37.0.0 (current) | `%-22s %s` | present | live capture below |
| adboost frontend (xdb USB mode) | `serial\tstate ...` | not implemented (by design) | `adboost/src/server/frontend.rs` |

Live capture from platform-tools 37.0.0 (this host, serial padded with spaces):

```
host:track-devices       -> "YTGUSCNFMFAIK7ZP\tdevice\n"
host:track-devices-l     -> "YTGUSCNFMFAIK7ZP       device usb:4-1.2 product:vext_f57aes model:f57aes device:f57aes transport_id:164\n"
host:track-devices-proto-text ->
  device {
    serial: "YTGUSCNFMFAIK7ZP"
    state: DEVICE
    bus_address: "usb:4-1.2"
    product: "vext_f57aes"
    model: "f57aes"
    device: "f57aes"
    connection_type: USB
    negotiated_speed: 480
    max_speed: 5000
    transport_id: 187
  }
```

### 2.5 Test corpus (must ship with the change)

Record raw frames under `adboost/tests/fixtures/adb-tracker/`:

- `android9/long.txt`, `pt33/long.txt`, `pt34/long.txt`, `pt35/long.txt`,
  `pt37/long.txt` (space-padded, with and without bare bus address).
- `adboost-frontend/long.txt` (tab-padded, for the other renderer).
- `pt35/proto.bin`, `pt37/proto.bin`.
- Edge cases: empty snapshot, CRLF, `no permissions`, unauthorized, offline,
  emulator serial, `192.168.1.5:5555`, unknown enum value, unknown proto field,
  truncated frame, over-cap length, serial with a space (must be rejected).

Contract test: for the same logical device set, `LongText` decoding and
`ProtoBinary` decoding must produce byte-for-byte equal `DeviceSnapshot` values
(modulo fields the text format cannot express, which the test pins explicitly).

### 2.6 Acceptance criteria

- [ ] `DeviceTracker::connect` negotiates ProtoBinary on adb >= 35 and LongText
      on adb <= 34, proven by a fake-server integration test.
- [ ] AOSP space-padded long lines decode to the correct serial/state/transport;
      the whole line is never a serial.
- [ ] Tab-padded lines and space-padded lines produce identical snapshots.
- [ ] `no permissions`, `unauthorized`, `offline`, and unknown states are
      represented by `ConnectionState`, never dropped.
- [ ] A malformed row increments `malformed_rows` and is never inserted.
- [ ] `DeviceSerial` rejects whitespace/control/empty/overlong input.
- [ ] One snapshot per frame; empty frames are authoritative empty snapshots.
- [ ] The proto decoder skips unknown fields and maps unknown enum values to
      `Unknown`.
- [ ] Stream failure publishes `TrackerHealth::Disconnected` exactly once, then
      reconnects with bounded backoff.
- [ ] No public API breakage; existing `track_devices()` stays unchanged.
- [ ] `cargo test -p adboost` and `cargo clippy -p adboost -- -D warnings` pass.

## 3. Integration ask

Once this lands:

1. Publish a new adboost revision/tag.
2. xdb bumps its git `rev` in `sources/host/crates/xdb-core/Cargo.toml`.
3. xdb deletes `parse_track_devices_long` and
   `track_devices_long_protocol` from `xdb-core/src/adb.rs` and consumes
   `adboost::tracker::DeviceTracker`.

xdb-side consumers and the exact migration are described in its task
`11-02-adb-upstream-tracker-terminal` (`design.md`, `implement.md`).
