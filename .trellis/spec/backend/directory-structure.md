# Directory Structure

> How code is organized in this Rust workspace (`xp_adb_client`, forked from
> `cocool97/adb_client` at v3.2.2). Documents what the code **actually does**.

---

## Overview

This is a Cargo **workspace** (edition **2024**, MSRV **1.88.0**, resolver `2`).
Active `members` (root `Cargo.toml`): **`adb_client`** + **`adboost_cli`**. Other
crates exist on disk but are excluded from this round's build/CI.

| Crate | Path | Type | Role |
|-------|------|------|------|
| `adb_client` | `adb_client/` | lib | Core ADB client library (async/tokio). Implements both ADB protocols (server + end-device) over USB / TCP. Pure log **emitter** (`tracing`). The bare name `adboost` is **reserved for this library's future rename**. |
| `adboost_cli` | `adboost_cli/` | bin | Async CLI front-end over the **local** `adb_client` (path dep; `mdns` + `usb`). Owns the `tracing` subscriber install. Hosts the `persistent` exerciser (closed-loop validation of the async USB / windowed path). Renamed from `adb_cli` — it no longer tracks upstream's CLI (upstream patches re-import into the library only). |
| `pyadb_client` | `pyadb_client/` | cdylib + rlib | Python bindings via PyO3. On disk; **excluded** from the workspace this round. |
| `mdns` | `examples/mdns/` | bin (example) | Standalone `mdns` + `usb` example. On disk; **excluded** this round. |

Shared package metadata (`authors` / `edition` / `license` / `version` /
`workspace.lints`) lives in `[workspace.package]` at the root `Cargo.toml` and
is inherited via `*.workspace = true`. Do **not** detach a crate from the
workspace (see `upstream-patch-import.md`).

---

## Module declaration convention

**`mod.rs`-per-directory style throughout.** There are zero `<name>.rs` +
`<name>/` sibling pairs (no Rust-2018 mixed style) — every directory module is
declared via its own `mod.rs`. This is a deliberate stylistic choice; edition
2024 does not require it.

When adding a new directory module: create `<dir>/mod.rs`, declare leaf modules
in it, and re-export the public types from there.

---

## `adb_client/src` layout

```
adb_client/src/
├── lib.rs                  crate root + re-exports (#![forbid(unsafe_code)])
├── adb_device_ext.rs       trait ADBDeviceExt  (the high-level device API)
├── adb_transport.rs        trait ADBTransport  (connect/disconnect)
├── error.rs                RustADBError enum + `pub type Result<T>`
├── utils.rs                crate-wide helpers (single file, NOT a dir)
├── emulator/               pub mod  (emulator console over TCP)
├── message_devices/        end-device path (raw ADB wire protocol)
│   ├── adb_message_device.rs          ADBMessageDevice<T: ADBMessageTransport>
│   ├── adb_message_device_commands.rs impl ADBDeviceExt for ADBMessageDevice<T>
│   ├── adb_message_transport.rs       trait ADBMessageTransport
│   ├── commands/                      one ADB op per file (push, pull, shell, ...)
│   ├── models/                        adb_rsa_key.rs → ADBRsaKey
│   ├── usb/                           USBTransport, ADBUSBDevice, persistent.rs
│   └── tcp/                           TcpTransport, ADBTcpDevice  [+ README.md]
├── models/                 data types (selectively re-exported)
├── server/                 talks to the local adb server daemon  [+ README.md]
│   ├── adb_server.rs                  struct ADBServer
│   ├── commands/                      impl ADBServer (connect, devices, ...)
│   └── models/                        device_short, device_long, ...
├── server_device/          a device reached *through* the server  [+ README.md]
│   ├── adb_server_device.rs           struct ADBServerDevice
│   ├── adb_server_device_commands.rs  impl ADBDeviceExt for ADBServerDevice
│   └── commands/                      forward, install, push, send, ...
└── mdns/                   cfg(feature = "mdns") device discovery
```

There is **no `transports/` directory** — each transport file is co-located
with its owning module. There is **no `utils/` directory** in `adb_client/src`
(it is the single file `utils.rs`; nested file-level utils exist, e.g.
`message_devices/utils.rs`, `message_devices/commands/utils/`).

---

## The three-way file split (most important rule)

Definitions, behavior, and trait impls are kept in **separate files**:

1. **Data types → `models/` dirs.** A `models/mod.rs` declares leaf files and
   re-exports types. These files hold the struct/enum plus small parsing/
   `TryFrom` impls. Example: `server/models/device_short.rs` → `DeviceShort`.
2. **Inherent behavior → `commands/` dirs, one operation per file.** Each file
   contains **only `impl` blocks** on the owning device type — no new structs.
   Example: `server/commands/devices.rs:11` → `impl ADBServer { fn devices(...) }`
   (the struct itself is in `server/adb_server.rs`).
3. **Trait implementations → dedicated `*_commands.rs` files**, separate from
   the struct definition file. Example: the `impl ADBDeviceExt for
   ADBServerDevice` lives in `server_device/adb_server_device_commands.rs`, not
   in `adb_server_device.rs`.

When adding a feature: put the type in `models/`, the inherent op in a new
`commands/<verb>.rs`, and wire the trait impl into the existing `*_commands.rs`.

