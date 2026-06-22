# Error Handling

> How errors are handled across the three crates of `xp_adb_client`
> (`adboost` core lib, `adb_cli` binary, `pyadb_client` PyO3 bindings).
> Each layer has a distinct, deliberate strategy.

---

## Overview

- **Core lib (`adboost`):** one `thiserror` enum `RustADBError` + a crate
  `Result<T>` alias. Foreign errors auto-convert via `#[from]`; domain errors
  are named variants with `{0}` format strings.
- **CLI (`adb_cli`):** a hand-rolled classified enum `ADBCliError` (NOT
  `anyhow`), surfaced via `ExitCode` + `log::error!`.
- **Python bindings (`pyadb_client`):** `anyhow::Result` + PyO3's `anyhow`
  feature → implicit Python `RuntimeError`. Never hand-build a `PyErr`.

---

## Error Types

### Core: `RustADBError` (`adboost/src/error.rs`)

- Uses **`thiserror`**: `#[derive(Error, Debug)]` (`error.rs:7`).
- ~45 variants (`error.rs:8-149`), public, re-exported from `lib.rs`.
- The crate `Result` alias (`error.rs:4`):
  ```rust
  pub type Result<T> = std::result::Result<T, RustADBError>;
  ```
  Referenced as `crate::Result<T>` internally; the CLI imports
  `adboost::Result` directly.

Variant conventions (use these patterns when adding a variant):

| Pattern | Example | Line |
|---|---|---|
| `#[error(transparent)]` + `#[from]` for foreign errors | `IOError(#[from] std::io::Error)` | 10–11 |
| Format-string variant with payload | `#[error("ADB request failed - {0}")] ADBRequestFailed(String)` | 16–17 |
| Multi-field format variant | `WrongResponseReceived(String, String)` | 22–23 |
| Unit variant, static message | `#[error("Conversion error")] ConversionError` | 46–47 |
| Numeric-payload variant | `InvalidIntegrity(u32, u32)` | 94–95 |
| Feature-gated variant | `#[cfg(feature = "usb")] UsbError(#[from] nusb::Error)` | 78–81 |

There are ~18 `#[from]` conversions for foreign error types (std::io, Utf8,
AddrParse, regex, ParseInt, image, nusb, base64, rsa, rcgen, rustls, pem,
mdns_sd, chrono, …). Because `std::sync::PoisonError<T>` is generic, it gets a
**manual** `From` impl (`error.rs:151-155`) mapping any `PoisonError<T>` →
`Self::PoisonError`.

**Rule:** add a new foreign error with `#[from]` + `#[error(transparent)]`; add a
new domain error as a named variant with a `{0}`-style format message.

### USB error variants (`nusb`, feature = "usb")

The USB transport uses **`nusb`** (pure-Rust, not `rusb`/libusb). Two
feature-gated variants:

| Variant | Source | Meaning |
|---|---|---|
| `UsbError(#[from] nusb::Error)` | nusb non-transfer ops (enumerate/open/claim/descriptors) | generic USB error |
| `UsbTransferError(#[from] nusb::transfer::TransferError)` | a bulk `transfer_blocking` failure that is NOT a timeout | transfer-level error |

> **Gotcha — read timeout must be matched structurally, never by string.**
> `nusb`'s `Endpoint::transfer_blocking(buf, timeout)` returns
> `TransferError::Cancelled` on timeout (it does NOT carry a "timed out"
> string). `map_transfer_status` (`usb_transport.rs`) maps `Cancelled →
> RustADBError::ReadTimeout`; all other `TransferError`s map to
> `UsbTransferError` so they correctly break the reader loop. **Never reintroduce
> a `err.to_string().contains("timed out")` check** — nusb's wording differs from
> libusb's and the string match silently breaks the disconnect/timeout
> distinction.

### Transport-neutral read timeout: `ReadTimeout` (NOT feature-gated)

