# selftest: reboot_recovery must always run last in the interactive phase

## Goal

`reboot_recovery` reboots the device, which is slow and leaves the device in a
not-yet-ready state afterward. Today `run_interactive_phase`
(`adboost_cli/src/selftest/interactive.rs`) runs it BEFORE
`tcpip.shell_through_tcp_device`, so a post-reboot not-ready device can fail later
cases. Make `reboot_recovery` **structurally always the last interactive case** —
robust to future additions, not just a one-time reorder.

## Requirements

- `reboot_recovery` runs after every other interactive/tcpip case in
  `run_interactive_phase`.
- The ordering is enforced structurally (e.g. reboot is appended/run last by
  construction), so adding a new case later cannot accidentally push a test after
  reboot.
- The "operator declined" skip-recording path still records the same case names.
- No behavior change to the individual cases themselves.

## Acceptance Criteria

- [ ] In `run_interactive_phase`, `case_reboot_recovery` is invoked and recorded
      after `usb_replug` AND `tcpip.shell_through_tcp_device`.
- [ ] A code structure (ordering comment + a single "reboot last" call site, or a
      small ordered list) makes it evident reboot must stay last.
- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo build -p adboost_cli` green.

## Definition of Done

- reboot is last; structure resists regression; quality gate green; one commit.

## Out of Scope

- The frame-atomic write-timeout fix (separate task `06-22-frame-atomic-write-timeout`).
- Changing what any interactive case asserts.

## Technical Notes

- File: `adboost_cli/src/selftest/interactive.rs` (`run_interactive_phase` ~36-74).
  Current order: `usb_replug` → `reboot_recovery` → `tcpip.shell_through_tcp_device`.
  Target: `usb_replug` → `tcpip.shell_through_tcp_device` → `reboot_recovery` (last).
- This is a CLI selftest change only; no library (`adboost`) code involved.
