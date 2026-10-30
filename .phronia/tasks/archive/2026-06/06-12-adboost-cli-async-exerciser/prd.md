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
      `.await`. Device CONSTRUCTORS are now async too (`ADBUSBDevice::new/autodetect/...`,
      `ADBTcpDevice::new...`) → `.await` them.
- [ ] **dyn-dispatch rebuild (architectural):** `ADBDeviceExt` is now async (AFIT +
      `trait_variant::make(Send)`) and **NOT dyn-compatible** — `boxed()` / `Box<dyn
      ADBDeviceExt>` were removed. The CLI currently funnels all device types into
      `Box<dyn ADBDeviceExt>` → one `run_command`. **Decision: generic-ize**
      `run_command` to `async fn run_command<D: ADBDeviceExt>(mut device: D, cmd: DeviceCommands)`
      and call it from each `main` match arm (usb / tcp / server) with the concrete device
      type. No new deps, compile-time monomorphized, idiomatic. The arms can no longer share a
      single `Box` variable — restructure so each branch calls `run_command(concrete, cmd).await`.
      (Rejected: `dynosaur` — extra dep + would touch the library trait; concrete enum — manual
      dispatch boilerplate.)
- [ ] **Byte-stream bridging:** `ADBDeviceExt` stream params are now
      `tokio::io::{AsyncRead, AsyncWrite}` (+ `Pin<Box<dyn AsyncWrite + Send>>` for `shell`).
      Bridge sync `File`/stdin/stdout via `tokio::fs::File` / `tokio::io::{stdin,stdout}` or
      `tokio_util::compat`. Pull/Push file I/O → `tokio::fs`.
- [ ] Server-side ops (`ADBServer`, `ADBServerDevice` host_features/get_logs/forward/reverse,
      `handle_host_commands`/`handle_local_commands`/`handle_emulator_commands`) appear to remain
      sync — keep them sync where the library API is still sync; only add `.await` where the
      called API is actually async. Verify per-call.
- [ ] All existing subcommands (server/host/local/emu/tcp/usb/mdns/version) compile & run.
      Preserve behavior; do not delete functionality.

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
- **CONFIRMED nusb-feature gap (prerequisite):** `adb_client/Cargo.toml:76` declares
  `nusb = { version = "0.2", optional = true }` WITHOUT its `tokio` feature. Real USB
  `connect()` then panics: "Awaiting blocking syscall without an async runtime: enable the
  `smol` or `tokio` feature of nusb." (Hit this session in the diagnostic harness.) The
  persistent exerciser exercises real USB, so this MUST be fixed: change to
  `nusb = { version = "0.2", features = ["tokio"], optional = true }` in `adb_client`. This is
  a real library bug (the async USB path can't actually connect without it) — fix it here as
  part of B (or note it loudly). Verify the legacy `ADBUSBDevice` path and the persistent path
  both connect after the fix.
- Confirmed migration facts: `ADBUSBDevice::{new,autodetect,...}` are async; all three device
  types impl async `ADBDeviceExt`; `Box<dyn ADBDeviceExt>` removed; ~17 device-method call
  sites + the constructors need `.await`; `run_command` takes `Box<dyn ADBDeviceExt>` today.