`RustADBError::ReadTimeout` is the single, **non-`#[cfg(feature = "usb")]`**
variant every transport returns when `ADBMessageTransport::read_message_with_timeout`
hits its deadline before a full message arrives. This is an explicit trait
contract (documented on the trait method), not an implicit convention:

- USB: `map_transfer_status` maps `TransferError::Cancelled → ReadTimeout`.
- TCP: `tcp_transport.rs::read_exact_timeout` maps its `tokio::time::timeout`
  elapse → `ReadTimeout` (the **read** path only; `write_all_timeout` still
  returns `IOError(ErrorKind::TimedOut)` with "TCP write timed out" — write
  timeout unification is deliberately out of scope).
- Consumer: the transport-generic persistent reader's `classify_read_result`
  (`persistent.rs`) matches ONLY `Err(RustADBError::ReadTimeout)` →
  `ReadStep::ReadTimeout` (keep looping / `continue`); everything else is a
  `ReadStep::ReadError` subject to the fatal/recoverable split.

> **Why this exists (regression history):** the trait method originally did not
> specify a timeout variant, so USB returned the old feature-gated `UsbTimeout`
> while TCP returned `IOError(ErrorKind::TimedOut)`. The reader only matched
> `UsbTimeout`, so a TCP idle read timeout fell into the fatal branch and tore
> down the entire persistent connection (`tcpip.shell_through_tcp_device` dropped
> ~1s after a successful CNXN handshake). Gating "timeout" on the `usb` feature
> was also wrong — TCP can build without `usb` and still needs the variant. The
> removed `UsbTimeout` variant is a **breaking** (major) error-enum change.
>
> **Rule:** when adding a new transport, return `RustADBError::ReadTimeout` on
> read-deadline elapse. Never reintroduce a transport-specific timeout encoding
> for reads, and never re-gate the timeout concept on a transport feature.

### CLI: `ADBCliError` (`adb_cli/src/models/adb_cli_error.rs`)

A hand-rolled enum (not `anyhow`, not `thiserror`) with two variants
(`adb_cli_error.rs:7-9`):
- `Standard(Box<dyn std::error::Error>)` — expected, user-facing errors.
- `MayNeedAnIssue(Box<dyn std::error::Error>)` — unexpected/internal; its
  `Display` impl prints a "this may be a bug, please report on GitHub" message
  (`adb_cli_error.rs:16-24`).

Alias: `pub type ADBCliResult<T> = Result<T, ADBCliError>;` (`adb_cli_error.rs:5`).

---

## Error Handling Patterns

### `?` is the dominant idiom

The `#[from]` impls mean foreign errors auto-convert through `?` with no
boilerplate. Example (`server_device/commands/send.rs:99,114`): `str::from_utf8(...)?`
and `String::from_utf8(body)?` both lift via the Utf8 `#[from]` variants.

### `.map_err(...)` — two distinct idioms

1. **Collapse a foreign error into a domain variant** (discard source with `|_|`):
   ```rust
   .try_into().map_err(|_| RustADBError::ConversionError)?         // send.rs:108
   sender.send(device).map_err(|_| RustADBError::SendError)?       // mdns/mdns_discovery.rs:42
   ```
2. **Re-wrap a cross-thread / channel error as `std::io::Error`**, then let `?`
   lift it through the `IOError` `#[from]` — used heavily in the persistent USB
   multiplexer and session stream:
   ```rust
   .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?  // persistent.rs:87,...
   .map_err(io::Error::other)?                                         // send.rs:26 (terser form)
   ```

### `Option → Result` lift

Use `.ok_or_else(|| RustADBError::...)?` (e.g.
`models/adb_stat_extended_response.rs:141`, used repeatedly in that file).

### Error construction at call sites

- Guard clause: `return Err(RustADBError::ADBRequestFailed("...".into()));`
  (`server_device/commands/send.rs:85-87`).
- Direct: `Err(RustADBError::DeviceNotFound("no device connected".to_string()))`
  (`server/commands/devices.rs:58`).
- Typed payload: `return Err(RustADBError::InvalidIntegrity(expected, got))`
  (`message_devices/usb/usb_transport.rs:274`).
