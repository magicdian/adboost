# Implement: Device ops first-class API

Ordered checklist. Each step ends with a compile check where practical; the full
gate is at the end. Rollback point: `main` is clean at task creation, so any step
can be reverted with `git checkout -- <files>`.

## 1. Prerequisites

- [x] Create branch `phronia/device-ops-outcome-api` before edits.
- [x] `adboost/Cargo.toml`: `tokio-util` features `["compat", "rt"]`.

## 2. Models (`adboost/src/models/`)

- [x] `install_options.rs`: `InstallOptions` + `InstallOptions::write_flags`
      (stable order `-r -g -t -d --user`), `Debug/Clone/Default/PartialEq/Eq`.
- [x] `install_control.rs`: `InstallProgress`, `InstallControl`
      (progress sender, cancel token, `progress_interval_bytes`; manual `Default`
      with 1 MiB), `DEFAULT_PROGRESS_INTERVAL_BYTES`.
- [x] `install_outcome.rs`, `uninstall_outcome.rs`, `remount_outcome.rs`,
      `root_outcome.rs` (`RootState` + `RootOutcome`).
- [x] `models/mod.rs`: declare + `pub use` all new types.
- [x] `adb_local_command.rs`: add `InstallWithOptions(u64, InstallOptions)`;
      shared renderer used by both install variants; unit tests for exact strings.
- [x] `remount_info.rs`: relax `from_str_response` (success line or `Using` rows,
      no failure marker) and keep `FromStr` for one line.

## 3. Errors

- [x] `error.rs`: add `InstallFailed`, `UninstallFailed`, `RootRefused`,
      `OperationCancelled`.
- [x] `adb_cli/src/models/adb_cli_error.rs`: classify the four (Standard).
- [x] Confirm no `_` arm is added to the exhaustive match.

## 4. Shared response seam

- [x] New `adboost/src/proxy/device_commands/response.rs`:
      `MAX_CONTROL_REPLY_BYTES` (64 KiB),
      `read_reply<R: AsyncRead + Unpin>(reader, Option<&CancellationToken>)`,
      `ReplyKind` / `parse_success_or_failure`,
      `RootDirection` + `classify_root_receipt`,
      `classify_remount_reply`.
- [x] Unit tests with `tokio::io::duplex`: fragmented reply, cap, cancel,
      success/failure/unknown, root matrix, remount success/failure/relaxed.
- [x] `device_commands/mod.rs`: declare `mod response;`.

## 5. Install

- [x] `install.rs`: `install_with_options`:
      open + `.apk` check + size → `set_serial_transport` →
      `InstallWithOptions` → `stream_apk` → `read_reply` → classify.
- [x] `stream_apk` generic helper: chunked write, progress at interval + final,
      `try_send` (drop on full), cancel via `select!`.
- [x] On cancel: shutdown the raw connection, return `OperationCancelled`.
- [x] Legacy `install()` delegates with `InstallOptions { user, ..default }`.
- [x] Tests: progress monotonic/final; cancel mid-stream.

## 6. Uninstall / root / unroot / remount

- [x] `uninstall_with_outcome` + legacy `uninstall()` delegation.
- [x] `root_with_outcome` / `unroot_with_outcome` read the receipt
      (`read_reply`) and classify; legacy `root()`/`unroot()` error on `Refused`.
- [x] `remount_with_outcome` reads to EOF and returns `RemountOutcome`; legacy
      `remount()` delegates and returns `.info`.

## 7. Integration tests (fake adb server)

- [x] In-process `tokio::net::TcpListener` on `127.0.0.1:0` speaking the `%04x`
      host protocol: `host:transport:*` → OKAY, then a canned per-service reply.
- [x] Cover: install success, install failure (`INSTALL_FAILED_*`), install flags
      observed on the wire, uninstall success/failure, root restarting/refused,
      remount success/failure, legacy `install/root/remount/uninstall` paths.

## 8. Docs

- [x] `adboost/src/proxy/README.md`: new APIs, progress/cancel semantics,
      `exec` boundary (raw stream, no exit code, no re-implementation downstream).
- [x] rustdoc on every new public item; document best-effort cancellation and
      "no reconnect wait" for root.

## 9. Quality gate

- [x] `cargo fmt --all --check`
- [x] `cargo clippy --all-targets -- -D warnings`
- [x] `cargo test`
- [x] `cargo clippy -p adboost --features usb --all-targets -- -D warnings`
- [x] `cargo test -p adboost --features usb`
- [x] `cargo build -p adboost_cli` (dependent still builds)

## 10. Wrap-up

- [x] Update `.phronia/spec/backend/` docs where behavior contracts changed
      (directory structure / error handling entries for the new seam).
- [x] Commit with the task path reference; archive per `finish-work`.
