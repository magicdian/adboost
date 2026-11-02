# Implement: Typed, negotiated device tracker

## 1. Ordered checklist

1. **Add the shared host-service seam** (`adboost/src/proxy/host_service.rs`).
   - `HostStatus`, `HostServiceError`, `send_request`, `read_status`,
     `FrameDecoder`.
   - `FrameDecoder` owns length/body progress; `read()` only.
   - Tests: complete frame, zero-length frame, partial length/body across a
     cancelled future, truncated EOF, invalid hex.
2. **Refactor `TCPProxyTransport`** to use the seam for request encoding and
   status parsing. Keep its public/crate behavior and error wording.
3. **Add the tracker model layer** (`adboost/src/tracker/models/`).
   - `device_serial.rs`, `connection_state.rs`, `transport_kind.rs`,
     `device_info.rs`, `device_snapshot.rs`, `server_features.rs`,
     `tracker_format.rs`, `tracker_health.rs`.
   - `ReconnectPolicy` / `TrackerConfig` (in `device_tracker.rs` or a model
     file; keep one primary type per file).
   - Inline unit tests for validation, state prefixes, transport inference,
     feature CSV parsing.
4. **Add the codecs** (`adboost/src/tracker/wire/`).
   - `long_text.rs`: serial split, longest-state-prefix, attribute scanner,
     bare bus, CRLF, malformed rows, empty body.
   - `proto.rs`: manual `MessageRead` for `Devices`/`Device`, unknown fields,
     unknown enums, field 10, invalid serial, empty snapshot.
   - `mod.rs`: `decode(format, body)`.
5. **Add `TrackerError`** (`adboost/src/tracker/error.rs`) and conversions from
   `HostServiceError`.
6. **Add `DeviceTracker`** (`adboost/src/tracker/device_tracker.rs`) and its
   `commands/` impls.
   - `connect`, `connect_default`, `connect_with_format`, `connect_with_config`.
   - `next_snapshot`, `format`, `addr`, `health`.
   - Negotiation, proto→long fallback, private `open`/`ensure_connected`.
   - Reconnect/backoff/health transition logic.
7. **Wire the module and exports**.
   - `lib.rs`: `pub mod tracker;`.
   - `tracker/mod.rs`: `#![doc = include_str!("./README.md")]`, module
     declarations, public re-exports.
   - `tracker/README.md` with a short usage example.
8. **Delegate the legacy parsers**.
   - Expose `pub(crate) fn parse_line` from `tracker/wire/long_text.rs`.
   - Rewrite `proxy/models/device_long.rs` and `device_short.rs` to use it.
   - Add `TryFrom<tracker::ConnectionState> for DeviceState`.
   - Keep the existing `DeviceLong` test corpus green.
9. **Add fake-server integration tests** in `device_tracker.rs`.
   - Long-text negotiation + two snapshots + empty snapshot.
   - Proto negotiation + proto decode.
   - Proto advertised but service FAIL → long fallback.
   - Malformed row counted, valid device still present.
   - Clean EOF → `Ok(None)` + `Disconnected`.
   - Frame truncation → reconnect with policy; `Disconnected` published once.
   - Reconnect disabled → error returned.
   - Cancel-safety: drop `next_snapshot` mid-frame, resume.
   - Text/proto contract equality.
10. **Docs/spec**.
    - `.phronia/spec/backend/adb-device-tracker.md` (new).
    - `.phronia/spec/backend/index.md` row.
    - `.phronia/spec/backend/directory-structure.md` module map + parser rule.
    - `.phronia/spec/backend/server-host-protocol.md` cross-reference.
    - `README.md` module row if it has a module table.
11. **Quality gate** and record evidence.

## 2. Validation commands

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo clippy -p adboost --all-features --all-targets -- -D warnings
cargo test -p adboost
cargo test -p adboost --all-features
cargo build -p adboost_cli
RUSTDOCFLAGS='-D warnings' cargo doc -p adboost --all-features --no-deps
```

If repository-wide clippy fails on untouched files (known pre-existing lints
from clippy 1.98 vs the 1.88 baseline), run clippy on the changed targets/files
and record the pre-existing failures separately.

## 3. Review gates

- [x] `FrameDecoder` is the only cancel-safe `%04x` frame reader.
- [x] `TCPProxyTransport` and `tracker` share the request/status/frame seam.
- [x] `DeviceLong`/`DeviceShort` have no duplicated long-text parsing.
- [x] `ServerFeatures` uses `host:host-features`; no code sends
      `host:features` for the tracker.
- [x] AOSP space padding and adboost tabs both decode; `no permissions` maps
      to `NoPermission`.
- [x] Invalid serials are never inserted; `malformed_rows` counts them.
- [x] Proto unknown fields/enums are tolerated.
- [x] `next_snapshot` is cancel-safe; partial frame state survives cancellation.
- [x] Reconnect is bounded/backed off; health transitions are edge-triggered.
- [x] No new dependency, feature, MSRV change, or `RustADBError` variant.
- [x] Existing tests and CLI build are green.

## 4. Verification results

```
cargo fmt --all --check                                    → ok
cargo test -p adboost                                      → 171 passed, 0 failed
                                                             + 6 doctests passed
cargo test -p adboost --all-features                       → 483 passed, 0 failed
                                                             + 8 doctests passed
cargo build -p adboost_cli                                 → ok
clippy, changed files (default + all features)             → zero findings
cargo doc --all-features (RUSTDOCFLAGS=-D warnings)         → changed files zero warnings;
                                                             pre-existing failures in
                                                             untouched files
```

Repository-wide clippy still fails on untouched code (clippy 1.98 vs the 1.88
baseline): `models/socket_spec.rs` dead code, `emulator/adb_emulator_device.rs`
simplification, `models/framebuffer_info.rs` `chunks_exact`, and the
`message_devices/stream/stream_transport.rs` test helpers. These are
pre-existing and not introduced by this task; the changed files are clean.

Rustdoc likewise fails only on pre-existing links/HTML in untouched files
(`server/sim_backend.rs`, `message_devices/usb/persistent.rs`,
`models/adb_local_command.rs`); no tracker/host-service doc warnings.

## 5. Rollback points

- **Before step 3**: the host-service seam is additive; remove the module and
  revert the `TCPProxyTransport` delegation.
- **Before step 8**: the tracker is additive; remove `tracker/` and the
  `lib.rs` declaration.
- **After step 8**: revert `device_long.rs` / `device_short.rs` together with
  the tracker parser; the regex implementations remain in git history.
