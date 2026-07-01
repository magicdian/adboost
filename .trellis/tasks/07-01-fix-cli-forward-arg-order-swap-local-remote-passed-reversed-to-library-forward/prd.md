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

## Decision needed (blocking): where to fix

Two ways to make the CLI correct — pick one, they are NOT both:
1. **Swap at the call site** (`local_commands.rs:35`): `forward(remote, local)`.
   Smallest; library signature (remote-first) unchanged; matches the `reverse`
   arm exactly.
2. **Flip the library signature** to `forward(local, remote)` (+ its callers /
   `selftest` / xdb). Larger blast radius; touches the public API + downstream.

Recommend **(1)** — the bug is purely in the handler; the library + its new
`ADBHostCommand::Forward` are already correct and unit-tested. Confirm before
implementing.

## Downstream sync

- Maintainer confirmed there is exactly ONE downstream (xdb) and can sync it if
  the library API changes. If we choose (1), the library API does NOT change, so
  no downstream sync is needed — only the CLI behavior is corrected. This is
  another point in favor of (1).

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
