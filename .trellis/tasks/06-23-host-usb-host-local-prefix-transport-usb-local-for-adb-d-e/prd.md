# host-usb/host-local prefix + transport-usb/local for `adb -d` / `adb -e`

## Goal

Make `adb -d` (force USB) and `adb -e` (force emulator/TCP-local) work against an
adboost server frontend, **aligned with native adb semantics**, including true
transport-type disambiguation in mixed USB+TCP topologies. Today both flags fail
at the first request (`unknown service: host-usb:features` /
`host-local:features`) because `dispatch_host_service` strips only `host-serial:`
and `host:`, and there is no `transport-usb` / `transport-local` selection arm.

Reported by xdb (downstream `DeviceBackend` consumer) whose topology is mixed USB
+ `host:port` TCP devices; they explicitly need real `-d`/`-e` disambiguation, not
a single-device degenerate workaround.

## Background — confirmed root cause (verified against rev c3d16a7)

Native adb encodes transport *type* into the host request prefix, in two phases:

| client | phase-1 prefix | phase-2 transport switch |
|---|---|---|
| (default) | `host:` | `host:transport-any` |
| `-s <serial>` | `host-serial:<serial>:` | `host:transport:<serial>` |
| `-d` (USB) | `host-usb:` | `host:transport-usb` |
| `-e` (local/emulator) | `host-local:` | `host:transport-local` |

- `frontend.rs:275` — `host-usb:`/`host-local:` match neither `strip_prefix`, fall
  to `else` → `unknown service: host-usb:features`. Matches the report verbatim.
- `frontend.rs:292-362` — `match svc` has `transport-any`/`transport:`/
  `transport-id:`/`tport:` but **no** `transport-usb`/`transport-local` arm.

Both phases of `-d`/`-e` therefore fail.

## Decision (ADR-lite)

**Context**: Two approaches were on the table — (A) alias `host-usb:`/`host-local:`
to `host:` and treat `transport-usb`/`transport-local` as `transport-any`
(unblocks `-d`/`-e` but cannot disambiguate in mixed topologies), or (B) carry a
real transport-kind filter end to end. Maintainer directive: an elegant,
long-term-maintainable design aligned with native adb as closely as possible — not
a single-point quick fix.

**Decision**: Approach **B**, modeled on AOSP's `TransportType` + single
`acquire_one_transport` filter:

1. **Contract extension (graceful, opt-in, future-proofed with `#[non_exhaustive]`).**
   Add `TransportKind { Usb, Local }` and `kind: Option<TransportKind>` to
   `DeviceEntry` (`backend.rs`), and mark `DeviceEntry` `#[non_exhaustive]`. This
   mirrors the existing `capabilities: Option<DeviceFeatureSet>` precedent: `None` =
   "this backend doesn't tag transport kind", treated conservatively as **matches
   any kind** so untagged backends keep working unchanged (`adb -d`/`-e` degrade to
   transport-any-over-the-matching-set, never worse than today). `DeviceEntry::new`
   defaults `kind: None`; add a `with_kind(TransportKind)` builder alongside the
   existing `with_capabilities` so external backends can set it. Downstream backends
   (xdb) opt into real disambiguation by populating it — consistent with the
   maintainer's "standard defaults + opt-in customization" philosophy.
   - AOSP itself has **no** unknown transport type (every transport is concretely
     USB or Local). `None` is *our* contract's "downstream hasn't told us" state,
     not an AOSP concept — kept only for backward-compatible trait evolution.
   - **Why `#[non_exhaustive]` now**: `DeviceEntry` is the public `DeviceBackend`
     seam. Verified blast radius today: **0 cross-crate struct-literal
     constructors** (`adboost_cli`/`examples`/`benches`/`pyadb_client` never build a
     `DeviceEntry` — they consume via the backend trait), and 7 in-crate literals in
     `adboost/src/` that `#[non_exhaustive]` does **not** restrict (same-crate).
     With the only external backend (xdb) already accepting this contract change and
     more `DeviceEntry` fields likely over time, forcing the `::new()` + builder path
     now makes every future field addition zero-break for downstreams at essentially
     no present cost. First `#[non_exhaustive]` in `server/` — record the convention
     in the spec.

