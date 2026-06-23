# select_tport: handle `tport:usb` / `tport:local` kind tokens (adb -d/-e phase 2)

## Goal

Fix `adb -d shell` / `adb -e shell` failing with `error: device not found`
against an adboost server frontend, even though `adb -d devices` lists the
device. This is a residual gap from task
`06-23-host-usb-host-local-prefix-transport-usb-local-for-adb-d-e`: that task
added `transport-usb`/`transport-local` arms but missed the `host:tport:usb` /
`host:tport:local` path that modern `adb` actually uses for phase 2.

## Background — root cause (confirmed by live ADB_TRACE capture)

`adb 35.0.2 -d shell true` emits two requests:

1. `host-usb:features`  → **works** (phase 1, added in the prior task)
2. `host:tport:usb`     → **fails** ← the bug

`select_tport` (`frontend.rs:799-850`) parses its selector tail as one of:
`any`/`-any`/empty, `id:<N>`/`-id:<N>`, else **treated as a serial**. So
`rest == "usb"` falls into the serial branch, looks for a device literally named
`"usb"`, finds none → `Err("device not found")`. The error matches the report
verbatim.

Captured wire evidence (`ADB_TRACE=all adb -d shell true`):

```
adb_client.cpp:358 adb_connect: service: host-usb:features
adb_client.cpp:119 Switch transport in progress: host:tport:usb
```

So modern `adb` uses `tport:` for the transport switch (it wants the 8-byte
transport-id back), NOT the legacy `transport-usb`. The prior task's
`transport-usb`/`transport-local` `match svc` arms are correct but were never on
the default client path; `tport:` is. `adb -e` is the same with `host-local:` /
`host:tport:local`.

**Why the prior task's tests missed it**: the `tport` unit tests only covered
`any` / `serial` / `id` selectors — never `tport:usb` / `tport:local`. The new
kind tests exercised `transport-usb`/`transport-local` (the arm that *was*
added), so both green while the real client path stayed broken.

## Decision (ADR-lite)

**Context**: `select_tport`'s `usb`/`local` handling must produce the same
kind-filtered single-device resolution and the same kind-specific AOSP error
wording as `select_transport_kind`, but `select_tport` additionally needs the
serial set in scope to compute the transport-id, and it has already fetched
`list_devices()`.

**Decision**: Reuse the existing kind resolver, factored to avoid a double
fetch. Extract the filter + zero/one/many core of `resolve_single_by_kind` into a
**pure helper over a device slice**, e.g.
`pick_single_by_kind(devices: &[DeviceEntry], want: Option<TransportKind>) ->
Result<&str, &'static str>` (using the existing `kind_matches` /
`no_devices_msg` / `ambiguous_msg`). Then:

- `resolve_single_by_kind` becomes a thin async wrapper: fetch, delegate to the
  pure helper, clone the serial.
- `select_tport` gains a `usb`/`local` branch (via the existing
  `parse_transport_kind`) that calls the **same** pure helper on its
  already-fetched `devices`, then runs the unchanged `transport_id_for` +
  `okay_tport` success path.

This keeps the "every transport-selection path funnels through one resolver"
invariant from `server-host-protocol.md` literally true (one shared core), with
no second `list_devices()` call and no duplicated wording.

**Consequences**: `select_transport_kind` / `dispatch_host_kind` /
`select_transport_any` keep delegating through `resolve_single_by_kind` (now
backed by the pure helper) — unchanged behavior. Only `select_tport` gains the
new branch. No contract/`DeviceEntry` change.

## Requirements

- `host:tport:usb` selects the single USB device (reply `OKAY` + 8-byte LE
  transport-id), `host:tport:local` the single local/TCP device.
- `tport:usb` / `tport:local` use kind-specific AOSP wording on zero /
  more-than-one, identical to `transport-usb`/`transport-local`:
  usb → `no devices found` / `more than one USB device`;
  local → `no emulators found` / `more than one emulator`.
- Existing `tport` selectors (`any`/empty/`serial:<s>`/`<s>`/`id:<N>`) and their
  wording unchanged.
- `tport:serial:usb` (a device genuinely named `usb` via the explicit `serial:`
  form) must still resolve by serial — only the bare `usb`/`local` tokens are
  kind tokens, matching AOSP. (Bare `tport:<serial>` where the serial happens to
  be `usb`/`local` is not representable unambiguously; AOSP treats bare
  `usb`/`local` as the type — we match that and rely on `serial:` for the literal
  case.)
- Untagged backend (`kind: None`): `tport:usb` degrades to transport-any
  uniqueness (single device selected), no regression.

## Acceptance Criteria

- [ ] `select_tport` resolves `usb`/`local` tokens via the shared pure kind
      helper; no second `list_devices()` fetch.
- [ ] `resolve_single_by_kind` refactored to delegate to the same pure helper
      (no behavior change; existing tests still pass).
- [ ] New unit tests: `tport:usb` single-USB → `OKAY`+id; `tport:local`
      single-local → `OKAY`+id; mixed topology `tport:usb` picks USB / `tport:local`
      picks TCP; `tport:usb` with two USB → `more than one USB device`;
      `tport:usb` with only TCP → `no devices found`; `tport:local` mirror;
      untagged single device `tport:usb` → `OKAY`+id.
- [ ] Existing `tport_*` tests unchanged and green.
- [ ] fmt + clippy(pedantic, `-D warnings`, default and `--features usb,server`)
      + `cargo test -p adboost` all green.
- [ ] `server-host-protocol.md` updated: note that `host:tport:usb`/`tport:local`
      are the modern `-d`/`-e` phase-2 path and route through the same resolver;
      the `tport` selector list includes `usb`/`local`.

## Definition of Done

- Regression test proving `host:tport:usb` no longer returns `device not found`.
- Spec updated in lockstep; one bug = one task = one commit.

## Out of Scope

- The `transport-usb`/`transport-local` `match svc` arms (already correct; kept
  for older clients / direct callers).
- Bridging `shell:`/`sync:` through the server to a TCP device (separate
  deferred follow-up, unchanged).
- Any `DeviceEntry` / backend contract change (none needed).

## Technical Notes

- Touch points: `frontend.rs` `select_tport` (~799-850), `resolve_single_by_kind`
  (~541), pure helpers `kind_matches`/`no_devices_msg`/`ambiguous_msg`/
  `parse_transport_kind` (already present from the prior task).
- Root cause and the `host:tport:usb` vs `transport-usb` distinction were
  confirmed by `ADB_TRACE=all adb -d shell true` against adb 35.0.2.
- Spec contract to keep in lockstep:
  `.trellis/spec/backend/server-host-protocol.md` (transport-selection table +
  Tests Required). Relates to memory `host-protocol-parity-gaps`.
