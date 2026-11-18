# Spec delta: device ops first-class API

Updated `.phronia/spec/backend/`:

## `directory-structure.md`

- Added **"Device control services share one reply seam"** under the proxy
  section: `proxy/device_commands/response.rs` owns the capped/cancellable reply
  read, the `Success`/`Failure [REASON]` classifier, and root receipt
  classification; remount markers stay with `models/remount_info.rs`. This
  prevents a future control-service command from re-deriving a single 1024-byte
  read or an exact `"Success\n"` comparison.
- Added `response.rs` to the `adboost/src` module tree and recorded the typed
  outcome method surface (`install_with_options`, `uninstall_with_outcome`,
  `root_with_outcome`, `unroot_with_outcome`, `remount_with_outcome`).

## `error-handling.md`

- Added **"Device operation outcomes"**: the four additive variants
  (`InstallFailed`, `UninstallFailed`, `RootRefused`, `OperationCancelled`),
  which layer produces each, the `Standard` CLI classification, and the
  best-effort cancellation contract. Also records that `RemountError(String)`
  keeps carrying the full device reply.

## Not changed

- No new feature flag or runtime dependency (only the existing `tokio-util`
  `rt` feature for `CancellationToken`).
- `ADBDeviceExt` is unchanged: the new methods are inherent to
  `ADBProxyDevice`, so other device backends are not forced to implement them.
