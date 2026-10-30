# PRD: Import xdb USB Extensions Patch

## Background

Upstream `adb_client` rejected our patch. We forked `adb_client` v3.2.2, renamed
it `xp_adb_client`, and will develop independently. The patch
`xpeng-debug-bridge/patches/adb_client/0001-xdb-usb-extensions.patch` (authored
against v3.2.1) must be imported into this fork.

## Goal

Import the **functional** changes from the patch while preserving the current
workspace structure.

## Scope decisions (confirmed with user)

1. **Skip the `Cargo.toml` changes.** The patch rewrites `adb_client/Cargo.toml`
   to detach it from the workspace (hard-coded authors/edition/license,
   `version = "3.2.1"`, inline `[lints]`). This was for standalone publishing.
   Our fork keeps the full workspace (root `Cargo.toml`, `version = "3.2.2"`,
   `workspace.lints`), so we do NOT apply the Cargo.toml hunk.
2. **Apply functional code file-by-file.** 9 files apply cleanly via `git apply`;
   `adb_local_command.rs` needs a manual edit (context drift, see below).

## Files to change

### Apply cleanly (verified `git apply --check` passes for these)
- `adb_client/src/lib.rs` — relax `missing_docs`/`missing_debug_implementations`
  to `allow`; re-export `ADBLocalCommand`.
- `adb_client/src/message_devices/adb_message_device.rs` — make `open_session` `pub`.
- `adb_client/src/message_devices/adb_session.rs` — add doc comments; (already pub).
- `adb_client/src/message_devices/mod.rs` — expose submodules; add `session_stream`;
  re-export `ADBMessageDevice`.
- `adb_client/src/message_devices/session_stream.rs` — NEW: `ADBSessionStream`
  (Read+Write over a session).
- `adb_client/src/message_devices/usb/adb_usb_device.rs` — add `inner_mut()`.
- `adb_client/src/message_devices/usb/mod.rs` — expose `persistent` module +
  re-exports; make `usb_transport` `pub(crate)`.
- `adb_client/src/message_devices/usb/persistent.rs` — NEW: `PersistentUsbConnection`,
  `MultiplexedSession`, `SessionReadHalf`, `SessionWriteHalf` (session multiplexing
  over a single CNXN+AUTH'd USB connection with background reader thread).

### Manual edit (context drift v3.2.1 → v3.2.2)
- `adb_client/src/models/adb_local_command.rs` — add `TcpConnect(u16)` variant +
  its `Display` arm (`tcp:{port}`). The patch's context assumed `Root,` was the
  last variant, but v3.2.2 added `#[cfg(feature = "framebuffer")] FrameBuffer`
  after it, so the hunk must be placed manually.

### NOT changed
- `adb_client/Cargo.toml` — intentionally left as-is (workspace inheritance).
- Root `Cargo.toml` — unchanged.

## Acceptance criteria

- [ ] All functional code from the patch is present (session_stream, persistent,
      TcpConnect, inner_mut, open_session pub, re-exports).
- [ ] `adb_client/Cargo.toml` still uses workspace inheritance; version stays 3.2.2.
- [ ] `cargo build -p adb_client --features usb` succeeds.
- [ ] `cargo clippy -p adb_client --features usb` has no new errors (warnings from
      pedantic lints acceptable if pre-existing pattern).
- [ ] No regression to existing tests: `cargo test -p adb_client`.

## Out of scope

- adb_cli / pyadb_client changes (patch does not touch them).
- Publishing / version bump.
