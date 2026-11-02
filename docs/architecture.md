# Architecture

> Narrative overview. The executable contracts live under
> [`.phronia/spec/`](../.phronia/spec/); this file points at the load-bearing
> ones and shows how the modules fit together.

## Workspace

| Crate | Role |
|---|---|
| `adboost` | Core async ADB library (client + server frontend). |
| `adboost_cli` | CLI over the local library. |
| `pyadb_client` / `examples/mdns` | Excluded from the active workspace build. |

## ADB client layers

```
proxy/          client of an EXTERNAL adb server (:5037)
  host_service    shared %04x request / OKAY-FAIL / frame seam
tracker/        typed device tracker (host:track-devices*)
  wire/           long-text + protobuf codecs
message_devices/ direct device protocol (USB / TCP / stream)
server/         adboost acting AS an adb server (feature = "server")
```

- `proxy` and `tracker` are clients of an external adb server. `server` is the
  opposite direction (adboost serving native `adb`/`scrcpy` clients).
- `message_devices` is the direct-to-device protocol stack and is independent
  of the host-service clients.

## Device tracker

`adboost::tracker` is always compiled and owns the client side of the adb
device-tracker wire family:

- `host:host-features` selects between `host:track-devices-proto-binary` and
  `host:track-devices-l`.
- One `%04x`-length-prefixed frame is one `DeviceSnapshot`; a zero-length frame
  is an authoritative empty snapshot.
- `DeviceSerial` is the identity invariant; invalid rows increment
  `malformed_rows` and are never inserted.
- `wire/long_text.rs` parses AOSP's space-padded long rows and adboost's
  tab-padded rows. `wire/proto.rs` decodes `adb.proto.Devices` with unknown
  fields/enums tolerated.
- `FrameDecoder` in `proxy/host_service.rs` owns partial-frame progress, so
  `DeviceTracker::next_snapshot` is cancel-safe.
- Reconnect is an explicit `TrackerConfig`/`ReconnectPolicy` concern;
  `TrackerHealth` reports one `Disconnected` per failure episode.

The detailed client contract is
[`.phronia/spec/backend/adb-device-tracker.md`](../.phronia/spec/backend/adb-device-tracker.md).
The server frontend's device-list renderer and its proto-flag honesty rule are
in
[`.phronia/spec/backend/server-host-protocol.md`](../.phronia/spec/backend/server-host-protocol.md).

## Related specs

- [ADB wire protocol contract](../.phronia/spec/backend/adb-wire-protocol-contract.md)
- [Directory structure](../.phronia/spec/backend/directory-structure.md)
- [Server host protocol](../.phronia/spec/backend/server-host-protocol.md)