2. **Default backend tags concretely (free).** `enumerate_usb()` →
   `TransportKind::Usb`; `tcp_device_entries()` → `TransportKind::Local`
   (`default_backend.rs:142,628`). So adboost's own behavior is fully AOSP-aligned
   out of the box.

3. **One shared kind-filter, every selection path agrees.** Introduce a single
   helper (the `acquire_one_transport` analogue) that filters the device list by an
   optional `TransportKind` then applies the existing zero/one/many uniqueness, so
   `transport-usb`/`transport-local`, `host-usb:`/`host-local:`, and the existing
   `transport-any`/`tport`/forward paths all route through **one** place. This
   directly serves the spec's core invariant: "every transport-selection path must
   agree on AOSP error semantics."

4. **Prefix = type-filtered `host:` (phase 1).** `host-usb:<sub>` / `host-local:<sub>`
   are structurally `host-serial:` with the device pinned by kind instead of serial.
   Route them through the per-device sub-service dispatch against the single
   kind-resolved device (covers `features`, `get-state`, forward-family, and the
   terminal `transport`/`tport` switch).

5. **Full AOSP error-wording parity (phase 2 + existing paths).** Adopt AOSP's
   type-specific wording everywhere (parity sweep, not just new paths):

   | condition | any | usb | local |
   |---|---|---|---|
   | 0 devices | `no devices/emulators found` | `no devices found` | `no emulators found` |
   | >1 devices | `more than one device/emulator` | *(verify exact bytes)* | `more than one emulator` |

   Verified against local `adb 35.0.2` binary strings. **Open detail flagged for
   implementation**: the USB-ambiguous literal — the binary exposes `more than one
   device/emulator`, `more than one emulator`, and `more than one device with
   serial`, but no standalone `more than one device`; confirm the exact
   `kTransportUsb` ambiguous string by grepping the target AOSP `transport.cpp`
   before pinning a test assertion.

6. **`host:devices`/`devices-l` stay type-agnostic** (AOSP never filters the device
   *list* by `-d`/`-e`; only selection is filtered). No change there.

**Consequences**:
- `DeviceBackend` trait contract grows one optional field, and `DeviceEntry`
  becomes `#[non_exhaustive]`. **Cross-crate adaptation required: none** — no
  downstream or sibling crate (`adboost_cli`, `examples`, `benches`,
  `pyadb_client`) constructs a `DeviceEntry` today; they consume via the backend
  trait. The 7 in-crate struct literals (mostly `frontend.rs` tests) add `kind:` —
  required by the new field regardless of `#[non_exhaustive]`. From now on external
  backends must build via `DeviceEntry::new(..).with_kind(..).with_capabilities(..)`
  instead of struct literals; future field additions are then zero-break downstream.
- The executable spec `server-host-protocol.md` Error Matrix + Tests Required must
  be updated to the AOSP wording in the same change (contract and code move
  together).
- Full-parity wording changes the existing `transport-any`/`tport`/forward error
  strings and their unit tests + the `case_official_adb_ambiguous_shell` parity
  assertion — in scope by explicit decision (long-term upstream alignment over
  minimal churn).

## Requirements

- `adb -d <cmd>` and `adb -e <cmd>` succeed end-to-end against adboost when a single
  device of the matching kind is present (both phases: `host-{usb,local}:features`
  then `transport-{usb,local}`).
- In a mixed USB+TCP topology with a tagging backend, `-d` selects the USB device
  and `-e` selects the TCP/local device; ambiguity within a kind yields the
  AOSP "more than one …" wording for that kind.
- `host-usb:`/`host-local:` support the same per-device sub-services as
  `host-serial:` (at minimum `features`, `get-state`, `get-serialno`,
  forward-family, `transport`/`tport`).
- `DeviceEntry` carries `kind: Option<TransportKind>`; `None` matches any kind
  (conservative, backward-compatible). Default backend populates it concretely.
  `DeviceEntry` is `#[non_exhaustive]` with a `with_kind(TransportKind)` builder;
  `DeviceEntry::new` defaults `kind: None`.
