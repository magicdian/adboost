# Logging Guidelines

> How logging is done in `xp_adb_client` (adboost). The library emits via
> **`tracing`** (migrated from `log` — subtask `06-12-lib-tracing-migration`);
> the binary owns subscriber installation.

---

## Overview

The library uses the **`tracing` crate** (emit-only) with **span-based per-session
attribution**. A concrete subscriber is installed only by the binary / downstream
consumer — never by the library.

| Crate | Logging deps |
|---|---|
| `adb_client` (lib) | `tracing = { features = ["log"] }` — emits events + spans, **never** installs a subscriber. `tracing-subscriber` is an **optional** dep, gated behind the off-by-default `tracing-init` feature (powers the `init_tracing_from_env()` convenience helper only). |
| `adboost_cli` (bin) | owns subscriber install (via `adb_client::init_tracing_from_env()` or its own `tracing-subscriber`). |
| `pyadb_client` | no logging deps. |

**Backward compatibility**: `tracing`'s `log` feature makes every event ALSO emit
a `log` record, so consumers wired only with `env_logger` still see output. Some
deps still route their own logs through `log` (`rustls`, `mdns-sd`, `tokio-rustls`
`logging` feature) — a `tracing` consumer can pick those up via `tracing-log`.

---

## Subscriber Initialization (binary only — library stays emit-only)

The **library must never install a subscriber** (`set_global_default`,
`tracing_subscriber::fmt().init()`, `env_logger::init()`, …). It only emits. A
library that installs a subscriber steals the process-global slot from the binary
and every other consumer.

The library exposes ONE optional convenience initializer, feature-gated and OFF by
default (`adb_client/src/lib.rs`):

```rust
#[cfg(feature = "tracing-init")]
pub fn init_tracing_from_env() {
    use tracing_subscriber::{EnvFilter, fmt};
    let _ = fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init(); // never panics; no-op if a subscriber already exists
}
```

`try_init` ensures it never fights a consumer-installed subscriber. The default
build pulls in only the tiny `tracing` emit crate (no subscriber).

### Activating logs at runtime (no rebuild)

With a subscriber installed, `RUST_LOG` (`EnvFilter`) selects output — including
**per-span / per-field** filtering that plain `log` cannot do:

- `RUST_LOG=adb_client=trace` — whole crate.
- `RUST_LOG=adb_client::message_devices::usb::persistent=trace` — just the USB multiplexer.
- `RUST_LOG=[reader]=trace` / `[writer]=trace` — just the reader / writer task.
- `RUST_LOG=[session{local_id=...}]=trace` — only one session (per-`local_id` attribution).
- `RUST_LOG=adb_client=info,[session]=debug` — combine.

---

## Spans (async per-session attribution)

adboost is async/multi-task (concurrent reader + writer tasks, many multiplexed
sessions). Flat log lines from interleaved tasks can't be tied to a session —
this made bugs #1/#2/#3 hard to diagnose. **Spans fix this**: hot paths carry
context (esp. `local_id`) so every event emitted under a span inherits it.

Span sites (`persistent.rs`): `do_connect` ("connect"), `do_auth` ("auth"),
`reader_loop` ("reader"), `writer_loop` ("writer"), `open_session`
("session", field `local_id`), `open_shell_v2`, `open_sync_session`.

> **CRITICAL — async span rule**: on an `async fn`, use
> `#[tracing::instrument(...)]` (it instruments the returned *future*, entered/
> exited around every `.await`). **NEVER** hold a synchronous `span.enter()` RAII
> guard across an `.await` — at a yield the span leaks onto whatever task runs
> next on that thread and is not correctly re-entered on resume. If a span field
> (like `local_id`) is computed inside the body, declare it empty in
> `#[instrument(..., fields(local_id))]` and fill it with
> `tracing::Span::current().record("local_id", local_id);`. A sync `.enter()`
> guard is only correct in synchronous code (e.g. a non-async unit test with no
> awaits in scope).

---

## Macro Style

**Always use the fully-qualified `tracing::<level>!` form. Do not `use tracing::...`.**
Every call site writes `tracing::debug!`, `tracing::info!`, etc. (the `log` → `tracing`
migration kept this fully-qualified convention; there are zero `use tracing::` macro
imports). The macro call shape is identical to `log`'s.

Use **inline format-arg captures** (the dominant style):

```rust
tracing::warn!("No private key found at {}. Generating random.", path);  // positional for expressions
tracing::error!("error while starting adb server: {e}");                 // inline capture
tracing::info!("Package {package_name} successfully uninstalled");       // inline capture
```

Prefer inline capture (`{e}`, `{package_name}`) when the value is a plain
binding; use positional args when the value is a method-call/expression.

**Lazy args**: `tracing` checks the level before evaluating macro args, so
expensive arg expressions (e.g. `String::from_utf8_lossy(...)`) are free when the
level is disabled — *as long as they are written INSIDE the macro call*, not
pre-computed into a binding first. Keep hot per-frame `trace!` args inline.

**Never use `println!` / `eprintln!` / `print!` for logging in library code** —
zero such calls exist in `adb_client/`. (`eprintln!` appears only in
`benches/benchmark_adb_push.rs`, which is acceptable for a bench harness.)

---

## Log Levels (observed convention)

| Level | Use for | Examples |
|---|---|---|
| `trace!` | Low-level wire/transport detail: byte counts, packet headers, per-message routing. | `usb_transport.rs:169` `"wrote chunk of size {write_amount} - {offset}/{data_len}"` |
| `debug!` | Protocol/state transitions: TLS upgrade, auth-required, endpoint discovery, thread lifecycle. | `adb_message_device.rs:75` `"Connection successfully upgraded from TCP to TLS"` |
| `info!` | User-facing success/status outcomes. **In the CLI, `info!` IS the program's stdout** — CLI output goes through `tracing::info!`. | `server_device/commands/uninstall.rs:25` |
| `warn!` | Recoverable / degraded conditions. | `"No private key found at {}. Generating random."` |
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

- Installing a subscriber (`tracing_subscriber::*::init()`, `set_global_default`,
  `env_logger::init()`) from the **library** — only the binary may. The library
  stays emit-only (the one exception is the opt-in `tracing-init` helper, which
  uses `try_init`).
- **Holding a sync `span.enter()` guard across `.await`** in an `async fn` — use
  `#[tracing::instrument]` instead (see the async span rule above). This is the
  highest-impact mistake: it silently misattributes events across tasks.
- Adding `tracing-subscriber` (or any subscriber) to `adb_client`'s **default**
  dependencies — it must stay `optional` behind `tracing-init`.
- Using `println!`/`eprintln!` for output in library code — use the `tracing`
  macros (CLI output goes through `tracing::info!`).
- Importing `use tracing::info;` then calling bare `info!` — the convention is the
  fully-qualified `tracing::info!` form.
- Pre-computing an expensive log arg into a binding before the macro (defeats
  level-gating) — keep it inline in the `trace!`/`debug!` call.
- Logging at the wrong level (e.g. per-byte wire detail at `debug!` instead of
  `trace!`).
