# Research: track-devices-l wire contract & codebase touch points

## AOSP wire contract (from xdb report + SERVICES.TXT)

- `host:track-devices-l`: OKAY, then `%04x`-framed long-format listing pushed
  on subscribe and on every device-set change; connection stays open.
- Long-format line = exactly what `host:devices-l` renders:
  `<serial>\t<state>[ key:value …] transport_id:<N>` (tab-separated first two
  columns; adblib `DeviceListTextParser(LONG_FORMAT)` tolerates missing
  product/model/device keys).
- AS adblib `SessionDeviceTracker.pickBestFormat`: `host:features` contains
  `devicetracker_proto_format` → `track-devices-proto-binary`, else →
  `track-devices-l`. **No FAIL fallback** — a missing service = empty device
  list in AS. (We must NOT advertise the proto feature without implementing
  the proto services.)

## Codebase facts (verified by reading, rev f936b67)

- `frontend.rs:1126 serve_track_devices` — writes OKAY, then loops
  `backend.subscribe_changes()` rendering `format_devices(&snapshot, false)`
  per frame. Parameterizing this + `format_devices` covers R1.
- `frontend.rs:1515 format_devices(devices, long: bool)` — long arm already
  renders product/model/device/transport_id; reused verbatim.
- Dispatch: `frontend.rs:313 match svc { "track-devices" => … }` — add an exact
  `"track-devices-l"` arm. Streaming services stay OUT of
  `host_data_query_payload` (that funnel is one-shot data queries only).
- `protocol::encode_framed` handles the `%04x` frame; oversized snapshot →
  warn + skip (existing behavior, unchanged).
- `DefaultDeviceBackend::subscribe_changes` (default_backend.rs:617) sends the
  initial snapshot immediately, then a full snapshot per nusb hotplug event;
  falls back to one-shot-close when hotplug is unavailable. → real-device
  initial-snapshot case is deterministic; unplug/replug produces new frames.
- Unknown-service FAIL sites (no logging today): frontend.rs ~289
  (`unknown service`), ~389 (`unknown host service`), ~552 (`unknown
  host-serial sub-service`), serve_local_service map-reject (~1184, reason may
  be rewritten by `local_service_reject_reason` hook — the frontend decision is
  still the thing to log), serve_reverse tail (~1346).
- `TcpStream::peer_addr()` is available at all five sites; use it for the log
  (failure-only path, cheap).
- Test infra: `MockBackend` + `round_trip` (reads to EOF — works for
  track-devices because MockBackend's `subscribe_changes` sends one snapshot
  then drops the sender → stream closes → EOF). No existing unit test covers
  `track-devices` itself or the unknown-service wording — add both.
- Real-device infra: `adboost_cli/src/selftest/` — `InProcessServer`
  (channels.rs, ephemeral port, DefaultDeviceBackend), `parity.rs` (official
  adb CLI as client; addr+serial case fns), `interactive.rs` (operator-prompted
  unplug/replug cases, `case_usb_forward_release_on_unplug` is the model),
  `mod.rs::run_through_server_phase` (wiring; runs once per run for
  whole-server cases; per-serial otherwise).
- Logging style: fully-qualified `tracing::warn!`, lowercase, warn =
  recoverable/degraded. No log-assertion tests exist (no subscriber machinery
  in unit tests) → make the log line a pure fn and unit-test that.

## Verification probes (from the xdb report)

```bash
# protocol-level, no device needed
python3 - <<'EOF'
import socket
def probe(svc):
    s = socket.create_connection(("127.0.0.1", 5037), timeout=2)
    s.sendall(("%04x" % len(svc)).encode() + svc.encode())
    status = s.recv(4); ln = s.recv(4)
    n = int(ln, 16); buf = b""
    while len(buf) < n:
        c = s.recv(n - len(buf))
        if not c: break
        buf += c
    print(svc, status, buf[:200]); s.close()
probe("host:track-devices-l")
EOF
```

Client-level regression set: `adb devices`, `adb devices -l`, `adb shell`,
scrcpy (legacy track-devices path), fresh Android Studio (should now list
devices and follow plug/unplug).