- All transport-selection paths share one kind-aware uniqueness helper and emit
  consistent AOSP error wording.
- `host:devices`/`devices-l` remain unfiltered by kind.
- `server-host-protocol.md` updated: Error Matrix → AOSP wording; add `host-usb:`/
  `host-local:` + `transport-usb`/`transport-local` rows; document the
  `TransportKind`/`kind: Option<_>` contract, the "None = matches any" rule, and the
  `#[non_exhaustive]` + builder convention (external backends construct via
  `new().with_kind().with_capabilities()`); refresh the Tests-Required list.

## Acceptance Criteria

- [ ] `host-usb:` / `host-local:` prefixes are stripped and dispatched (no more
      `unknown service: host-usb:features`).
- [ ] `transport-usb` / `transport-local` `match svc` arms select via the shared
      kind-filtered uniqueness helper and return `TransportSelected`.
- [ ] `DeviceEntry` has `kind: Option<TransportKind>`, is `#[non_exhaustive]`, and
      exposes `with_kind`; `DeviceEntry::new` defaults `None`; `None` matches any
      kind. `DefaultDeviceBackend` tags Usb/Local. All in-crate literals updated.
- [ ] Unit tests (features `server,usb`): single USB dev → `-d` phase-1+phase-2 ok;
      single local dev → `-e` ok; mixed set → `-d` picks USB / `-e` picks Local;
      kind-ambiguous → correct AOSP wording; wrong-kind-absent → correct "no …"
      wording; untagged (`None`) backend → behaves as transport-any (no regression).
- [ ] Existing transport-any/tport/forward error-string tests updated to AOSP
      wording and passing.
- [ ] `server-host-protocol.md` Error Matrix, new service rows, contract notes, and
      Tests-Required list updated to match.
- [ ] `case_official_adb_ambiguous_shell` parity case updated to AOSP wording;
      consider an analogous `-d`/`-e` parity assertion if feasible with one device.
- [ ] `cargo build`/`clippy`/tests green under `--features "server,usb"`.

## Definition of Done

- Tests added/updated (unit + parity where applicable); lint/clippy/build green.
- Executable spec updated in lockstep with behavior.
- Downstream impact noted: trait `DeviceEntry` gained one optional field; default
  is backward-compatible (no required downstream change).

## Out of Scope

- Bridging local services (`shell:`/`sync:`) *through* the server to a TCP device —
  blocked on the `MultiplexedSession`/`PersistentUsbConnection` generalization
  already documented as a deferred follow-up in `server-host-protocol.md`. `-e` to a
  TCP device selects the transport correctly; actually bridging its shell is a
  separate task and unchanged by this work.
- `host:transport-id:` / `-s` behavior (already correct).
- Any `devices`/`devices-l` kind filtering (intentionally type-agnostic per AOSP).

## Research References

* [`research/native-adb-transport-kind.md`](research/native-adb-transport-kind.md)
  — AOSP `TransportType` (Usb/Local/Any/Host), single `acquire_one_transport`
  type+serial+id filter with zero/one/many wording, `-d`/`-e` two-phase client flow,
  `devices` never type-filtered, and "no unknown transport type" in AOSP.

## Technical Notes

- Touch points: `backend.rs:28` (`DeviceEntry` + `TransportKind`),
  `default_backend.rs:142,628` (tag kind), `frontend.rs:256` (`dispatch_host_service`
  prefix strip), `frontend.rs:292-362` (`match svc` arms), `frontend.rs:659-679`
  (`select_transport_any` → shared kind-aware helper), `frontend.rs:378`
  (`dispatch_host_serial` is the reuse model for the per-device prefix path).
- Authoritative error strings come from local `adb 35.0.2`; the one unverified
  literal (USB-ambiguous) must be grep-confirmed against the target AOSP
  `transport.cpp` before a byte-exact test assertion is pinned.
- Spec contract to keep in lockstep: `.trellis/spec/backend/server-host-protocol.md`
  (Transport selection error matrix + Tests Required).
