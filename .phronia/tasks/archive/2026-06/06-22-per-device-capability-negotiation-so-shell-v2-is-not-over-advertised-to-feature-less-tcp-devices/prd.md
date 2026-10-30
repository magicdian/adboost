# Per-device capability negotiation (stop over-advertising shell_v2 to feature-less devices)

## Goal

Today the server frontend negotiates capabilities **once, globally** at
`serve()` time (`frontend.rs:111-112`) from a device-agnostic
`DeviceBackend::capabilities()`. When one backend fronts devices of differing
capability — e.g. a full Android adbd (banner has `shell_v2`) **and** a stripped
adbd reached via `adb forward tcp:8885 tcp:6665` + `adb connect 127.0.0.1:8885`
(banner has an **empty** features segment, no `shell_v2`) — the frontend
advertises and gates `shell,v2` for **all** devices. The client then sends
`shell,v2,...,pty:` to the stripped adbd, which rejects the OPEN with `CLSE`
(`open session failed`). `adb shell` is unusable on the feature-less device.

The fix is **per-device** capability negotiation, designed not as a point-fix
but as a long-term modeling correction: a device's *identity* already travels
per-device (`DeviceEntry`: serial/state/product/model), but its *capabilities*
collapse into a single global call. This task makes capability a per-device
property sourced from the device's own CNXN banner.

## What I already know (verified in source)

* Global negotiation: `serve()` → `backend.capabilities()` → `caps.negotiated_with(..)`
  (`frontend.rs:111-112`). All later reads use the global `self.caps`.
* Three `host:features` reply sites, two already carry a serial:
  * `frontend.rs:313` — global `host:features` (pre-transport; data query).
  * `frontend.rs:341` — `host-serial:<serial>:features` (**has serial**).
  * `frontend.rs:171` — post-`transport` `host:features` (**serial chosen**).
    Native `adb -s <serial> shell` uses this one to pick v1 vs v2
    (`frontend.rs:152-156`).
* Two gating sites, both global: `sync:` (`frontend.rs:797`), `shell,` v2
  (`frontend.rs:806`) in `map_local_service`.
* **The report's "关键观察" is WRONG**: `PersistentConnection::device_features()`
  (`persistent.rs:643`) returns the `DeviceFeatureSet` adboost advertises **TO**
  the device — NOT the device's banner. The device banner is parsed during
  `do_connect` for **only `delayed_ack`** (`banner_advertises_delayed_ack`,
  `persistent.rs:313`); the full device-side feature set is **discarded**. Both
  fix directions therefore need NEW plumbing to parse + store + expose the
  device's banner feature set.
* `DeviceFeatureSet` has `to_banner_string()` (serialize) but **no** reverse
  parser. A `from_banner` parser must be added (symmetric to the existing
  delayed_ack scan, but for the full set).
* Timing: USB devices are enumerated from USB descriptors
  (`find_all_connected_adb_devices`) **before** any CNXN handshake — at
  `list_devices` time the banner is **not yet available**. The handshake happens
  lazily on first `open_local_service` (`get_or_open`). TCP devices are
  `host:connect`ed with the persistent connection (and banner) already in hand.
* STLS-upgraded TCP path returns an **empty** banner (`persistent.rs:833`) → that
  device's parsed feature set is empty → conservatively no `shell_v2`. This is
  the correct honest behavior, no special-casing needed.

## Key design decisions (from brainstorm)

1. **Capability lives on the device, modeled on `DeviceEntry`.**
   `DeviceEntry` gains `capabilities: Option<DeviceFeatureSet>` — identity and
   capability now share a lifetime. `None` = "not yet known" (USB device not yet
   handshaked); `Some` = parsed from that device's CNXN banner.

2. **Type = `DeviceFeatureSet`** (the device-banner truth), NOT
   `BackendCapabilities`. Rationale (long-term): the per-device truth source IS
   the device banner; `DeviceFeatureSet` already models exactly that and already
   has `cmd`/`sync_v2`-family fields, so future per-device honesty for more
   features is zero-friction. `BackendCapabilities` is a *backend-can-bridge*
   view (it even has `reverse`, which is not a device banner feature) — mixing
   the two semantics into one type is short-term convenient but long-term
   muddy. The final gate is the **intersection**:
   `device_banner_features ∩ backend_can_bridge`, made explicit at the frontend.

3. **`list_devices` stays lightweight.** USB entries enumerate with
   `capabilities: None` (no forced handshake → `track-devices` hotplug snapshots
   stay cheap, honoring the existing lazy-connection design); TCP entries (already
   connected) fill `Some`. The backend **caches** parsed banner capabilities, so a
   USB device that *has* been handshaked fills `Some` on later enumerations.

4. **Authoritative per-device query carries a timeout.** New backend hook
   `device_capabilities(serial, timeout) -> Option<DeviceFeatureSet>`:
   cache-hit returns immediately; otherwise it may attempt a handshake within the
   timeout window; on timeout / not-found returns `None`. The timeout lives on
   this **on-demand, single-device** call — NOT on `list_devices` (where a
   timeout would make streaming `track-devices` snapshots block on N handshakes).
   Default trait impl returns `None` (back-compat: existing backends unchanged).

