# Fix reboot-recovery selftest and proxy shell-v2 exit-code loss

## Goal

The interactive `selftest` run surfaced two defects. Both are real bugs (not
design limits), both verified by reading the code paths:

1. `interactive.reboot_recovery` FAILS with "reboot command failed: session
   channel closed" — the test issues the reboot over `shell:reboot` and reads
   the stream to EOF, but reboot tears the stream down immediately and the read
   returns `BrokenPipe`, which `shell_exec` treats as an error.
2. `through_server.shell_exit_code` is SKIPPED ("channel does not surface shell
   exit codes") even though `usb_direct.shell_v2` surfaces it fine — the proxy's
   shell-v2 reader discards an already-captured exit code on the trailing EOF.

Fix both so the interactive reboot case passes and the through-server exit-code
case stops being spuriously skipped.

## What I already know (verified root causes)

### Bug 1 — reboot_recovery
- `adboost_cli/src/selftest/interactive.rs:104` calls
  `conn.shell_exec("reboot")`.
- `shell_exec` (`adb_client/src/message_devices/usb/persistent.rs:1429`) loops
  reading the session; it only treats `UnexpectedEof` as a clean end (line
  1441). A rebooting device tears the stream down and the read yields
  `BrokenPipe` ("session channel closed", persistent.rs:1918-1921), returned as
  `Err` → the case fails.
- ADB has a dedicated `reboot:` local service. Its correct semantics: open the
  session, the device replies OKAY, done — no EOF read needed (reference impl:
  `adb_client/src/message_devices/commands/reboot.rs:11-19`).
- `PersistentUsbConnection::open_session` already confirms OKAY before returning
  (persistent.rs:1193-1199), so a thin `reboot` wrapper that opens
  `ADBLocalCommand::Reboot(..)` and drops the session is sufficient.
- `PersistentUsbConnection` currently has NO reboot method.

### Bug 2 — proxy shell-v2 exit code lost
- `ADBProxyDevice::shell_command` picks v2 when the server advertises
  `shell_v2`/`cmd` (it does), routing to `shell_command_v2`
  (`adb_client/src/proxy/adb_proxy_device_commands.rs:183`).
- The v2 loop captures the exit byte into `exit` at line 277, then loops back to
  read the next 5-byte frame header at line 214. The device has closed the
  stream, so `read_exact` returns `UnexpectedEof` and line 216 does
  `return Ok(None)` — **discarding the captured `exit`**.
- The working direct path, `ShellV2Session::execute`
  (`adb_client/src/message_devices/usb/shell_v2_session.rs:150-152, 183-186`),
  `break`s on a between-frames EOF and returns the captured exit code. The proxy
  reader must do the same: `return Ok(exit)` instead of `Ok(None)`.

## Requirements

1. Add `PersistentUsbConnection::reboot(&self, reboot_type: RebootType)` that
   opens the `reboot:` service via `open_session(&ADBLocalCommand::Reboot(..))`
   and returns once the device has accepted it (OKAY already confirmed by
   `open_session`); the session is dropped/closed without reading to EOF.
2. Change `interactive.rs` `case_reboot_recovery` to call the new `reboot`
   method instead of `shell_exec("reboot")`.
3. Fix `shell_command_v2` so a between-frames EOF returns the captured exit code
   (`Ok(exit)`), aligning with `ShellV2Session::execute`. Both EOF sites that
   currently `return Ok(None)` (header read at line 216 and the exit-byte read
   at line 279) should be reviewed; the header-read site is the load-bearing fix.

## Acceptance Criteria

- [ ] `PersistentUsbConnection` exposes a `reboot` method using the `reboot:`
      service (no shell, no EOF read).
- [ ] `interactive.reboot_recovery` no longer fails on "session channel closed"
      (the reboot command itself succeeds; device-return wait is unchanged).
- [ ] `shell_command_v2` returns the captured exit code on trailing EOF.
- [ ] `through_server.shell_exit_code` PASSES (returns a non-zero code for
      `false`) instead of being SKIPPED, on a device whose server advertises
      shell_v2.
- [ ] `cargo test` (adb_client + adboost_cli) green; clippy clean.
- [ ] Unit test covering the v2 "EOF after exit-status frame returns the code"
      decode behavior if it can be expressed without a live device.

## Definition of Done

- Tests added/updated where expressible without hardware.
- `cargo clippy` / `cargo test` green.
- Comments updated where behavior changes (shell_exec vs reboot semantics).
- Interactive selftest re-run on hardware to confirm reboot_recovery passes
  (manual, hardware-gated).

## Technical Approach

- **Bug 1**: new method on `PersistentUsbConnection`:
  ```rust
  pub async fn reboot(&self, reboot_type: RebootType) -> Result<()> {
      // open_session confirms the device's OKAY; reboot needs nothing more.
      let _session = self.open_session(&ADBLocalCommand::Reboot(reboot_type)).await?;
      Ok(())
  }
  ```
  Dropping `_session` is fine (best-effort per-stream CLSE on drop). Then
  `interactive.rs` calls `conn.reboot(RebootType::System).await`.
- **Bug 2**: in `shell_command_v2`, change the header-read EOF branch from
  `return Ok(None)` to `return Ok(exit)`. The exit-byte-read EOF branch
  (line 279) returns `Ok(None)` for a *truncated* exit frame — leave as-is or
  also return `Ok(exit)` (it is `None` there anyway); document the choice.

## Decision (ADR-lite)

**Context**: Reboot must not be issued over a shell channel that the reboot
itself destroys; and the proxy v2 reader drops a captured exit code on the
normal end-of-stream.
**Decision**: Use the dedicated `reboot:` service for reboot; return the
captured `exit` on between-frames EOF in the proxy v2 decoder, matching the
direct `ShellV2Session` behavior.
**Consequences**: `selftest` interactive reboot passes; through-server exit
codes align with direct USB. No protocol/wire change; purely client-side
correctness.

## Out of Scope

- tcpip channel implementation (separately tracked, legitimately skipped).
- v1 shell exit codes / stderr separation (v1 protocol has no exit frame —
  matches native adb).
- LIS2 large-file directory listing (existing TODO, `commands/list.rs:186`).
- Exposing reboot/forward/reverse through the `ADBDeviceExt` trait.

## Technical Notes

- Files: `adb_client/src/message_devices/usb/persistent.rs`,
  `adboost_cli/src/selftest/interactive.rs`,
  `adb_client/src/proxy/adb_proxy_device_commands.rs`.
- Reference impl for reboot service: `commands/reboot.rs`.
- Reference for correct v2 EOF handling:
  `message_devices/usb/shell_v2_session.rs:143-199`.
</content>
</invoke>
