# Product

## Typed adb device tracker

Downstreams that read the adb server's device tracker no longer parse adb text.
`adboost::tracker` provides:

- one complete `DeviceSnapshot` per tracker frame, with typed `DeviceInfo`;
- validated `DeviceSerial` identities (the whole rendered line can never become
  a serial);
- automatic negotiation of `host:track-devices-proto-binary` vs the AOSP
  `host:track-devices-l` text format;
- tolerant decoding of unknown protobuf fields/enums and malformed rows
  (`malformed_rows` is reported, never inserted);
- a cancel-safe `next_snapshot()` loop and an explicit reconnect/health policy.

The existing `ADBProxyServer::track_devices` callback API and the CLI
`host devices` / `host track-devices` commands are unchanged. The tracker is an
additive library surface for downstream tools such as `xdb`.

## Compatibility

- No new cargo dependency, feature, MSRV change, or `RustADBError` variant.
- `DeviceLong` / `DeviceShort` keep their signatures and now share the tracker's
  long-text parser.
- The adboost server frontend still does not advertise
  `devicetracker_proto_format`; the client tracker works against real adb
  (proto or long text) and against adboost's long-text frontend.
