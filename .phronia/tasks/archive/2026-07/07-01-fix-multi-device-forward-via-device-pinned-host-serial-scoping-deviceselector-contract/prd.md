# Fix multi-device `forward` via device-pinned host-serial scoping (DeviceSelector contract)

## Goal

`ADBProxyDevice::forward` / `forward_remove` / `forward_remove_all` fail with
`more than one device/emulator` whenever ≥2 devices are attached, stranding
every consumer that relies on `adb forward` (xdb SSH relay / hyp-layer). Fix the
**root cause at the contract layer**: `host:forward`/`host:killforward` are
**device-pinned host services**, not device/local services, so they must be
addressed via `host-serial:<serial>:` / `host-transport-id:<id>:` prefixes — not
by a `host:transport:` switch followed by a bare `host:forward`.

## What I already know (verified in repo)

- **Same-connection sequence is the bug**: `forward.rs:10` `set_serial_transport()`
  opens a fresh socket (`tcp_proxy_transport.rs:202`) + sends `host:transport:<serial>`;
  `forward.rs:12` then writes `host:forward:...` on that *same* socket
  (`proxy_connection`, `tcp_proxy_transport.rs:55`). Real adb does not bind the
  bare `host:forward` to the previously-selected transport → auto-select → fail.
- **`shell` works on the same path** because `shell:`/`shell,v2` are genuine
  device services meant to follow `host:transport:`. That asymmetry is the tell.
- **Model mis-layering (root cause)**: `Forward`/`ForwardRemove`/`ForwardRemoveAll`
  live in `ADBLocalCommand` (`adb_local_command.rs:88-90`) yet render as
  `host:...` (`:157-161`). They are neither local nor plain-host: they are
  *device-pinned host* services.
- **Server side already models this correctly**: `frontend.rs:400 dispatch_pinned_prefix`
  parses `host-serial:<serial>:forward:...` / `killforward:` (`:481-491`) and the
  transport-id analogue. `forward.rs` (server) `parse_forward` supports the
  `norebind:` prefix and `tcp:` endpoints.
- **Selector precedence already exists once**: `set_serial_transport`
  (`adb_proxy_device.rs:78-90`) = `transport_id` → `identifier` → `TransportAny`.
  This precedence is the thing to hoist into a reusable `DeviceSelector`.
- **`reverse` must NOT change**: `reverse:forward:` is a genuine device-scoped
  service, correctly issued after `host:transport:`. Do not "symmetrically" edit it.

## ⚠️ Regression-test trap (critical)

adboost's OWN server tolerates the client hack: `frontend.rs:224-240` routes a
post-transport `host:forward` to the already-chosen serial. So a test that drives
the in-repo sim/server would **falsely pass** — this is exactly why the bug
escaped. The valid regression test asserts the **client-emitted wire string**
(unit-level, server-independent), e.g.
`assert_eq!(cmd.to_string(), "host-serial:ABC123:forward:norebind:tcp:17023;tcp:17023")`.

## Decisions (resolved in brainstorm)

- **[Q1 design] `DeviceSelector` contract (chosen).** Introduce
  `DeviceSelector { TransportId(u32) | Serial(String) | Any }` as the single
  source of truth for device selection. Two renderings:
  - `transport_cmd() -> ADBHostCommand` — `host:transport[-id]:…` for device
    services (shell etc.); replaces the if/else body of `set_serial_transport`.
  - `pin_prefix() -> Option<String>` — `host-serial:<serial>:` /
    `host-transport-id:<id>:` / `None` (auto) for device-pinned host services.
  Forward family moves out of `ADBLocalCommand` semantics and emits
  `<pin_prefix>forward:<local>;<remote>` (no `set_serial_transport` first).
- **[Q2b remove-all is GLOBAL] `forward_remove_all()` keeps bare `host:killforward-all`.**
  Research (research file §5) confirms AOSP `killforward-all` is process-global:
  a single global `listener_list`, `remove_all_listeners()` takes no transport,
  and native `adb -s <serial> forward --remove-all` STILL sends bare
  `host:killforward-all` (wipes all devices). So only `forward` and
  `forward_remove` get serial-scoped; `forward_remove_all` stays global and needs
  no `set_serial_transport` either. Scoping it would be non-standard + wrong.
- **[Q2 norebind] Keep current rebind behavior (no `norebind:`).** Research
  (see reference) confirms native `adb forward` default sends bare
  `host:forward:<local>;<remote>` = REBIND. `norebind:` is only for
  `--no-rebind`. Switching our default to norebind would DIVERGE from native.
  Target wire: `host-serial:<serial>:forward:tcp:<local>;tcp:<remote>`.
- **[Q3 CLI arg-order] Separate task — PROVEN, downstream-syncable.** See
  "Finding #2" below; provenance nailed. Fold into finish-work as a follow-up task.
