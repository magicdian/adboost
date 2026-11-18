# Design: Device ops first-class API

## 1. Layering

The change sits entirely in the proxy client layer. Each concern gets exactly one
home, so future device-ops work (split APK, USB direct) extends the same seams:

```
models/                          data types + wire rendering
  install_options.rs               InstallOptions (+ flag rendering, one source)
  install_control.rs               InstallControl, InstallProgress
  install_outcome.rs               InstallOutcome
  uninstall_outcome.rs             UninstallOutcome
  root_outcome.rs                  RootState, RootOutcome
  remount_outcome.rs               RemountOutcome

proxy/device_commands/
  response.rs (new)                shared seam: capped + cancel-aware reply read,
                                   install/uninstall outcome parsing, root and
                                   remount classification (pure functions)
  install.rs                       install_with_options (stream+progress+cancel)
                                   legacy install()
  uninstall.rs                     uninstall_with_outcome + legacy uninstall()
  root.rs / unroot.rs              *_with_outcome + legacy wrappers
  remount.rs                       remount_with_outcome + legacy remount()

error.rs                          4 additive domain variants
adb_cli_error.rs                  exhaustive classification arms
```

`models/` holds no I/O. `device_commands/response.rs` holds I/O + pure parsing but
no device mutation, and is the only place reply grammar lives. Each operation file
keeps only its own flow (single responsibility).

## 2. Wire contract

### 2.1 Service strings

`ADBLocalCommand` gains `InstallWithOptions(u64, InstallOptions)`. Rendering:

```
exec:cmd package 'install' [-r] [-g] [-t] [-d] [--user X] -S <size>
```

`ADBLocalCommand::Install(size, user)` stays for compatibility and delegates to
the same renderer by converting into `InstallOptions { user, ..default }`.
Flag order is fixed by construction (a single `write_install_flags` helper), so
tests assert the exact string.

### 2.2 Reply reading

One helper, `response::read_reply(reader, cancel)`:

- loops `read` into a growable buffer;
- stops at EOF (`Ok(0)`) or at `MAX_CONTROL_REPLY_BYTES` (64 KiB), never a single
  1024-byte read;
- when a `CancellationToken` is supplied, each read races `cancelled()` in a
  `select!` (both branches cancel-safe) and returns `OperationCancelled`;
- returns `String` via `from_utf8_lossy`, so non-UTF-8 device noise is preserved
  lossily instead of failing the operation.

The reader is generic over `AsyncRead + Unpin` so fragmented replies are testable
with `tokio::io::duplex` and no network.

### 2.3 Parsing

`parse_success_or_failure(raw) -> Option<ReplyKind>` in `response.rs`:

- trims trailing whitespace; scans lines;
- `Success…` line → success;
- `Failure [REASON]` line → failure with `REASON` (brackets stripped);
- other → unknown response.

Callers map the result to the typed outcome or `InstallFailed`/`UninstallFailed`.
`package` is left `None` unless a reliable field appears in the reply (the current
`cmd package install` reply does not carry one).

## 3. Install flow (`install_with_options`)

```
open file + check .apk extension + metadata.len()
set_serial_transport()
send InstallWithOptions(size, options.clone())
write loop: read chunk -> cancel check -> write_all -> accumulate progress
  every progress_interval_bytes (or at EOF) -> try_send(InstallProgress)
read_reply(transport, cancel)         // cap + cancel
parse -> InstallOutcome / InstallFailed
```

- The write loop is a standalone generic async fn (`stream_apk`) over
  `AsyncRead`/`AsyncWrite` + `InstallControl`, so progress monotonicity and
  cancellation are unit-testable with `duplex`.
- Progress uses `try_send`; `Full`/`Closed` are ignored (observation channel never
  blocks or fails the install). The final progress is always attempted with the
  exact `total_bytes`.
- Cancellation: the loop returns `OperationCancelled`; the caller closes the
  socket (`take_raw_connection()?.shutdown()`) so the device sees EOF and aborts
  `cmd package install`. Documented best-effort: the device may have completed.
