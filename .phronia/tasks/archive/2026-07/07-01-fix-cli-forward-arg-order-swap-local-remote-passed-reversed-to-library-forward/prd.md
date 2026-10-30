# Fix CLI `forward` arg-order swap (local/remote passed reversed to library `forward`)

## Goal

`adboost_cli`'s `forward` handler passes `(local, remote)` into the library
`forward(remote, local)` signature — swapping the two ports. `adb forward
tcp:1111 tcp:2222` emits `host:forward:tcp:2222;tcp:1111` (reversed). Fix the
handler so the CLI forwards the ports in the order the user gave.

## Proven (do not re-litigate — this is a confirmed bug)

- Library signature: `ADBProxyDevice::forward(&mut self, remote: String, local: String)`
  (remote-first); its Display is internally self-consistent.
- **Control group proves the swap is a mistake, not intent:** the `reverse`
  handler passes `(remote, local)` correctly matching `reverse(remote, local)` —
  same file, adjacent line (`adboost_cli/src/handlers/local_commands.rs:40`).
  Only the `forward` arm (`:35`) passes `(local, remote)` into `(remote, local)`.
- clap def (`adboost_cli/src/models/local.rs:37`): `Add { local, remote }` (CLI
  order `forward <local> <remote>`, matches adb).
- End-to-end: `Add{local:"tcp:1111", remote:"tcp:2222"}` → `forward(local, remote)`
  → binds remote="1111"/local="2222" → `host:forward:tcp:2222;tcp:1111`.
- Provenance: introduced in `19aa24a` (CLI rebrand/async migration).
- Single-port selftest (`selftest/mod.rs`, calls the library directly, symmetric
  ports) masked it end-to-end.

## Decision (RESOLVED by evidence): fix at the call site (option ①)

Two ways to make the CLI correct:
1. **Swap at the call site** (`local_commands.rs:35`): `forward(remote, local)`.
   Library signature (remote-first) unchanged; matches the `reverse` arm exactly.
2. **Flip the library signature** to `forward(local, remote)`.

**Chosen: (1).** Evidence makes (2) actively unsafe:

- **xdb calls the library `forward()` directly and ALREADY correctly** — with an
  explicit comment. `xdb-core/src/adb.rs:417-427`:
  ```rust
  pub async fn forward_tcp(&mut self, local_port: u16, remote_port: u16) -> Result<()> {
      // adboost: forward(remote, local) — param order reversed from CLI
      let remote_str = format!("tcp:{}", remote_port);
      let local_str  = format!("tcp:{}", local_port);
      self.device.forward(remote_str, local_str).await …
  ```
  Flipping the library signature would SILENTLY break xdb (its
  local/remote would swap). xdb pins adboost by git rev, so a signature flip is a
  breaking change to the one confirmed downstream, to fix a bug that is NOT in the
  library.
- The library `forward(remote, local)` + `reverse(remote, local)` share one
  remote-first convention (both inherent `ADBProxyDevice` methods, not in
  `ADBDeviceExt`). Option (1) keeps forward/reverse handler arms symmetric.
- The bug is purely in the CLI handler; the library + `ADBHostCommand::Forward`
  are already correct and unit-tested (task 07-01-...-deviceselector-contract).

## Research: native adb CLI arg order (see research file)

`adb forward LOCAL REMOTE` (local-first); `adb reverse REMOTE LOCAL` (remote-first)
— mirrors sharing one `forward:<arg0>;<arg1>` wire mapping. adboost_cli's clap
defs already match this: `ForwardCommand::Add { local, remote }` (local-first),
`ReverseCommand::Add { remote, local }` (remote-first). So ONLY the handler
call-site order is wrong; the CLI surface is correct and must not change.

## Acceptance Criteria

- [ ] `adb forward tcp:1111 tcp:2222` emits `…forward:tcp:1111;tcp:2222`.
- [ ] Regression test that asserts the emitted order for asymmetric ports
      (end-to-end through the handler, not just the library — the library is
      already covered).
- [ ] `reverse` arm behavior unchanged.

## Out of Scope

- The multi-device host-serial scoping fix (separate, already-done task
  `07-01-fix-multi-device-forward-via-device-pinned-host-serial-scoping-...`).

## Technical Notes

- Files: `adboost_cli/src/handlers/local_commands.rs:32-35`,
  `adboost_cli/src/models/local.rs:30-38`.
- Depends conceptually on the scoping task landing first (shared `forward.rs` /
  wire layer), but the two fixes are independent commits.
