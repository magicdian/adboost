# Logging Guidelines

> How logging is done in `xp_adb_client`. Documents the actual conventions
> observed across the library and CLI (87 call sites).

---

## Overview

The project uses the **`log` facade crate** (not `tracing`), with **`env_logger`**
as the concrete backend in the CLI only.

| Crate | Logging deps |
|---|---|
| `adb_client` (lib) | `log = "0.4"` only — emits records, **never** initializes a logger (correct library practice). |
| `adb_cli` (bin) | `log` + `env_logger = "0.11"` — owns logger init. |
| `pyadb_client` | neither — no logging deps. |

Some dependencies route their own logs through `log` via their `logging` feature
(`rustls`, `mdns-sd`).

---

## Logger Initialization

**Single init point**, in the CLI only — `adb_cli/src/utils.rs:6-9`:

```rust
pub fn setup_logger(debug: bool) {
    Builder::from_env(Env::default().default_filter_or(if debug { "debug" } else { "info" }))
        .init();
}
```

- Honors `RUST_LOG` if set; otherwise falls back to `debug` (when the `--debug`
  CLI flag is passed) or `info`.
- The **library must never call `.init()`** — it only emits via the `log`
  macros and lets the binary configure the backend.

---

## Macro Style

**Always use the fully-qualified `log::<level>!` form. Do not `use log::...`.**
There are zero `use log::` imports in the codebase — every call site writes
`log::debug!`, `log::info!`, etc.

Use **inline format-arg captures** (the dominant style):

```rust
log::warn!("No private key found at {}. Generating random.", path);  // positional for expressions
log::error!("error while starting adb server: {e}");                 // inline capture
log::info!("Package {package_name} successfully uninstalled");       // inline capture
```

Prefer inline capture (`{e}`, `{package_name}`) when the value is a plain
binding; use positional args when the value is a method-call/expression
(`adb_message_device.rs:124-127`).

**Never use `println!` / `eprintln!` / `print!` for logging in library code** —
zero such calls exist in `adb_client/`. (`eprintln!` appears only in
`benches/benchmark_adb_push.rs`, which is acceptable for a bench harness.)

---

## Log Levels (observed convention)

| Level | Use for | Examples |
|---|---|---|
| `trace!` | Low-level wire/transport detail: byte counts, packet headers, per-message routing. | `usb_transport.rs:169` `"wrote chunk of size {write_amount} - {offset}/{data_len}"` |
| `debug!` | Protocol/state transitions: TLS upgrade, auth-required, endpoint discovery, thread lifecycle. | `adb_message_device.rs:75` `"Connection successfully upgraded from TCP to TLS"` |
| `info!` | User-facing success/status outcomes. **In the CLI, `info!` IS the program's stdout** — all CLI output goes through `log::info!`. | `server_device/commands/uninstall.rs:25`; `adb_cli/src/handlers/host_commands.rs:14-82` |
| `warn!` | Recoverable / degraded conditions. | `persistent.rs:61` `"No private key found at {}. Generating random."` |
| `error!` | Failures, often terminal. | `adb_server.rs:72` `"error while starting adb server: {e}"`; CLI top-level `main.rs:116` |

---

## Message conventions

- **Error messages: lowercase, "error while …" prefix.** e.g.
  `"error while starting adb server: {e}"`, `"got error with device: {e}"`
  (`adb_server.rs:72,75`; `mdns/mdns_discovery.rs:44`; `usb_transport.rs:208,216`).
- **Subsystem tag prefix** for the persistent-USB multiplexer: messages are
  prefixed `"PersistentUsb: ..."` / `"PersistentUsb reader: ..."`
  (`persistent.rs:61,111,...`). If you add a new long-lived background
  subsystem, follow this tag convention.
- **CLI output uses `info!`**, not `println!` — keep that so output respects the
  configured log filter.

---

## What NOT to Log

- No secrets / private key material. Note the auth flow only logs the *path* to
  the private key and the fact a random one is being generated
  (`persistent.rs:61`), never the key bytes.
- No raw payload contents beyond size/diagnostic metadata at `trace!`.

---

## Common Mistakes

- Calling `env_logger`/`.init()` from the **library** — only `adb_cli` may
  initialize a logger.
- Using `println!`/`eprintln!` for output in library code — use the `log` macros
  (CLI output goes through `log::info!`).
- Importing `use log::info;` then calling bare `info!` — the convention is the
  fully-qualified `log::info!` form.
- Logging at the wrong level (e.g. per-byte wire detail at `debug!` instead of
  `trace!`).