- `match` fallthrough: `_ => Err(RustADBError::...)` (e.g. `models/device_state.rs:75`).

---

## Panics: when they are allowed

`#![forbid(unsafe_code)]` is set crate-wide (`lib.rs:2`). Panics are allowed
**only** in two categories:

1. **Static `LazyLock` regex compilation** — the pattern is a compile-time
   constant, so `.expect("...")` only fires on a developer typo. Accepted idiom:
   ```rust
   static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(LITERAL).expect("..."));
   ```
   (e.g. `server/models/device_short.rs:7`, `models/adb_stat_extended_response.rs:88`).
2. **Test code** under `#[cfg(test)]` / `#[test]` — `.expect("reason")` and
   `panic!` are fine there.

Everywhere else, propagate via `Result` + `?`. No `todo!` / `unimplemented!`
exists in the codebase.

---

## CLI error surfacing (`adb_cli`)

- `fn main() -> ExitCode` (`main.rs:114`) calls `inner_main()`; on `Err` it logs
  `log::error!("{err}")` (using the `Display` impl) and returns
  `ExitCode::FAILURE` (`main.rs:115-118`).
- `fn inner_main() -> ADBCliResult<()>` (`main.rs:123`) uses `?` throughout,
  relying on the `From` impls to lift errors.
- **`From<adboost::RustADBError> for ADBCliError`** (`adb_cli_error.rs:35-86`)
  does the classification: an **exhaustive match (no `_` arm)** routes each
  `RustADBError` variant to `MayNeedAnIssue` (internal) or `Standard`
  (user-facing). The exhaustive match is intentional — adding a new
  `RustADBError` variant forces a compile error here until it is classified.
- `From<std::io::Error>` routes IO errors to `Standard` (`adb_cli_error.rs:28-33`).
- Argument-validation errors are built directly:
  `ADBCliError::Standard("...".into())` (`main.rs:195,205`).

**Rule:** when adding a `RustADBError` variant, also add its classification arm
in `adb_cli_error.rs` — the compiler will remind you, do not silence it with `_`.

---

## Python bindings error mapping (`pyadb_client`)

- PyO3 is configured with the `anyhow` feature (`pyadb_client/Cargo.toml:28`)
  plus `anyhow = "1.0"` (`Cargo.toml:27`).
- Fallible `#[pymethods]` return **`anyhow::Result<T>`** and just `?`-propagate
  the underlying `RustADBError`. `anyhow` absorbs it; PyO3's `anyhow` feature
  converts `anyhow::Error → PyErr` (a Python `RuntimeError` carrying the
  `Display` string).
- There is **zero** explicit exception construction (`PyValueError::new_err`,
  `.map_err` to a `PyErr`) in `pyadb_client/src`.

**Rule:** new fallible binding methods return `anyhow::Result<T>` and rely on
`?`. Do not hand-build `PyErr`. (Two constructors use `PyResult` —
`adb_server.rs:20`, the `#[pymodule]` fn — these are the outliers; prefer
`anyhow::Result` for consistency.)

---

## Common Mistakes

- **`lock().unwrap()` on a `Mutex`** instead of propagating `PoisonError`. The
  crate defines `RustADBError::PoisonError` + a `From<PoisonError>` impl
  precisely so locks can ride `?`, yet `message_devices/usb/persistent.rs` has
  9 `lock().unwrap()` calls (lines 232, 265, 284, 295, 313, 563, 612, 675, 728)
  that bypass it. **This is known tech debt — do not copy it.** New lock sites
  should propagate `PoisonError` via `?`.
- Adding a `RustADBError` variant without a classification arm in
  `adb_cli_error.rs` (only works because the match is exhaustive — keep it that
  way; never add a `_` arm to "fix" the compile error).
- Hand-building `PyErr` in `pyadb_client` instead of returning `anyhow::Result`.
- `.unwrap()` / `.expect()` on a genuinely fallible path in library code —
  reserve them for static-regex init and tests only.
