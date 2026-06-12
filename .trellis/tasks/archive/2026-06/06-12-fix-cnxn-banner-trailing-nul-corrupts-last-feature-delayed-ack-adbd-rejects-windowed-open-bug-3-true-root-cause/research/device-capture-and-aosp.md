# Device capture + AOSP banner-parsing facts (bug #3 true root cause)

## Live device capture (Android 16, MediaTek 0e8d:201c, serial YTGUSCNFMFAIK7ZP)
Throwaway harness `/tmp/adboost-diag` used `subscribe_raw(|_|true)` (frame tee
before routing) + `open_session(shell:getprop)`.

| Run | banner | OPEN arg1 | device reply | result |
|---|---|---|---|---|
| WINDOWED (current code) | `...delayed_ack\0` (trailing NUL) | 32 MiB | `CLSE(arg0=0, arg1=local_id)` in ~1.8ms | rejected |
| CLASSIC (control) | `shell_v2` (no delayed_ack), no relevant NUL effect | 0 | `OKAY` | success |
| WINDOWED (NUL removed) | `...delayed_ack` (no NUL) | 32 MiB | `OKAY data_len=4 payload=[00,00,00,02]` (=32MiB LE grant) | success in 18ms |

Single NUL byte flips reject→accept. Decisive.

## AOSP `packages/modules/adb` (refs/heads/main) — verified
- Host CNXN payload: `adb.cpp get_connection_string()` → `host::features=<csv>` via
  `Join(props, ';')`; host omits ro.product.* (`#if !ADB_HOST`). `send_connect`
  assigns payload from the string with **NO appended NUL**.
- adbd parse: `handle_new_connection` → `banner(payload.begin(), payload.end())`
  (no NUL strip on banner; `StripTrailingNulls` only on A_OPEN address, `adb.cpp:519`).
- `parse_banner`: `Split(banner,":")` → `pieces[2]` (third field) → `Split(pieces[2],";")`
  → `Split(prop,"=")`; on `key=="features"` calls `SetFeatures(value)` (`adb.cpp:379-380`).
- `StringToFeatureSet` = `Split(value, ",")` with **no trim** (`transport.cpp:1247-1251`).
  Trailing NUL → last token `"delayed_ack\0"` != `"delayed_ack"`.
- `SetFeatures` → `delayed_ack_ = CanUseFeature(features_, kFeatureDelayedAck)`
  (exact-string membership, `transport.cpp:1260-1270`). `kFeatureDelayedAck="delayed_ack"`.
- A_OPEN handler: `send_bytes=arg1`; `if (SupportsDelayedAck() != bool(send_bytes)) send_close(0, arg0, t)`
  (`adb.cpp:507`). This is the ONLY immediate-CLSE-on-OPEN path with arg0=0. The window
  magnitude (32 MiB) is NOT range-checked — only the boolean must agree.
- Windowed OPEN format (`sockets.cpp connect_to_remote`): arg0=local_id,
  arg1=INITIAL_DELAYED_ACK_BYTES (32MiB) when SupportsDelayedAck else 0, payload=service\0.
  → adboost's 32 MiB in arg1 is CORRECT; the defect is purely the banner NUL making
  adbd's SupportsDelayedAck()=false.

## adboost anchors
- `models/device_feature_set.rs:135-137` to_banner_string (the `\0` defect) + tests 144-182.
- `usb/persistent.rs:~541` stale comment "trailing NUL matches a real adb server" (false).
- `adb_message_device.rs` builds `host::{pkg}\0` — different banner, no features; not this bug.
