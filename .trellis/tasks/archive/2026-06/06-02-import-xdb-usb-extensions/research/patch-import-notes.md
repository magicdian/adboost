# Patch Import Notes

## Source patch
`/Volumes/MagicWork/xp-non-aosp-codes/xpeng-debug-bridge/patches/adb_client/0001-xdb-usb-extensions.patch`

Authored against adb_client **v3.2.1**. Our fork is **v3.2.2**.

## `git apply --check` result (run from repo root)
- Clean: Cargo.toml, lib.rs, adb_message_device.rs, adb_session.rs,
  message_devices/mod.rs, session_stream.rs (new), usb/adb_usb_device.rs,
  usb/mod.rs, usb/persistent.rs (new).
- FAILS: `adb_client/src/models/adb_local_command.rs` — context drift.

## adb_local_command.rs drift
Patch context assumes the enum ends:
```
    TcpIp(u16),
    Usb,
    Root,
}
```
But v3.2.2 has an extra variant after `Root`:
```
    Usb,
    Root,

    #[cfg(feature = "framebuffer")]
    FrameBuffer,
}
```
So apply the two additions manually:
1. Add `TcpConnect(u16)` enum variant (after `Root`, before the framebuffer cfg block).
2. Add Display arm `Self::TcpConnect(port) => write!(f, "tcp:{port}"),`
   (after `Self::Root => write!(f, "root:"),`).

## Decision: skip Cargo.toml hunk
Patch's Cargo.toml rewrite detaches adb_client from the workspace for standalone
publishing (hard-coded authors/edition/license, version=3.2.1, inline [lints]).
Our fork KEEPS the workspace (root Cargo.toml owns these, version 3.2.2). Applying
that hunk would break the workspace. => DO NOT apply Cargo.toml changes.

## New symbols introduced (public API surface)
- `message_devices::session_stream::ADBSessionStream<T>`
- `message_devices::usb::persistent::{PersistentUsbConnection, MultiplexedSession,
  SessionReadHalf, SessionWriteHalf, SessionChannels}`
- `ADBMessageDevice` re-exported from `message_devices`
- `ADBLocalCommand::TcpConnect(u16)`
- `ADBUSBDevice::inner_mut()`
- `ADBMessageDevice::open_session` becomes `pub`

## Verify
- `cargo build -p adb_client --features usb`
- `cargo test -p adb_client`
- `cargo clippy -p adb_client --features usb`