- **[Q4 precedence] `transport_id → serial → auto`** — reuse existing
  `set_serial_transport` order verbatim inside `DeviceSelector`.

## Finding #2 (separate task): CLI forward arg-order swap — PROVEN

- `forward()` lib signature is `(remote, local)` (remote-first); Display
  `host:forward:{local};{remote}` is internally self-consistent.
- **Control group proves intent:** `reverse` handler passes `(remote, local)`
  correctly matching `reverse(remote, local)` — same file, adjacent line
  (`local_commands.rs:40`). Only the `forward` handler (`:35`) passes
  `(local, remote)` into `(remote, local)` → swapped.
- End-to-end: `adb forward tcp:1111 tcp:2222` → clap `Add{local,remote}` →
  `forward(local, remote)` binds remote="1111"/local="2222" → emits
  `host:forward:tcp:2222;tcp:1111` (reversed). Single-port selftest
  (`selftest/mod.rs:338`, calls lib directly) masks it.
- Provenance: introduced in `19aa24a` (CLI rebrand/async migration), not a
  deliberate signature contract. Safe to fix + sync the single downstream (xdb).

## ⚠️ Protocol nuance (design-confidence note)

Research found modern native adb pins forward via `host:tport:serial:<serial>`
(force_switch, returns transport-id → server binds subsequent forward to it),
then bare `host:forward:`. adboost sends the OLDER `host:transport:<serial>`,
which the server does NOT sticky-bind to the following forward → the field bug.
Regardless of that mechanism, `host-serial:<serial>:forward:` is the safer target:
documented in SERVICES.TXT, accepted by native AND adboost's own server
(`frontend.rs:481`), self-contained (zero connection-state dependency), and
already used in production by xdb's killforward fallback.

## Requirements (evolving)

- Multi-device `forward`/`forward_remove`/`forward_remove_all` emit a device-pinned
  host command (no `set_serial_transport` first) and succeed with ≥2 devices.
- Selector precedence (`transport_id` → `serial` → auto) lives in exactly one place.
- Bare `host:forward` (auto-select) retained only when neither id nor serial known.

## Acceptance Criteria

- [ ] `DeviceSelector` is the sole place encoding `transport_id → serial → auto`;
      `set_serial_transport` delegates to `selector.transport_cmd()`.
- [ ] Serial-scoped forward wire string:
      `assert_eq!(..., "host-serial:ABC123:forward:tcp:17023;tcp:17023")`.
- [ ] Transport-id-scoped forward wire string:
      `host-transport-id:<id>:forward:tcp:…;tcp:…`.
- [ ] Serial-scoped killforward: `host-serial:ABC123:killforward:tcp:17023`.
- [ ] Auto fallback (neither id nor serial) still emits bare `host:forward:…`.
- [ ] `forward`/`forward_remove`/`forward_remove_all` no longer call
      `set_serial_transport`.
- [ ] `reverse` family wire strings unchanged (regression guard).
- [ ] Existing forward/killforward server tests + full suite green.
- [ ] **selftest coverage** (`adboost_cli/src/selftest/`): (a) existing
      single-device forward control-plane case stays green (no regression);
      (b) NEW real-hardware case that, when ≥2 devices are enumerated, drives
      `forward` against an explicitly-selected serial and asserts it succeeds
      (the only test that reproduces the actual server auto-select failure) —
      gracefully `Skipped` when <2 devices present (mirror the `adb_available`
      skip pattern). Use asymmetric local/remote ports so an arg-order/scope
      slip cannot pass silently.

## Research References

* [`research/native-adb-forward-norebind.md`](research/native-adb-forward-norebind.md)
  — native default is REBIND (bare `host:forward:`); `norebind:` only via
  `--no-rebind`; pinned form `host-serial:<serial>:forward:[norebind:]<local>;<remote>`.

## Definition of Done

- Tests added (wire-string unit tests, not sim-driven).
- `cargo clippy` (pedantic) + fmt + test green.
- Docs/comments updated where the model layer moves.
- Memory/spec updated: device-pinned-host-service bug class.

## Out of Scope (explicit)

- CLI `forward` arg-order bug (finding #2) — separate task.
- `reverse` family (correct as-is).
- Any sim-backend "reproduction" test (would false-pass; see trap above).

## Technical Notes

- Files: `adboost/src/models/adb_local_command.rs`, `adb_host_command.rs`,
  `adboost/src/proxy/device_commands/forward.rs`, `adboost/src/proxy/adb_proxy_device.rs`.
- Server reference (already correct): `adboost/src/server/frontend.rs:400-517`,
  `adboost/src/server/forward.rs`.
- Related memories: prefer-root-cause-fix-at-contract-layer, host-protocol-parity-gaps,
  sim-harness-regression-net, handle-library-api-exposure-requests.
