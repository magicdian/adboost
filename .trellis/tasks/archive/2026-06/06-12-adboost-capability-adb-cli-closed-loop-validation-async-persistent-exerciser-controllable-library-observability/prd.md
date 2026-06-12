# adboost capability: adb_cli closed-loop validation + controllable library observability

## Goal

Close two capability gaps exposed by the recent bug-fix chain (#1 delayed_ack version,
#2 crc skip-checksum, #3 CNXN banner NUL — all in the async `PersistentUsbConnection`):

1. **Closed-loop validation via the CLI** — exercise adboost changes in-tree against a real
   device before shipping, so bugs don't surface first at the downstream consumer (xdb). The
   CLI also serves as a reference implementation for new APIs.
2. **Controllable observability in the library** — a downstream consumer hitting a problem can
   flip on DEBUG/TRACE logging via env var (no rebuild), with per-session attribution.

This is a parent task; work is split into two subtasks (see Decomposition).

## Decisions (ADR-lite)

### D1 — CLI rebrand `adb_cli` → `adboost_cli`, full async migration
- async `main` (tokio), depends on the **local workspace** `adb_client` (path dep, not the
  crates.io `^3.2.1`), added back to workspace `members` + CI.
- **Context**: `adb_cli` is sync + legacy-API (`ADBUSBDevice`, `device.shell(...)?` no await)
  and does NOT compile against the now-async `adb_client`. It also never touches
  `PersistentUsbConnection` — the exact surface where #1/#2/#3 lived.
- **Naming**: CLI crate = `adboost_cli`. The bare name **`adboost` is reserved for the
  library's future** (the `adb_client` crate may later be renamed `adboost`); the CLI must not
  claim it.
- **Rationale**: adboost's future is pure-async; upstream `adb_client` patches will only be
  re-imported into the **library**, never the CLI. The CLI no longer tracks upstream's
  `adb_cli`, so cutting the name is correct.

### D2 — Library migrates `log` → `tracing` (emit-only), env-var + optional init helper
Research: [`research/logging-approach.md`](research/logging-approach.md).
- Mechanical rewrite of the 69 `log::x!` → `tracing::x!` (identical call shape, all
  fully-qualified → near-zero-risk). Enable `tracing`'s `log` feature so existing
  `log`/`env_logger` consumers still see output (backward compatible).
- **Hot-path spans** carrying the existing `local_id: u32` as a span field: `do_connect`,
  `do_auth`, `reader_loop`, `writer_loop`, `open_session`, `open_shell_v2`,
  `open_sync_session` — fixes the "which session/task emitted this?" pain behind the bugs.
- **Library stays a pure emitter** — no `tracing-subscriber`/`env_logger` in `adb_client`
  default deps. Activation = `RUST_LOG` (`EnvFilter`: per-module + per-span + per-`local_id`)
  **plus an optional feature-gated `init_tracing_from_env()` helper** (`tracing-init` feature,
  OFF by default, `try_init` so it never fights a consumer-installed subscriber).

## Requirements (MVP)

### Library (subtask A)
- [ ] `tracing` replaces `log` across `adb_client` (69 sites); `log` feature enabled for compat.
- [ ] Spans on the hot paths with `local_id` as a field — including **sync_session (push/pull)
      and shell_v2_session**, not just `open_session` (expansion sweep).
- [ ] Library installs NO subscriber; `tracing-init` feature (off by default) gates
      `init_tracing_from_env()` using `try_init`.
- [ ] Per-frame `trace!` on the hottest paths confirmed level-gated (no overhead when off).

### CLI (subtask B)
- [ ] `adb_cli` → `adboost_cli`: dir `adb_cli/` → `adboost_cli/`, crate/binary name
      `adboost_cli`, async `main` (tokio), path dep on local `adb_client`, back in `members` + CI.
- [ ] All existing CLI subcommands compile & run against the async library (migrate `.await`).
- [ ] **New `persistent` / `usb-direct` exerciser subcommand** driving
      `PersistentUsbConnection` end-to-end (e.g. `shell getprop` via `shell_exec`/`open_shell_v2`).
- [ ] **Self-check output** in the exerciser: print negotiation ground truth (device_version,
      delayed_ack negotiated, the banner sent, first frame cmd after OPEN) — formalizes the
      /tmp diagnostic harness so future bugs reproduce with one command (expansion sweep).
- [ ] CLI uses `init_tracing_from_env()` so `RUST_LOG` works out of the box.

### Docs
- [ ] README/spec: how to enable logging (`RUST_LOG` examples incl. per-`local_id`); how to run
      `adboost_cli` against a real device for persistent closed-loop validation (env, steps) —
      manual real-device smoke, not automated CI (needs hardware) (expansion sweep).

## Acceptance Criteria

- [ ] `cargo build`/`clippy`/`test` green for BOTH `adb_client` and `adboost_cli` in the
      workspace (CI builds both).
- [ ] `RUST_LOG=adb_client=trace` (and a per-`local_id` span filter example) produce attributed
      output from a downstream consumer with no adboost rebuild.
- [ ] `adboost_cli <persistent-exerciser> shell getprop` works against a real device and prints
      the self-check negotiation summary.
- [ ] Existing `log`/`env_logger`-style consumers still receive output (tracing `log` feature).

## Definition of Done
- Tests added/updated; lint/typecheck/CI green for both crates.
- Docs updated (logging activation + real-device validation runbook).
- No regression to the `adb_client` async API; backward-compatible log output preserved.

## Out of Scope
- pyadb_client bindings (separate crate, not mentioned).
- Full upstream feature parity of the CLI (only what closed-loop validation needs).
- Automated real-device CI (no hardware in CI — manual runbook only).
- Renaming the library crate `adb_client` → `adboost` (reserved future work, not now).
- `tokio-console` integration (possible later; `tracing` migration is the enabler).

## Decomposition (subtasks)
- **A — library tracing migration** (do FIRST; the CLI depends on the new logging story).
- **B — adboost_cli rebrand + async migration + persistent exerciser** (depends on A).

## Technical Notes
- Recent bug context + wire contract: `.trellis/spec/backend/adb-wire-protocol-contract.md`.
- Library helpers already present: `PersistentUsbConnection::{shell_exec, open_shell_v2,
  open_session}` (`persistent.rs:894/1058/1071`); `local_id` generated at `persistent.rs:895`.
- Key files: `Cargo.toml` (members), `adb_cli/` (→ `adboost_cli/`),
  `adb_client/Cargo.toml`, `adb_client/src/message_devices/usb/persistent.rs`,
  `adb_client/src/adb_device_ext.rs`.
- Research: [`research/logging-approach.md`](research/logging-approach.md).
