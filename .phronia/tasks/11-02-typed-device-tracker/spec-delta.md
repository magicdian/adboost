# Spec Delta

## Target files

- `.phronia/spec/backend/adb-device-tracker.md` (new)
- `.phronia/spec/backend/index.md`
- `.phronia/spec/backend/directory-structure.md`
- `.phronia/spec/backend/server-host-protocol.md`

## Intended changes

- Add `adb-device-tracker.md`: the client-side `adboost::tracker` contract —
  `host:host-features` negotiation, proto-binary vs long-text services,
  AOSP long-text grammar (space padding, tab padding, bare/`usb:` bus,
  `no permissions` help text, `(no serial number)` rejection), the
  `DeviceSerial` invariant, protobuf schema/forward compatibility, snapshot
  boundaries, cancel-safe framing, and reconnect/health semantics.
- `index.md`: add the new guide to the pre-development checklist and the
  guidelines index.
- `directory-structure.md`: replace the stale `server/` + `server_device/`
  tree with the actual `proxy/` client, `tracker/`, and `server/` frontend
  layout; add the shared `proxy/host_service` seam rule and the
  "one model, two codecs, one long-text parser" tracker rule.
- `server-host-protocol.md`: add a client-side counterpart note to the
  device-list family section, pointing at the tracker and restating that the
  server frontend must not advertise `devicetracker_proto_format` until both
  proto services exist.

## Rationale

The tracker is a new client-side wire-family owner. Without a durable spec,
future changes can re-introduce a third long-text parser or conflate
`host:host-features` with the per-transport `host:features`. The directory
tree was also stale relative to the current `proxy`/`server` split, which the
new module makes more visible.

## Verification

- The code task's tests cover the wire formats, serial invariant, negotiation,
  snapshot boundary, cancellation, and reconnect/health.
- The spec text matches the landed module layout and the existing
  `server-host-protocol.md` proto-flag honesty rule.
- No spec file is part of the code commit; this delta is applied after the
  task lands (`phronia task spec plan|commit`).
