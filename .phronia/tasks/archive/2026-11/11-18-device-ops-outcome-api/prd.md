# PRD: Device ops first-class API — install options/progress/cancel, structured outcomes

- **Source**: `/private/tmp/adboost-feature-request-xdb-host-ops.md` (xdb / xsdk, 2026-11-18)
- **Kind**: additive client API on `ADBProxyDevice` + shared response seam + additive error variants
- **Priority**: P0 (P1 progress/cancel included; split APK / USB direct are P2, out of scope)
- **Scope**: `adboost` crate (`models/`, `proxy/device_commands/`, `error.rs`) and
  the CLI's exhaustive error classification

---

## 1. Problem

The proxy transport already forwards `root:`/`unroot:`/`remount:`/`exec:` and can
therefore install/root/remount at the wire level. What is missing is **client API
expressiveness**:

1. `install()` cannot pass `-r`/`-g`/`-t`/`-d`, has no progress, no cancellation.
2. `install()`/`uninstall()` read the reply once into 1024 bytes and require an
   exact `b"Success\n"` match — a fragmented or trailing byte misreports failure.
3. `root()`/`unroot()` call `proxy_connection(..., false)`, **throwing the device
   receipt away**; `adbd cannot run as root in production builds` becomes success.
   This is a correctness bug.
4. `remount()` reads once into 1024 bytes and requires the fixed suffix
   `"remount succeeded"`, so multi-line output is truncated and vendor wording fails.
5. Failures are `ADBRequestFailed(String)`; callers re-parse `INSTALL_FAILED_*`
   text, duplicating protocol knowledge downstream (xdb).

## 2. Requirements

### 2.1 Types (public, additive)

- `InstallOptions { replace, grant, test_only, downgrade, user: Option<String> }`
  (`Default` = all false / `None`).
- `InstallProgress { bytes_sent, total_bytes }`.
- `InstallControl { progress: Option<mpsc::Sender<InstallProgress>>,
  cancel: Option<CancellationToken>, progress_interval_bytes: u64 }`
  (`Default` = no progress, no cancel, 1 MiB interval).
- `InstallOutcome { success: bool, package: Option<String>, raw: String }`.
- `RootState { Restarting, AlreadyRoot, Refused, Unknown }` + `RootOutcome { state, raw }`.
- `UninstallOutcome { success, raw }`.
- `RemountOutcome { info: Vec<RemountInfo>, raw }`.

### 2.2 Methods on `ADBProxyDevice`

- `install_with_options(path, &InstallOptions, InstallControl) -> Result<InstallOutcome>`
- `uninstall_with_outcome(package, user) -> Result<UninstallOutcome>`
- `root_with_outcome() -> Result<RootOutcome>`
- `unroot_with_outcome() -> Result<RootOutcome>`
- `remount_with_outcome() -> Result<RemountOutcome>`

Existing `install` / `uninstall` / `root` / `unroot` / `remount` keep their
signatures and become thin wrappers over the new ones.

### 2.3 Semantics

- Install service string, stable order:
  `exec:cmd package 'install' [-r] [-g] [-t] [-d] [--user X] -S <size>`.
- Install: `set_serial_transport` → send request → stream file → read reply to
  EOF with a byte cap (64 KiB) → classify.
- Progress: emit roughly every `progress_interval_bytes`, and always emit the
  final `bytes_sent == total_bytes`; use non-blocking send and drop on a full
  channel (observation only, never blocks install).
- Cancel: stop reading the file, close the connection (device-side install
  aborts best-effort), return `OperationCancelled`. Documented as best-effort.
- Reply classification (install/uninstall): `Success` → outcome;
  `Failure [REASON]` → `InstallFailed`/`UninstallFailed { reason, raw }`;
  anything else → `*Failed { reason: "unknown response", raw }`.
- Root: read the receipt, classify
  `restarting adbd as root` → `Restarting`,
  `adbd is already running as root` → `AlreadyRoot`,
  `adbd cannot run as root in production builds` → `Refused`,
  else `Unknown` (raw preserved). No reconnect-wait logic in adboost.
  `root()` returns `RootRefused` on `Refused`. For `unroot`, "restarting adbd as
  non root" → `Restarting`, "adbd not running as root"/"already running as non
  root" → `AlreadyRoot` (documented as "already in the requested mode").
- Remount: read to EOF (cap), parse `Using … for …` lines; success when a
  success line is present (or `Using` lines exist) and no failure marker
  (`remount failed`, `not running as root`, `permission denied`, …) is present;
  otherwise `RemountError(raw)`. `RemountOutcome.raw` always carries the text.

### 2.4 Error model (additive)

`RustADBError::{InstallFailed { reason, raw }, UninstallFailed { reason, raw },
RootRefused { raw }, OperationCancelled}`. `Result<T>` unchanged.
`adb_cli_error.rs` must classify the new variants (exhaustive match, no `_`).

## 3. Non-goals

- Split APK / `install-multiple` (P2 per the request; separate issue).
- USB-direct `PersistentUsbConnection::install_with_options` (P2).
- Changing `ADBDeviceExt::exec` or adding exit codes to it.
- Waiting for device reconnect after `root` in adboost (caller policy).

## 4. Compatibility

- Methods and types additive; old signatures unchanged.
- `ADBLocalCommand::Install(u64, Option<String>)` is retained as a compatibility
  variant; a new `InstallWithOptions(u64, InstallOptions)` carries the flags.
  Both render through one shared renderer.
- New error variants require downstream `_` arms (accepted minor evolution).
- No new runtime dependencies: enable the existing `tokio-util` `rt` feature for
  `CancellationToken`.

## 5. Acceptance criteria

- [ ] `install_with_options` renders `-r/-g/-t/-d/--user` in stable order with
      correct `-S <size>`.
- [ ] Install reply reading no longer depends on a single 1024-byte read or an
      exact `Success\n` match (fragmentation covered by tests).
- [ ] Install returns `InstallOutcome` or `InstallFailed` with a readable reason.
- [ ] Progress is monotonic and ends at `total_bytes`; cancel returns
      `OperationCancelled` and closes the connection.
- [ ] `uninstall_with_outcome` returns structured success/failure.
- [ ] `root_with_outcome` distinguishes restarting / already / refused; legacy
      `root()` errors on refusal.
- [ ] `remount_with_outcome` reads full (capped) output and classifies failure.
- [ ] Existing APIs still work and delegate to the new ones.
- [ ] Tests cover flag rendering, fragmented replies, progress, cancellation,
      root classification, remount success/failure, uninstall outcome, and
      legacy API behavior.
- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo test` pass; `usb` feature still builds/tests.
- [ ] README/rustdoc documents the new APIs, cancellation semantics, and the
      `exec` boundary.

## 6. Notes

- Keep `prd.md` focused on requirements, constraints, and acceptance criteria.