---

## Naming conventions

- **Files: `snake_case`, named after their primary type.** One primary type per
  file. `adb_server.rs` → `ADBServer`, `adb_usb_device.rs` → `ADBUSBDevice`,
  `device_short.rs` → `DeviceShort`, `adb_rsa_key.rs` → `ADBRsaKey`.
- **Command files: verb/feature-named**, one ADB operation each
  (`push.rs`, `pull.rs`, `shell.rs`, `forward.rs`, `tcpip.rs`).
- **Type names: `PascalCase`.** The crate error type is `RustADBError`; the
  crate alias is `Result<T>` (`error.rs:4`).
- **Known prefix inconsistency (document, don't "fix" ad hoc):** both `ADB...`
  (`ADBServer`, `ADBDeviceExt`, `ADBStatResponse`) and `Adb...` (`AdbStatResponse`,
  `AdbVersion`, `AdbRequestStatus`) exist. Match the prefix already used by
  sibling types in the same module rather than introducing a third style.

---

## Device abstraction layering

```
ADBTransport            (adb_transport.rs)            connect / disconnect
   └─ ADBMessageTransport (message_devices/adb_message_transport.rs:12)
        : ADBTransport + Clone + Send + 'static       message read/write w/ timeouts
             └─ ADBMessageDevice<T: ADBMessageTransport>  transport-agnostic device
ADBDeviceExt            (adb_device_ext.rs)            high-level public API
```

`ADBDeviceExt` (the unified public surface: `shell_command`, `shell`, `exec`,
`pull`, `push`, `list`, `reboot`, `install`, `framebuffer`, …) is implemented
for **four** concrete types:

| Concrete type | impl location |
|---|---|
| `ADBMessageDevice<T>` (generic) | `message_devices/adb_message_device_commands.rs:13` |
| `ADBServerDevice` | `server_device/adb_server_device_commands.rs:40` |
| `ADBUSBDevice` | `message_devices/usb/adb_usb_device.rs:111` |
| `ADBTcpDevice` | `message_devices/tcp/adb_tcp_device.rs:37` |

`ADBUSBDevice` / `ADBTcpDevice` wrap `ADBMessageDevice<T>` with a concrete `T`.

---

## Public API re-export idiom (`lib.rs`)

Crate root sets `#![forbid(unsafe_code)]`, doc-includes `../README.md`, and gates
docs.rs cfg. It mixes two exposure styles — match the surrounding style:

- **Namespaced** (`pub mod server`, `pub mod server_device`, `pub mod emulator`,
  `#[cfg(feature = "mdns")] pub mod mdns`) → `adb_client::server::ADBServer`.
- **Flattened** (`pub use message_devices::*`, selective `pub use models::{...}`,
  `pub use adb_device_ext::ADBDeviceExt`, `pub use error::{Result, RustADBError}`)
  → `adb_client::ADBDeviceExt`, `adb_client::usb::ADBUSBDevice`.

Per-leaf idiom: declare leaf modules private (`mod x;`), surface public types via
`pub use x::Type;` in the parent `mod.rs`; use `pub(crate)` for internal
cross-module items. `ADBTransport` is intentionally crate-internal (`use`, not
`pub use`).

---

## `adb_cli/src` layout (mirrors the lib's split)

```
adb_cli/src/
├── main.rs            entry: Opts::parse() → dispatch MainCommand → run_command()
├── adb_termios.rs     cfg(linux|macos) terminal raw-mode helper
├── utils.rs           setup_logger, long_version
├── handlers/          command EXECUTION (handle_host/local/emulator_commands)
└── models/            clap command/arg DEFINITIONS (NOT the lib's models)
    ├── opts.rs        Opts (root Parser), MainCommand enum
    ├── device.rs / host.rs / local.rs / emu.rs / tcp.rs / usb.rs ...
    └── adb_cli_error.rs  ADBCliError + ADBCliResult
```

Arg parsing uses **`clap` (derive feature)**. Command **definitions** live under
`models/`; command **execution** lives under `handlers/` + `run_command` — the
same definitions-vs-behavior split as the library.

---

## Per-module rustdoc via README

`server/`, `server_device/`, and `message_devices/tcp/` pull a sibling
`README.md` into rustdoc via `#![doc = include_str!("./README.md")]` in their
`mod.rs`. Crate roots do the same with `../README.md` (`adb_client/src/lib.rs:6`,
`adb_cli/src/main.rs:1`, `pyadb_client/src/lib.rs:3`).

**Known inconsistency:** `message_devices/usb/README.md` exists but its `mod.rs`
does **not** doc-include it. If you add a new device module with a README,
prefer the doc-include form for consistency.

---

## Common Mistakes

- Putting a struct definition inside a `commands/<verb>.rs` file — those are for
  `impl` blocks only. Data types go under `models/`.
- Adding a `<name>.rs` + `<name>/` pair — breaks the `mod.rs`-only convention.
- Detaching a crate from the workspace (rewriting inherited `*.workspace = true`
  fields) — breaks sibling members and version consistency.
- Introducing a third casing for the `ADB`/`Adb` prefix instead of matching the
  module's existing types.
