# Subtask B — `adboost_cli` rebrand + async migration + persistent USB exerciser

Parent: `06-12-adboost-capability-...`. Depends on **subtask A** (library tracing migration).

## Goal

Rebrand `adb_cli` → `adboost_cli`, migrate it to async against the LOCAL workspace
`adb_client`, add it back to the workspace build/CI, and add a persistent-connection exerciser
subcommand so adboost's async USB path (where #1/#2/#3 lived) can be validated in-tree against
a real device — turning the throwaway `/tmp` diagnostic harness into a permanent, one-command
reproducer + reference implementation.

## Requirements (MVP)

### Rebrand + workspace wiring
- [ ] Rename dir `adb_cli/` → `adboost_cli/`; crate name + binary `adboost_cli`
      (`Cargo.toml` `[package] name`, `[[bin]]` if present, README, `.deb`/`.rpm` asset paths).
- [ ] `adboost_cli/Cargo.toml`: depend on the **local** `adb_client` via
      `{ path = "../adb_client", features = ["mdns","usb"] }` (replace the crates.io `^3.2.1`).
- [ ] Add `adboost_cli` to root `Cargo.toml` `members` (so it builds in CI).
- [ ] Keep `env_logger`? No — switch the CLI to the library's logging story: call
      `adb_client::init_tracing_from_env()` (enable `adb_client`'s `tracing-init` feature) OR
      install `tracing-subscriber` directly in the CLI. Pick one; CLI owns subscriber install
      (library stays emit-only). `RUST_LOG` must work out of the box.

### async migration
- [ ] `main` becomes async (`#[tokio::main]`); all `ADBDeviceExt`/device calls migrated to
      `.await` (`device.shell(...)?` → `device.shell(...).await?`, etc.).
- [ ] All existing subcommands (server/host/local/emu/tcp/usb) compile & run against the async
      `adb_client`. Where an API changed shape in the async rewrite, adapt the call site
      (preserve behavior; do not delete functionality).

### persistent exerciser (the closed-loop value)
- [ ] New subcommand (e.g. `adboost_cli usb-direct …` / `adboost_cli persistent …`) that builds
      a `PersistentUsbConnection` (default features → windowed/delayed_ack path) and runs at
      least `shell <cmd>` (via `shell_exec`/`open_shell_v2`), printing stdout + exit code.
- [ ] **Negotiation self-check** printed by the exerciser: device_version, delayed_ack
      negotiated (local/device/result), the banner sent, and the first frame cmd/arg after
      OPEN (OKAY vs CLSE). This formalizes the `/tmp` harness so the next protocol bug
      reproduces with one command. (Use the public API: `device_features()`, and the
      `subscribe_raw` tee if a raw view is wanted; keep it read-only / non-invasive.)
- [ ] Support choosing classic vs windowed (e.g. a `--no-delayed-ack` flag setting
      `DeviceFeatureSet { delayed_ack:false, .. }`) so the exact #3 control experiment is one flag.

### docs
- [ ] README/spec runbook: prerequisites (kill adb/xdb servers, device over USB), how to run
      the exerciser, `RUST_LOG` examples (incl. per-`local_id` span filter), classic-vs-windowed
      flag. Manual real-device smoke (no automated CI — needs hardware).

## Acceptance Criteria
- [ ] `cargo build`/`clippy --all-targets -- -D warnings`/`test` green for `adboost_cli` in the
      workspace; CI builds both crates.
- [ ] `adboost_cli <exerciser> shell getprop` works against a real device and prints the
      negotiation self-check + getprop output. (Verified on the Android-16 device used this session.)
- [ ] `RUST_LOG=adb_client=debug adboost_cli …` produces attributed library logs.
- [ ] `--no-delayed-ack` reproduces the classic path; default reproduces windowed (now working
      post bug-#3 fix).

## Definition of Done
- Both crates green in CI; CLI runs against real device; runbook documented.
- No loss of existing CLI functionality during the async migration.

## Out of Scope
- New CLI features beyond the exerciser + the async port of existing commands.
- pyadb_client; automated real-device CI; library crate rename.

## Technical Notes
- Current CLI is sync + legacy API: `adb_cli/src/main.rs` (`fn main()`, `ADBUSBDevice`,
  `ADBServerDevice`, `device.shell(...)?` no await), handlers in `adb_cli/src/handlers/`.
- Library helpers: `PersistentUsbConnection::{new_from_ids, new_from_ids_with_features,
  shell_exec, open_shell_v2, open_session, device_features, subscribe_raw}`
  (`persistent.rs`); `ADBLocalCommand::ShellCommand("getprop".into(), vec![])` → `shell:getprop`.
- Device used this session: MediaTek `0e8d:201c`, serial `YTGUSCNFMFAIK7ZP`.
- nusb needs its `tokio` feature for real `connect()` (the diagnostic harness had to add it) —
  check whether `adb_client`'s nusb dep enables it; if the CLI hits the "Awaiting blocking
  syscall without an async runtime" panic, that nusb-feature gap must be fixed (likely in
  `adb_client/Cargo.toml`, coordinate with subtask A / note as a found issue).
