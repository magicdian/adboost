# Backend hook: custom unsupported-service FAIL reason

## Goal

Give an injected `DeviceBackend` a defaulted, backward-compatible way to substitute
a human-actionable FAIL reason when the frontend rejects a post-transport local
service at the capability gate (`map_local_service`), instead of always emitting the
hardcoded `service not supported: {service}`. Motivating consumer: xdb's synthetic
`<serial>_hyp` SSH-bridged device, which can't satisfy `sync:`/`shell,v2` and wants
to point the user at `xdb pull/push --target hyp` at the exact moment of failure.

This is a contract-layer fix (one defaulted trait method + one call site), not a
single-point patch: any backend bridging a non-adbd endpoint (SSH/serial/proxy/sim)
routinely hits services it can't satisfy and is currently forced to emit the same
opaque string. It is the natural extension of adboost's "bespoke injected backend"
value proposition.

## What I already know (verified against rev at HEAD)

- `map_local_service` (`frontend.rs:1187`) is a **pure sync** fn returning
  `Result<ADBLocalCommand, String>`. It produces the rejection strings at
  `:1198` (`sync:`), `:1208` (`shell,*`), `:1234` (catch-all `service not
  supported`), and `:1219` (`invalid tcp port`).
- All of those funnel into the **single** `Err(reason)` arm of `serve_local_service`
  (`:1129-1135`) which writes `protocol::fail(&reason)` and returns. This is the one
  and only call site to touch.
- The `Raw("sync:")` / `Raw("shell,v2…")` verbatim bridge means the typed
  `open_sync_session()` / `open_shell_v2()` methods are **never** invoked on the
  injected-backend path — confirmed: no `open_sync_session` references in
  `frontend.rs`. So a backend cannot intercept the rejection by overriding those.
- The trait `DeviceBackend` (`backend.rs:219`, `#[trait_variant::make(Send)]`)
  already uses the defaulted-`async move { … }` opt-in pattern for
  `subscribe_lifecycle` (`:243`), `transport_alive` (`:278`), `release_reverse`
  (`:295`), `capabilities` (`:321`), `device_capabilities` (`:341`),
  `open_sync_session`/`open_shell_v2` (`:358`/`:377`). The new hook is isomorphic.
- Test infra: `MockBackend` in `frontend.rs` tests (`:1607`) is hardware-free and
  returns the rejection **before** `open_local_service` (which is `unimplemented!()`).
  An override-hook test asserts the reason reaches the client without touching the
  bridge. Pure-fn `map_local_service` unit tests live at `:2938-3104`.

## Key architectural decisions (analysis)

### Layering — hook at the async call site, NOT inside the pure mapper
`map_local_service` stays pure/sync (its 8 unit tests assert decisions + default
strings unchanged). The backend hook is `async` and lives in `serve_local_service`:
it maps the mapper's `Err(default)` → `Err(backend_override.unwrap_or(default))`.
This keeps the *decision* (pure, tested) cleanly separated from the *human-facing
reason* (async, backend-customizable). Folding the hook into the pure fn would force
it async and break the pure-test layer — rejected (this is also why the FR's
"richer return type" alternative is rejected: it changes the pure signature).

### Scope — `map_local_service` reject path ONLY (answers FR open-q#1 definitively)
`serve_local_service` has two FAIL paths:
- `map_local_service` reject (`:1132`): reason is **frontend-hardcoded** (all of
  `sync:`/`shell,*`/catch-all/`invalid tcp port`) → backend has no voice → **this
  hook covers all of these via one unified seam.**
- `open_local_service` failure (`:1142`, `open session failed: {e}`): the `{e}` is
  **already the backend's own error** → backend already controls this string via its
  `Result` → adding the hook here would be redundant.
So scoping to the map-reject path is not an inconsistency — it is the precise closure
of "the FAIL strings the frontend owns but the backend should." **This rationale
MUST be in the method doc** so future readers don't think the omission is arbitrary.

