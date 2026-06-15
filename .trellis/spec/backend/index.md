# Backend Development Guidelines

> Coding guidelines for `xp_adb_client` — a pure-Rust ADB (Android Debug
> Bridge) client library, forked from `cocool97/adb_client` at v3.2.2 and
> maintained independently.

---

## Overview

This is a Rust Cargo workspace (edition 2024, MSRV 1.88.0): the `adb_client`
core library, the `adb_cli` binary, the `pyadb_client` PyO3 bindings, and an
mDNS example. There is **no web/database backend** — "backend" here means the
Rust library + CLI code. The guidelines below document the codebase's **actual**
conventions with real file paths and line numbers.

---

## Pre-Development Checklist

Read the relevant guideline before writing code in that area:

- **Always**: [Directory Structure](./directory-structure.md),
  [Quality Guidelines](./quality-guidelines.md)
- **Touching errors / `Result` / panics**: [Error Handling](./error-handling.md)
- **Adding `log` calls**: [Logging Guidelines](./logging-guidelines.md)
- **Persistence / RSA key / session state**: [Persistence & External State](./database-guidelines.md)
- **CNXN version / `delayed_ack` / message integrity (crc/magic)**: [ADB Wire Protocol Contract](./adb-wire-protocol-contract.md)
- **Server frontend host protocol / transport selection (`host:tport*` / `transport*` / forward routing)**: [Server Host Protocol](./server-host-protocol.md)
- **Importing an upstream `.patch`**: [Upstream Patch Import](./upstream-patch-import.md)

---

## Guidelines Index

| Guide | Description | Status |
|-------|-------------|--------|
| [Directory Structure](./directory-structure.md) | Workspace + module layout, mod.rs convention, models/commands split, device trait layering, composable backend helpers (sync/shell_v2/`ReverseEngine`) + reverse-by-link-type | Filled |
| [Persistence & External State](./database-guidelines.md) | No DB; RSA key on disk, USB session multiplexing, state conventions | Filled |
| [Error Handling](./error-handling.md) | `RustADBError` (thiserror), `Result` alias, CLI `ADBCliError`, PyO3 `anyhow` mapping | Filled |
| [Quality Guidelines](./quality-guidelines.md) | clippy pedantic, MSRV, features, testing style, quality gate | Filled |
| [Logging Guidelines](./logging-guidelines.md) | `log` facade, `log::<level>!` style, level conventions | Filled |
| [ADB Wire Protocol Contract](./adb-wire-protocol-contract.md) | CNXN version ↔ `delayed_ack` ↔ `data_check` coupling; magic-only receive integrity; AOSP version constants | Filled |
| [Server Host Protocol](./server-host-protocol.md) | Server frontend transport-selection parity (`tport`/`transport`/forward); AOSP error wording matrix; `tport:any`-before-local-service gotcha | Filled |
| [Upstream Patch Import](./upstream-patch-import.md) | How to import patches into this fork (skip Cargo.toml, handle version drift) | Filled |

---

**Language**: All documentation is written in **English**.