- Legacy `install()` builds `InstallOptions { user, ..Default }` and discards the
  outcome, preserving its contract.

## 4. Root / unroot flow

```
set_serial_transport()
send Root/Unroot request (OKAY consumed by send_adb_request)
read_reply(transport, None)           // receipt is *not* discarded
classify_root_receipt(raw, direction) -> RootState
```

Classification is a pure function with a `RootDirection` parameter (`Root` /
`Unroot`) so the wording for unroot is not forced through root-only branches:

| receipt | state |
|---|---|
| `restarting adbd as root` / `restarting adbd as non root` | `Restarting` |
| `adbd is already running as root` / `adbd is already running as non root` / `adbd not running as root` | `AlreadyRoot` (already in requested mode) |
| `adbd cannot run as root in production builds` | `Refused` |
| empty / unrecognized | `Unknown` |

`root()` keeps its signature but returns `RootRefused { raw }` when state is
`Refused` (the documented correctness fix); `Unknown` stays `Ok(())` to avoid
breaking devices that emit unexpected but non-refusing text (logged at WARN).

## 5. Remount flow

`remount_with_outcome` reads to EOF (cap), then:

- parse every `Using <path> for <mode>` line via the existing `RemountInfo::from_str`;
- failure markers (case-insensitive): `remount failed`, `not running as root`,
  `permission denied`, `device is not unlocked`, `no such file`;
- success if no failure marker and (a success marker such as `remount succeeded`
  is present **or** at least one `Using` line parsed);
- otherwise `RemountError(raw)`.

`RemountInfo::from_str_response` is relaxed to the same rule so the legacy
`remount()` and the new API share one classifier. `RemountOutcome.raw` keeps the
device text for diagnostics.

## 6. Error model

Additive variants on `RustADBError`:

```rust
#[error("install failed: {reason} (raw: {raw})")]
InstallFailed { reason: String, raw: String },
#[error("uninstall failed: {reason} (raw: {raw})")]
UninstallFailed { reason: String, raw: String },
#[error("adb root refused by device: {raw}")]
RootRefused { raw: String },
#[error("operation cancelled")]
OperationCancelled,
```

All four are user-facing/standard conditions → classified `Standard` in
`adb_cli_error.rs` (exhaustive match, no `_` arm).

## 7. Dependencies

`tokio-util` already exists with `compat`; add `rt` (the feature that exports
`CancellationToken`). No new crates, no new runtimes.

## 8. Testing strategy

Inline `#[cfg(test)] mod tests` per file:

- `adb_local_command.rs`: exact flag rendering / order / legacy variant parity.
- `response.rs`: fragmented replies (`duplex`, write in two halves), 64 KiB cap,
  failure reason extraction, unknown reply, root/unroot classification matrix,
  remount success/failure/relaxed wording.
- `install.rs`: `stream_apk` progress monotonicity + final value; cancel mid
  stream returns `OperationCancelled` and stops writing.
- `proxy/device_commands/mod.rs` (or `install.rs`): in-process fake adb server on
  `127.0.0.1:0` speaking `%04x` host requests, used to drive the real
  `ADBProxyDevice` for install success/failure, uninstall, root, remount, and the
  legacy wrappers. Loopback only, no hardware, no new dependency.

## 9. Rollout / compatibility

Additive minor release. Downstream xdb switches to the new methods and keeps
compiling against old ones. If a downstream constructs
`ADBLocalCommand::Install(size, user)`, it still works; new code uses
`InstallWithOptions`. No wire-protocol change (server frontend already bridges
`exec:` verbatim).

## 10. Tradeoffs

- Two `ADBLocalCommand` install variants: a small amount of enum surface in
  exchange for a non-breaking change; both share one renderer, so no divergence
  risk.
- No response timeout on the reply read: byte cap prevents unbounded memory; a
  device that never closes still blocks, matching existing proxy behavior. A
  timeout knob is left to a future transport-level change.
- Root `Unknown` remains `Ok(())` on the legacy API to avoid regressions, while
  the new API always exposes the state.
