# Subtask A — library `log` → `tracing` migration (emit-only, hot-path spans, optional init helper)

Parent: `06-12-adboost-capability-adb-cli-closed-loop-validation-async-persistent-exerciser-controllable-library-observability`.
Do this BEFORE subtask B (the CLI depends on the new logging story).

## Goal

Migrate `adb_client` from the `log` facade to `tracing` so a downstream consumer can flip on
DEBUG/TRACE logging via env var (no rebuild) with **per-session attribution** — the exact gap
that made bugs #1/#2/#3 hard to diagnose (flat interleaved reader/writer/session log lines
couldn't be tied to a `local_id`). The library stays a pure emitter.

Research: [`../06-12-adboost-capability-.../research/logging-approach.md`] (parent task) —
primary recommendation: migrate to `tracing` + `log` feature + hot-path spans on `local_id`.

## Requirements (MVP)

- [ ] Replace the ~69 `log::{trace,debug,info,warn,error}!` call sites with `tracing::...`
      across `adb_client/src`. Call shape is identical and all sites are fully-qualified, so
      this is a mechanical find/replace; verify each still compiles unchanged.
- [ ] `adb_client/Cargo.toml`: replace `log = "0.4"` with
      `tracing = { version = "0.1", features = ["log"] }`. The `log` feature makes every
      `tracing` event also emit a `log` record, so existing `log`/`env_logger` consumers (and
      the current CLI before subtask B lands) keep seeing output — backward compatible.
- [ ] Add hot-path spans carrying the existing `local_id: u32` as a span field, on:
      `do_connect`, `do_auth`, `reader_loop`, `writer_loop`, `open_session`, `open_shell_v2`,
      `open_sync_session`. Use `#[tracing::instrument(...)]` or manual `info_span!` —
      whichever fits each fn (skip non-Debug/large args). Every WRTE/OKAY/CLSE event emitted
      while a session span is entered must inherit `local_id`.
- [ ] **Library installs NO subscriber.** No `tracing-subscriber` / `env_logger` in
      `adb_client` default dependencies. Add an optional `tracing-init` feature (OFF by
      default) gating `tracing-subscriber = { ..., features = ["env-filter","fmt"], optional }`
      and a helper:
      ```rust
      #[cfg(feature = "tracing-init")]
      pub fn init_tracing_from_env() {
          use tracing_subscriber::{fmt, EnvFilter};
          let _ = fmt().with_env_filter(EnvFilter::from_default_env())
                       .with_writer(std::io::stderr).try_init(); // never panics / never fights an installed subscriber
      }
      ```
- [ ] Confirm per-frame `trace!` on the hottest paths (e.g. `reader_loop` per-frame line at
      `persistent.rs:653`, the `do_connect` drain loop) is level-gated so there is no cost when
      the level is disabled (tracing checks level before evaluating args — verify no eager
      formatting outside the macro).

## Acceptance Criteria

- [ ] `cargo build -p adb_client` (default + `--features usb`) green; no `log::` references
      remain in `adb_client/src` (grep clean); `tracing` is the only logging emit dep.
- [ ] `cargo clippy -p adb_client --all-targets --features usb -- -D warnings` and default green.
- [ ] `cargo test -p adb_client --features usb` green (existing tests unaffected — message
      strings may change but no test asserts on `log` output today; verify).
- [ ] `cargo build -p adb_client --features usb,tracing-init` green and `init_tracing_from_env`
      is reachable only under the feature.
- [ ] Manual: with a `tracing-subscriber` consumer (or the `tracing-init` helper),
      `RUST_LOG=adb_client=trace` emits output; a `[session{local_id=...}]` filter narrows to
      one session (documented example; spot-checkable via a unit/integration test that enters a
      `session` span and asserts the field is present, if feasible without hardware).

## Definition of Done
- Tests/clippy/build green; no `log::` left in the library; backward-compatible output via the
  `log` feature.
- Brief doc note (in code docs and/or spec) on how to activate logs (RUST_LOG examples incl.
  per-`local_id`) and the library-stays-emit-only contract. Full README runbook is in the
  parent/CLI subtask, but the library-side activation contract is documented here.

## Out of Scope
- The CLI changes (subtask B).
- `tokio-console` (the `tracing` migration is the enabler; not wired here).
- Renaming `adb_client` → `adboost`.
- Changing log message wording beyond what the macro swap requires.

## Technical Notes
- 69 sites: `log::debug!`×22, `trace!`×17, `warn!`×14, `info!`×9, `error!`×7.
- `local_id` generated at `persistent.rs:895`; span field source.
- Transitive deps already enable `log` output (`rustls`/`tokio-rustls`/`mdns-sd`
  `features=["logging"]`); with `tracing/log` these still flow to a `log` subscriber, and a
  `tracing` consumer can pick them up via `tracing-log` on their side.
- Hot-path fn anchors: `do_connect` (`persistent.rs:524`), `do_auth` (`:606`),
  `reader_loop` (`:613/661`), `writer_loop` (`:818`), `open_session` (`:894`),
  `open_shell_v2` (`:1058`), `open_sync_session` (`:1044`).
