# Support vsock socket-spec in forward with extensible SocketSpec enum

## Goal

Add vsock remote endpoint support to `adb forward` (e.g. `forward tcp:8885 vsock:2:46668`), matching native adb's capability. Design the implementation around typed `LocalSocketSpec` / `RemoteSocketSpec` enums that make future extensions straightforward without large refactoring.

## Requirements

- `adboost forward add tcp:8885 vsock:2:46668` works end-to-end
- Proper parse errors for malformed vsock specs (e.g. `vsock:abc:123`, `vsock:2` missing port, port overflow)
- Wire format correctness: service string sent to device is exactly `"vsock:<cid>:<port>"`
- ForwardRegistry tracks the full remote spec (for `forward --list` display)
- Extensible enum design — adding a new variant requires: (1) add variant, (2) add parse arm, (3) done
- `killforward` matches by local spec string (`LocalSocketSpec::to_string()`)
- Existing tcp-only forwards continue to work unchanged

## Acceptance Criteria

- [ ] `forward add tcp:0 vsock:2:46668` succeeds (returns allocated port)
- [ ] `forward --list` shows `tcp:XXXX vsock:2:46668` for active vsock forwards
- [ ] `forward add tcp:8885 vsock:abc:123` returns clear parse error
- [ ] `forward add tcp:8885 vsock:2` (missing port) returns clear parse error
- [ ] Existing tcp→tcp forwards continue to work unchanged
- [ ] Adding a hypothetical `localabstract:` variant requires only enum + parse changes, no bridge refactor
- [ ] `cargo clippy` clean, all existing tests pass

## Definition of Done

- Unit tests for `LocalSocketSpec` / `RemoteSocketSpec` parsing (valid + invalid inputs)
- Unit tests for Display output (wire format correctness)
- Existing forward/killforward tests unbroken
- `cargo clippy` / `cargo check` green

## Technical Approach

### Decision (ADR-lite)

**Context**: Forward needs to support non-tcp remote endpoints (vsock now, localabstract/jdwp later). The current `u16`-based ForwardRequest cannot represent these.

**Decision**: Split `LocalSocketSpec` / `RemoteSocketSpec` enums (方案 B) with `RemoteSocketSpec::Display` as the wire format (方案 A for service string generation).

**Consequences**:
- Compile-time guarantee that vsock cannot appear in local position
- `tcp` variant duplicated across both enums (acceptable: they're small enums)
- Future reverse support can reuse `RemoteSocketSpec` for its listen-side spec
- `ADBLocalCommand` gets no new variants; forward bridge uses `ADBLocalCommand::Raw(remote_spec.to_string())`

### Type Design

```rust
// adboost/src/models/socket_spec.rs

/// Host-side listener endpoint (forward LOCAL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSocketSpec {
    Tcp(u16),
    // future: LocalAbstract(String), LocalFilesystem(String)
}

/// Device-side connect endpoint (forward REMOTE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSocketSpec {
    Tcp(u16),
    Vsock { cid: u32, port: u32 },
    // future: LocalAbstract(String), Jdwp(u32), Dev(String)
}
```

Both implement:
- `Display` → wire format string (`"tcp:1234"`, `"vsock:2:46668"`)
- `FromStr` → parse from user/protocol string with clear error messages

### Implementation Plan

1. **New module `models/socket_spec.rs`** — define `LocalSocketSpec`, `RemoteSocketSpec`, `Display`, `FromStr`, unit tests
2. **Refactor `ForwardRequest`** — replace `local_port: u16` / `remote_port: u16` with `local: LocalSocketSpec` / `remote: RemoteSocketSpec`; update `parse_forward` to delegate to `FromStr`
3. **Update `ForwardRegistry`** — key by `LocalSocketSpec` (or resolved port); store `RemoteSocketSpec` for list display
4. **Update bridge in `frontend.rs`** — `run_forward_listener` uses `ADBLocalCommand::Raw(remote.to_string())` instead of `ADBLocalCommand::TcpConnect(port)`
5. **Update `ADBHostCommand::Forward` Display** — use spec `.to_string()` for wire format (should already work if local/remote fields become spec types or remain strings)
6. **Verify CLI** — no changes needed (local/remote are already `String`, parsed server-side)

### Edge Cases

- `vsock:2:0` — pass through, let adbd decide
- CID 0/1 (hypervisor/local) — no restriction, pass through
- `vsock:2` without port → parse error "vsock requires cid and port: vsock:<cid>:<port>"
- Port > u32::MAX → parse error

### Test Strategy

**Key insight**: forward rule registration is lazy — the device-side OPEN only fires when a client connects to the host listener, not at `forward add` time. So vsock control-plane tests work on ANY device (no vsock listener needed).

| Layer | What | How |
|-------|------|-----|
| Parsing | `LocalSocketSpec` / `RemoteSocketSpec` FromStr + Display | Unit tests in `socket_spec.rs` — valid & invalid inputs |
| Control-plane | add/list/remove with vsock remote | Extend existing frontend mock tests (same pattern as tcp tests) |
| Data-plane (new) | Host connect → device OPEN `"vsock:2:46668"` → echo round-trip | **New sim_backend test** with `Scenario::with_echo_bytes()` — verifies correct service string + bridge byte fidelity (also fills the existing tcp forward data-plane gap) |
| CLI selftest | Real device control-plane | Extend `case_forward_control_plane` with a vsock rule (add + appears in list + remove) — works on any device since rule registration is lazy |

## Out of Scope

- localabstract/localfilesystem/localreserved LOCAL endpoint variants
- jdwp/dev/dev-raw remote support
- vsock as LOCAL listener
- reverse vsock support
- Platform gating (adbd rejects on unsupported platforms naturally)

## Research References

- [`research/aosp-socket-spec.md`](research/aosp-socket-spec.md) — AOSP uses string-prefix dispatch with no typed enum; vsock connect = `vsock:<cid>:<port>`; vsock is remote-only for forward