### Decision boundary — Err→Err only, never changes routing (honest banner intact)
The hook can only rewrite the reason of an already-decided rejection. It cannot turn
a rejection into an accept, cannot change which `ADBLocalCommand` is opened, and
cannot make the frontend advertise a feature it didn't. This preserves the
honest-banner principle and keeps the "service interceptor that accepts" idea
(FR-rejected as too large a surface) firmly out of scope.

### Single FAIL frame
The call site still emits exactly one `protocol::fail`; the hook only chooses its
payload. No double-reply.

## Requirements

- Add `local_service_reject_reason(&self, serial, service, default_reason)` to
  `DeviceBackend`, default `None`, written as an explicit `async move { None }` block
  (post-`trait_variant` rewrite requirement).
- In `serve_local_service`, on `map_local_service` `Err(default_reason)` (ALL error
  variants), consult the backend and `unwrap_or(default_reason)` before the single
  `protocol::fail`.
- Default backends (`DefaultDeviceBackend`, `SimDeviceBackend`, existing `MockBackend`)
  produce **byte-identical** FAIL output to today (no override → `None`).
- Method doc states: (a) it only customizes the reason, never routing/gating;
  (b) why only the map-reject path has it (open-path is already backend-authored);
  (c) it fires on every `map_local_service` rejection, backend self-selects by
  `service` and may wrap `default_reason`.

## Acceptance Criteria

- [ ] New defaulted `DeviceBackend` method; default returns `None`.
- [ ] `serve_local_service` consults it before the generic FAIL; `unwrap_or(default)`.
- [ ] Existing no-override backends: byte-identical FAIL (`service not supported: …`).
- [ ] Test: a backend overriding the hook sees its custom reason reach the client
      (full round-trip through `handle_client`, exact bytes asserted).
- [ ] Test: a non-overriding backend keeps `service not supported: <service>`.
- [ ] Frontend emits exactly one FAIL frame on this path (no double-reply).
- [ ] `map_local_service` pure-fn tests unchanged and still green.

## Definition of Done

- Tests added (override + default path) in `frontend.rs` test module.
- `cargo fmt` / `clippy` / `cargo test` green.
- Method doc complete (routing-invariance + gate-path-only rationale).
- No behavior change for any backend that doesn't override the hook.

## Out of Scope

- A general "service interceptor" that can *accept* otherwise-rejected services.
- Adding the hook to the `open_local_service` failure path (already backend-authored).
- Changing `map_local_service`'s signature/return type.
- Any xdb-side consumption code (lives in the xdb repo, not adboost).

## Decisions (resolved with maintainer)

1. **Name**: `local_service_reject_reason` — emphasizes this is the local-service
   path's reject reason (distinct from host-path FAILs), scope is explicit.
2. **Trigger**: fires on **ALL** `map_local_service` `Err` (incl. `invalid tcp
   port`), giving the call site a single unified seam. The backend self-selects via
   the `service` string and returns `None` for anything it doesn't care about. The
   pure mapper keeps returning `String` (no structured-error refactor) — the call
   site does not need to discriminate Err categories.
3. **Signature**: pass `default_reason` so a backend can **wrap/append** rather than
   only replace — e.g. `"{default} — for file transfer use xdb pull …"`. Default
   impl returns `None`, so zero behavior change for non-overriding backends.

   ```rust
   async fn local_service_reject_reason(
       &self,
       _serial: &str,
       _service: &str,
       _default_reason: &str,
   ) -> Option<String> {
       async move { None }
   }
   ```
   Call site (`serve_local_service`):
   ```rust
   Err(default_reason) => {
       let reason = self
           .backend
           .local_service_reject_reason(serial, service, &default_reason)
           .await
           .unwrap_or(default_reason);
       stream.write_all(&protocol::fail(&reason)).await?;
       return Ok(());
   }
   ```

## Technical Notes

- Files: `adboost/src/server/backend.rs` (trait method), `adboost/src/server/
  frontend.rs` (call site + tests).
- Mirror the doc/style of `device_capabilities` (`backend.rs:341`) for the new method.
- Test harness: extend/clone `MockBackend` in `frontend.rs` tests; use the existing
  `round_trip_select` + follow-up local-service request pattern to drive the FAIL.
