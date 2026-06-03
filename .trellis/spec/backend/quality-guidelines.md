# Quality Guidelines

> Code quality standards for `xp_adb_client`. Documents the actual enforced
> standards (clippy config, CI commands, testing style) — not aspirations.

---

## Toolchain baseline

From the root `Cargo.toml` `[workspace.package]`, inherited by all members:

- **Edition: 2024** (`Cargo.toml:7`)
- **MSRV: `rust-version = "1.88.0"`** (`Cargo.toml:13`) — CI has a dedicated
  MSRV job; do not use APIs newer than 1.88.0.
- **Version: 3.2.2** (`Cargo.toml:12`) — owned by the workspace, inherited via
  `version.workspace = true`. Do not duplicate/override per-crate.

No `rustfmt.toml`, `.rustfmt.toml`, or `clippy.toml` exists anywhere — the
project relies on **default `rustfmt`** and the inline `[workspace.lints.clippy]`
table.

---

## Clippy / lint configuration

Root `Cargo.toml:19-21`:

```toml
[workspace.lints.clippy]
pedantic = { level = "warn", priority = -1 }
missing_errors_doc = "allow"
```

All members opt in via `[lints] workspace = true`. **CI runs
`cargo clippy --all-targets -- -D warnings`**, so pedantic findings fail CI.
Write code that passes `clippy::pedantic`.

### Lint attribute conventions

- **`adb_client` core lib forbids `unsafe`**: `#![forbid(unsafe_code)]`
  (`lib.rs:2`). Do not introduce `unsafe` in the library.
- Allowed crate-level relaxations (inner attributes only):
  - `lib.rs:3` `#![allow(missing_debug_implementations)]`
  - `lib.rs:4` `#![allow(missing_docs)]`
  - `lib.rs:5` `#![allow(clippy::missing_errors_doc)]`
- `pyadb_client` **requires** docs: `#![forbid(missing_docs)]`
  (`pyadb_client/src/lib.rs:1`) — every public binding item needs a doc comment.

### Required pattern: crate-level allows, not item-level

The codebase has **zero item-level `#[allow(...)]`** attributes. Prefer a
crate-level inner `#![allow(...)]` for a lint that is genuinely undesirable
project-wide; avoid scattering `#[allow(...)]` on individual items. If you must
silence a lint locally, justify it in a comment.

---

## Feature flags (`adb_client/Cargo.toml:20-24`)

```toml
[features]
default = ["framebuffer"]
mdns = ["dep:mdns-sd"]
usb = ["dep:nusb"]
framebuffer = ["dep:image"]
```

- **`framebuffer`** (default) — gates framebuffer dump/decode + related error
  variants. Code in `message_devices/commands/framebuffer.rs`.
- **`usb`** — gates the whole `message_devices/usb/` subsystem + USB error
  variants. `adb_cli` and `pyadb_client` enable it. Backed by **`nusb`**
  (pure Rust, no libusb/C toolchain — `usb = ["dep:nusb"]`).
- **`mdns`** — gates `pub mod mdns` + mDNS discovery.
- **There is no `tcp` feature** — TCP transport is always compiled. The `tcp`
  keyword in `Cargo.toml:9` is a crates.io keyword, not a feature.

Gate feature-specific code with `#[cfg(feature = "...")]` and add the docs.rs
badge companion `#[cfg_attr(docsrs, doc(cfg(feature = "...")))]` for new public
feature-gated items (matches `lib.rs:30`, `error.rs:79`, etc.).

**CI caveat (known gap):** CI invokes clippy/test/build with **default features
only** — it does **not** pass `--features usb`/`mdns`. So USB/mDNS code paths and
their pedantic lints are not covered by CI. When you change USB/mDNS code,
**verify locally** with the feature enabled (see below).

---

## Testing Requirements

- **Style:** inline `#[cfg(test)] mod tests { ... }` in the same file as the code
  under test. There is **no `tests/` directory**.
- **Scope:** unit-level, parser/serialization-focused — no device or network
  I/O. Existing tests: `message_devices/utils.rs:15`,
  `server/models/device_long.rs:101`, `models/adb_stat_extended_response.rs:219`.
- **Assertions:** std `assert_eq!` with a **descriptive message string** as the
  third arg, e.g. `assert_eq!(a, b, "serialized data does not match deserialized")`.
- **Test data:** realistic captured ADB output as raw string literals.
- **In tests** `.expect("reason")` on `Result` is acceptable (it is the
  convention) — unlike production code.
- No `mockall` / external test-util crates; keep tests dependency-free.
- New parser/serialization logic should ship with a unit test in the same file.

### Benchmarks

`benches/benchmark_adb_push.rs` uses **`criterion`** (dev-dependency,
`harness = false`). It benchmarks against a real device, so it is a manual/local
bench, **not run in CI**.

---

## Quality gate (run before declaring work done)

Mirror what CI does, plus cover the feature-gated paths CI misses:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings        # default features (what CI runs)
cargo test

# If you touched USB / mDNS / framebuffer code, also run with the feature on:
cargo clippy -p adb_client --features usb --all-targets -- -D warnings
cargo build  -p adb_client --features usb
cargo test   -p adb_client
cargo build  -p adb_cli                          # dependents still build
```

---

## Forbidden / discouraged patterns

- `unsafe` in `adb_client` (forbidden by `#![forbid(unsafe_code)]`).
- `println!`/`eprintln!` for output in library code — use the `log` macros
  (see `logging-guidelines.md`).
- `.unwrap()`/`.expect()` on genuinely fallible paths in production code —
  allowed only for static `LazyLock<Regex>` init and tests
  (see `error-handling.md`).
- New `lock().unwrap()` on a `Mutex` — propagate `RustADBError::PoisonError`
  instead (the `persistent.rs` unwraps are known tech debt).
- Item-level `#[allow(...)]` scattered across the code — prefer a justified
  crate-level allow.
- `todo!` / `unimplemented!` — none exist; don't introduce them on merged code.
- Detaching a crate from the workspace or duplicating inherited
  `*.workspace = true` metadata (see `directory-structure.md` and
  `upstream-patch-import.md`).

---

## Code Review Checklist

- [ ] Passes `cargo fmt --all --check` (default rustfmt).
- [ ] Passes `cargo clippy --all-targets -- -D warnings` (pedantic-clean).
- [ ] Feature-gated changes verified locally with `--features usb` (CI won't).
- [ ] No new `unsafe`, `println!` (lib), fallible-path `unwrap`, or item-level
      `#[allow]`.
- [ ] New parser/serialization code has an inline `#[cfg(test)]` test.
- [ ] New `RustADBError` variant has a classification arm in
      `adb_cli/src/models/adb_cli_error.rs` (exhaustive match).
- [ ] Errors handled per `error-handling.md`; logging per `logging-guidelines.md`.
- [ ] MSRV-safe (no APIs newer than Rust 1.88.0).
