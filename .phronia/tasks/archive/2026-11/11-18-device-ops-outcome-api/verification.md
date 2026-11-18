# Real-device verification — 2026-11-18

Probe: `adboost/examples/device_ops_probe.rs` (proxy API, same path xdb uses).

## Device

- Serial `YTGUSCNFMFAIK7ZP`, model `d03` / product `XCCP_M_Global` (mtk8676),
  Android 16 (SDK 36), `ro.build.type=userdebug`, `test-keys`, `ro.debuggable=1`.
- Host adb server 1.0.41 (35.0.2), device clock reads 2024-02-29 (stale clock —
  explains package timestamps).

## Results

| Probe | Result |
|---|---|
| `remount` before root | `RemountError("Not running as root. Try \"adb root\" first.")` — structured failure |
| `uninstall com.adboost.definitely.not.installed` | `UninstallFailed { reason: DELETE_FAILED_INTERNAL_ERROR, raw }` |
| `root` | `RootState::Restarting`, raw `restarting adbd as root\n`; transport id 23 → 24; `adb shell id` = uid 0 |
| `root` again | `RootState::AlreadyRoot`, raw `adbd is already running as root\n` |
| `remount` as root | success; 8 `Using overlayfs …` entries; raw ends with `Remount succeeded` + `Now reboot your device …` |
| `install app-debug_1_signed.apk` (no flags) | `Success`; progress 4.26 / 9.50 / 14.75 / 19.99 / 24.43 MB, final == total |
| `install` again (no `-r`) | device returned `Success`; system `adb install` behaves identically on this Android 16 build (same-version reinstall accepted), so not an API defect |
| `install --replace --grant --user 0` | `Success` (flags accepted on the wire) |
| `uninstall com.xiaopeng.statusbartest` | `Success`; `pm list packages -3` back to 0 |
| `unroot` | `RootState::Restarting`, raw `restarting adbd as non root\n`; `adb shell id` back to uid 2000 |

Notable: the remount reply would have failed the **old** parser — it required the
body to *end* with `"remount succeeded"`, but this device appends
`Now reboot your device for settings to take effect`. The relaxed classifier
accepted it and preserved the full text.

## Cleanup / state

- Test package uninstalled; third-party package count restored to 0.
- adbd returned to non-root.
- Overlayfs mounts created by `remount` remain until the next reboot (normal
  remount behavior, not an adboost side effect).

## Not exercised on device

- Cancellation (unit-tested with `tokio::io::duplex`); aborting a live 24 MB
  install would leave device-side state to clean up.
- Split APK and USB-direct paths (P2, intentionally out of scope).
- `adboost_cli` still calls the legacy `install/uninstall/root/remount`; the new
  option/outcome APIs are library-only, which is why this probe exists.