5. **Defense in depth = per-device `host:features` + gate fallback.**
   * Primary: the serial-aware `host:features` replies (`frontend.rs:171`, `:341`)
     answer with `global_caps ∩ device_caps`, so the client **gracefully**
     selects v1 for a feature-less device (the right UX — no failed OPEN).
   * Fallback: before routing `shell,v2` / `sync:` (`map_local_service`), re-check
     the target device's capabilities; if absent, FAIL with a clear reason rather
     than passing the OPEN through to be CLSE'd by the device.

6. **Coverage = all optional features uniformly** (`shell_v2` + `sync_v2`), since
   a stripped adbd rejects `sync:` for the same reason it rejects `shell,v2`.

## Requirements

* Add `DeviceFeatureSet::from_banner(&str) -> DeviceFeatureSet` parsing the
  `features=` CSV segment of a CNXN banner (handles NUL-termination + empty
  segment), symmetric to `to_banner_string`.
* Parse + store the device-side banner feature set on the persistent connection
  at `do_connect` time (currently only `delayed_ack` is extracted). Expose it
  via a new accessor distinct from the existing `device_features()` (which means
  "what we advertise"); name the new one to clearly mean "peer's advertised set".
* `DeviceEntry` gains `capabilities: Option<DeviceFeatureSet>`; `DeviceEntry::new`
  keeps current behavior (`None`).
* `DefaultDeviceBackend` caches parsed per-serial banner capabilities; fills
  `Some` for TCP (and handshaked USB) entries; implements
  `device_capabilities(serial, timeout)`.
* New `DeviceBackend::device_capabilities(&self, serial, timeout) ->
  Option<DeviceFeatureSet>` with a default `None` impl (back-compat).
* Frontend: serial-aware `host:features` (171, 341) and the `shell,v2`/`sync:`
  gates (797, 806) consult `global_caps ∩ device_caps`. The pre-transport global
  `host:features` (313) keeps replying global caps (no serial yet — unavoidable;
  the post-transport reply is what clients actually gate `shell` on).
* Intersection logic is a single, unit-tested pure function.

## Acceptance Criteria

* [ ] `from_banner` round-trips with `to_banner_string` and handles empty /
      NUL-terminated / unknown-token banners (unit tests).
* [ ] A device whose banner lacks `shell_v2` is **not** offered `shell,v2`:
      post-transport `host:features` omits it, and the `shell,v2` gate FAILs
      cleanly (frontend unit test with a mock backend returning a feature-less
      `DeviceFeatureSet`).
* [ ] A full-feature device is still offered `shell,v2` / `sync:` (no regression).
* [ ] `device_capabilities(serial, timeout)` returns cached `Some` on hit,
      `None` on timeout/not-found.
* [ ] `list_devices` performs **no** new blocking handshake (USB entries may be
      `None`); `track-devices` snapshot cost unchanged.
* [ ] Existing backends compile unchanged (default trait impl).
* [ ] `cargo test` + `cargo clippy --all-targets` green.

## Definition of Done

* Unit tests: banner parser, intersection function, frontend per-device gating
  (mock backend). No dependence on the special hypervisor testbed.
* No public-API break beyond the additive `DeviceEntry` field + new trait method
  with a default impl (note: adding a public struct field is technically a
  breaking change for struct-literal constructors — call this out in the commit;
  `DeviceEntry::new` + `..Default`-style usage stays working).
* Spec: update `server-host-protocol.md` (and/or `adb-wire-protocol-contract.md`)
  to document per-device feature negotiation as the contract.

## Out of Scope

* Bug 1 (TCP_NODELAY) — already fixed (`e90ab60`).
* Implementing `sync_v2`/`cmd` bridging itself — this task only makes the
  *advertising/gating* per-device honest; it does not add new bridged services.
* Reworking `BackendCapabilities` global call — it stays as the
  "backend-can-bridge" half of the intersection; this task adds the device half.
* xdb-side changes — adboost provides the mechanism; xdb adopts it separately.

## Technical Notes

* Intersection semantics: a feature is advertised/allowed for a device iff the
  backend can bridge it (global `BackendCapabilities`) AND the device's banner
  advertised it (`DeviceFeatureSet`). `None` device caps → conservative (treat as
  "unknown" → do not advertise optional features; gate fallback FAILs v2/sync).
* Naming care: keep existing `device_features()` ("what we advertise to the
  device") and add a clearly-named peer accessor to avoid the very confusion the
  bug report fell into.
* Files in play: `models/device_feature_set.rs` (parser), `usb/persistent.rs`
  (store peer banner set + accessor), `server/backend.rs` (DeviceEntry field +
  trait method), `server/default_backend.rs` (cache + impl), `server/frontend.rs`
  (per-device features + gates), `server/capabilities.rs` (intersection helper).

## Open Questions (to resolve during implementation, non-blocking)

* Exact name for the peer-features accessor (e.g. `peer_features()` /
  `negotiated_peer_features()`).
* Whether the intersection helper lives on `ServerCapabilities` or as a free
  function in `capabilities.rs`.
