# Research: Observability approach for adb_client (async ADB client library)

- **Query**: `log` facade vs `tracing` for an async Rust LIBRARY; library hygiene; consumer activation without rebuild; concrete ranked recommendation for adboost.
- **Scope**: mixed (internal repo verification + external ecosystem practice)
- **Date**: 2026-06-12

---

## Verified repo state (ground truth)

| Fact | Evidence |
|---|---|
| `adb_client` depends on `log` only; no `tracing`, no `env_logger` | `adb_client/Cargo.toml:31` (`log = { version = "0.4.30" }`); no `tracing*` lines anywhere in manifest |
| 69 `log::*` call sites, all `log::debug!/trace!/warn!/info!/error!` style (fully-qualified, no `use log` imports) | `grep -rE 'log::(debug\|trace\|warn\|info\|error)' adb_client/src` → 69; `use log` → 0 hits |
| The CLI is where activation happens | `adb_cli/Cargo.toml:19-20` has `env_logger = "0.11.10"` and `log = "0.4.30"`; `adb_client` has neither subscriber |
| Library is async/multi-task with per-session multiplexing | `persistent.rs:381-387` spawns `reader_loop` + `writer_loop` tokio tasks; `reader_loop` (`:661`) demuxes by `local_id` into `HashMap<u32, SessionChannels>` (`:666`) |
| Per-session identity already exists as `local_id: u32` | `open_session` (`:894`) generates `local_id` (`:895-898`), registers it (`ReaderControl::Register(local_id, ...)`, `:914`) |
| Several transitive deps already enable the `logging` feature for `log`-based output | `rustls`/`tokio-rustls`/`mdns-sd` all have `features = ["logging"]` in `adb_client/Cargo.toml:42,57,62-64` |
| Hot paths that would benefit from span context | `do_connect` (`:524`), `do_auth` (`:606`), `reader_loop` (`:661`), `writer_loop` (`:818`), `open_session` (`:894`), `open_sync_session` (`:1044`), `open_shell_v2` (`:1058`) |

The recent hard bugs (delayed_ack version gating at `persistent.rs:362-368`, CNXN banner NUL at `do_connect:524+`, crc/skip-checksum) all emit via `log::debug!/trace!` and were diagnosed with ad-hoc `RUST_LOG=debug`. The pain: a flat `log` line from interleaved reader/writer/session tasks cannot tell you *which* `local_id`/task produced it.

---

## Findings

### 1. `log` facade vs `tracing` for an ASYNC library

**Modern consensus: for async libraries, `tracing` is the recommended choice; `log` remains fine for simple synchronous libraries.**

- The `log` crate's own docs explicitly defer to `tracing` for async/structured needs: "If you use `async` code... consider the `tracing` library, whose `Span` type... can be used to instrument async functions." (docs.rs `log` crate top-level docs, "Crate Features"/"Use with `std`" notes, and the long-standing `log` README pointer to tracing.)
- `tracing`'s value proposition is precisely the adboost problem: **spans attach contextual key/values that are carried across `.await` points and across tasks**, so every event emitted while a span is entered is tagged with that span's fields. For concurrent reader/writer/N-session interleaving, a `local_id` span field turns "which session was this?" from guesswork into a filterable field. (tracing docs.rs: "Spans" + "`in_scope`" + `#[instrument]` sections; tokio.rs blog "Diagnostics with Tracing", 2019.)
- Plain `log` has no notion of a span; a `log::debug!` line carries only its message + module path + level. You can manually prepend `local_id` to every message (adboost already does this informally, e.g. `"PersistentUsb: ..."` prefixes), but that is not *structured* and cannot be filtered as a field.

**What mature async libs actually emit:**

| Library | Emits | Notes |
|---|---|---|
| **tokio** | `tracing` | tokio is instrumented with `tracing` (feature `tracing`); `tokio-console` consumes its spans/events. The tokio project *authors* `tracing`. |
| **hyper** | `tracing` | hyper 0.14+ migrated from `log` to `tracing` (behind its internal `tracing`/`nightly` debug gating). |
| **reqwest** | `log` historically; relies on hyper's `tracing` underneath | reqwest emits via `log` at the reqwest layer; lower layers (hyper/h2) emit `tracing`. Demonstrates the bridge coexistence (see §3). |
| **sqlx** | `tracing` | sqlx emits query spans/events via `tracing` (e.g. `sqlx::query` target with slow-query warnings). |
| **rustls** | `log` | rustls deliberately uses the `log` facade (behind its `logging` feature — which adboost already turns on at `Cargo.toml:42`). A counter-example: a security-focused, mostly-synchronous-state-machine library that stays on `log`. |
| **aws-sdk-rust** | `tracing` | The AWS SDK for Rust standardized on `tracing` for request/retry/span instrumentation. |

Takeaway: every *async, multi-task, concurrency-heavy* library in the set (tokio, hyper, sqlx, aws-sdk) emits `tracing`. The `log`-only holdouts (rustls) are essentially synchronous state machines where per-task span context buys little. adboost is firmly in the first category (spawned reader/writer tasks + multiplexed sessions), so the span advantage **does** justify migration here.

### 2. Library hygiene — emit only, never install a subscriber

**Confirmed idiomatic split:**

- A **library** must only *emit* (`log::` records or `tracing::` events/spans). It must **never** call `tracing_subscriber::fmt().init()`, `env_logger::init()`, `set_logger`, or `set_global_default`. Installing a global subscriber/logger is a process-global, set-once side effect; if a library does it, it steals that one global slot from the binary and from every other consumer.
  - `log` docs: "Loggers are installed by the executable... Libraries should never set the logger." (`log` crate docs, "Use" / "In libraries" section.)
  - `tracing` docs: the global default subscriber is set with `set_global_default`/`subscriber::set_default`, described as the binary's responsibility; tracing-subscriber's `init()` docs warn it sets the global default and should be called once, by the application. (tracing-subscriber docs.rs, `fmt::init` / `util::SubscriberInitExt::init`.)
- The **binary** (`adb_cli` → `adboost_cli`) and **downstream consumers** (xdb) own subscriber installation. This already matches the repo: `adb_cli` is the only crate with `env_logger`.

Net: keep `adb_client` a pure emitter. Whatever the choice in §1, the dependency that installs output (`env_logger` or `tracing-subscriber`) lives in the binary/consumer, not in `adb_client`'s default build.

### 3. Interop: the `tracing-log` bridge and `log` feature

Two directions of compatibility exist; both matter for adboost because xdb might be a `tracing` shop and the in-tree CLI is currently a `log`/`env_logger` shop.

- **tracing events → log consumers**: enable `tracing`'s `log` feature (`tracing = { features = ["log"] }`). Then every `tracing` event also emits a corresponding `log` record, so a consumer with only `env_logger`/`set_logger` still sees the output. (tracing docs.rs, "Crate Feature Flags" → `log`; and the `log-always` variant.)
- **log records → tracing subscribers**: the `tracing-log` crate's `LogTracer` captures `log::` records and re-emits them as `tracing` events, so a `tracing`-only application sees output from `log`-emitting dependencies. (tracing-log docs.rs, `LogTracer::init`.) `tracing-subscriber`'s default `init()` already wires this up via its `tracing-log` feature.

**Migration mechanism comparison:**

- *Option A — keep `log::` calls, add the bridge:* add nothing to `adb_client` except optionally documenting that consumers using `tracing` should enable `tracing-log`. Zero code churn. But you get **no spans** — you keep flat lines and never solve the "which session?" problem. This is a non-solution for the stated pain.
- *Option B — mechanical `log::x!` → `tracing::x!` conversion:* `tracing`'s `debug!/trace!/warn!/info!/error!` macros are drop-in source-compatible with `log`'s at call sites (same `(target: ..., "fmt", args)` shape). The 69 sites are all fully-qualified `log::debug!(...)` etc., so a single mechanical rewrite (`log::` → `tracing::`) compiles unchanged. Then *add* spans only on the hot paths. With `tracing`'s `log` feature enabled, the converted calls **still** reach `log`-only consumers — so the current `adb_cli` + `env_logger` keeps working with no CLI change. This is the cleaner path: it yields spans *and* preserves backward compat.

Conclusion: **Option B (convert to `tracing`, enable the `log` feature) is cleaner than Option A**, because a library *can* emit `tracing` while still letting `log`-only consumers see everything (via `tracing/log`), and `tracing`-only consumers see everything too (via the consumer's `tracing-log`). The bridge is bidirectional at the ecosystem level; you just pick where to put each shim.

### 4. Consumer activation without rebuild

- **`log` + env_logger**: `RUST_LOG=adb_client=debug` (per-module via target). Granularity = level + module path. No span/field filtering (spans don't exist).
- **`tracing` + tracing-subscriber `EnvFilter`**: `RUST_LOG` supports level + target *and* span directives, e.g.:
  - `RUST_LOG=adb_client=trace` — whole crate.
  - `RUST_LOG=adb_client::message_devices::usb::persistent=trace` — just the multiplexer.
  - `RUST_LOG=[session{local_id=42}]=trace` — only events within a `session` span whose `local_id` field equals 42 (span+field directive). (tracing-subscriber docs.rs, `EnvFilter` "Directives" → the `[span{field=value}]` syntax.)
  - `RUST_LOG=adb_client=info,[reader_loop]=trace` — combine.

`tracing`/`EnvFilter` gives **strictly more runtime control via env var alone** (per-module *and* per-span/per-field, including per-`local_id` session filtering), which directly serves the downstream "flip on TRACE for one subsystem without rebuilding" requirement. `env_logger` cannot do span/field filtering at all.

**Optional init helper:** the dominant ecosystem norm is that **libraries expose nothing** (tokio, hyper, sqlx, rustls all expose no logging-init API — the consumer wires the subscriber). A minority expose a small *optional, feature-gated* convenience initializer. For adboost, a tiny helper such as:

```rust
#[cfg(feature = "tracing-init")]   // off by default
pub fn init_tracing_from_env() {
    use tracing_subscriber::{fmt, EnvFilter};
    let _ = fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();   // try_init = never panics if a subscriber already exists
}
```

is acceptable **only if** it is (a) feature-gated off by default, (b) uses `try_init` (so it never fights an already-installed consumer subscriber), and (c) documented as a convenience for `adboost_cli` / quick downstream bring-up, not the library's default behavior. The default build stays a pure emitter, preserving §2 hygiene.

### 5. Concrete recommendation for adboost (ranked)

#### PRIMARY recommendation — migrate to `tracing`, emit-only, with hot-path spans

**Crate:** `tracing` (the emitter) in `adb_client`. **No** subscriber crate in `adb_client`'s default deps.

**Cargo layout (zero forced subscriber deps on the bare library):**

```toml
# adb_client/Cargo.toml
[dependencies]
tracing = { version = "0.1", features = ["log"] }   # `log` feature => log-only consumers still see output
# (remove the bare `log` dep; tracing+log covers the bridge)

# Optional convenience initializer, OFF by default:
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"], optional = true }

[features]
tracing-init = ["dep:tracing-subscriber"]   # only enables the init_tracing_from_env() helper
```

The bare `cargo add adb_client` pulls in `tracing` (a tiny, near-universal emit-only crate) and **no subscriber** — xdb installs its own. `tracing`'s `log` feature means the existing `adb_cli` + `env_logger` keeps working unchanged.

**Migration mechanics:**
1. Mechanical rewrite: `log::debug!` → `tracing::debug!`, `log::trace!` → `tracing::trace!`, etc. across the 69 sites (all are fully-qualified, so this is a literal find/replace; the macro call shape is identical). Remove `use log` (there are none) and the `log` dependency.
2. Add spans on the hot paths (these are the interleave points where flat logs are ambiguous):
   - `do_connect` → `#[tracing::instrument(skip(transport, private_key))]` or a manual `info_span!("connect")`.
   - `do_auth` → `info_span!("auth")`.
   - `reader_loop` → `info_span!("reader")` entered for the task's lifetime; `writer_loop` → `info_span!("writer")`.
   - `open_session` / `open_sync_session` / `open_shell_v2` → `info_span!("session", local_id = local_id)` carrying the existing `local_id: u32` (`persistent.rs:895`) as a span field, so every WRTE/OKAY/CLSE event for that session is auto-tagged.
3. Keep the `delayed_ack`/banner/crc debug lines as `tracing::debug!`/`trace!` — they now inherit whatever span is active, so a session-scoped trace shows them in context.

**How a downstream consumer turns on TRACE for one subsystem (no rebuild):**
- Whole crate: `RUST_LOG=adb_client=trace ./xdb`
- Just the multiplexer: `RUST_LOG=adb_client::message_devices::usb::persistent=trace`
- Just the reader task: `RUST_LOG=[reader]=trace`
- Just one session id: `RUST_LOG=[session{local_id=42}]=trace`
- Mixed: `RUST_LOG=adb_client=info,[session]=debug`
  (consumer must have `tracing-subscriber` with `env-filter` installed, which xdb does once; `adboost_cli` can use the optional `init_tracing_from_env()` helper or its own.)

**Rationale:** directly solves the per-session/per-task disambiguation pain that caused the recent hard debugging; gives the consumer maximal env-var runtime control (per-module + per-span + per-field); preserves backward compatibility for the existing `log`/`env_logger` CLI via `tracing/log`; and keeps the library a pure emitter with zero forced subscriber deps. The mechanical rewrite + the fact that `local_id` already exists as a field make the migration low-risk.

#### SECONDARY option — stay on `log`, document the `tracing-log` bridge

Keep the 69 `log::` calls. Add no spans. Document that `tracing` consumers should enable `tracing-log`'s `LogTracer`, and that `RUST_LOG=adb_client=debug` activates output via `env_logger`.

- Pros: zero code change; minimal risk; matches rustls's choice.
- Cons: **does not solve the core problem** — no spans, so interleaved reader/writer/session lines remain unattributable; consumer filtering stays level+module only (no per-session). Acceptable only if migration effort must be deferred.

---

## External References

- **`log` crate docs** (docs.rs/log) — "Loggers are installed by the executable; libraries should never install one"; pointer to `tracing` for async/structured logging.
- **`tracing` crate docs** (docs.rs/tracing) — Spans carry contextual fields across `.await`/tasks; `#[instrument]`; crate feature `log` / `log-always` for emitting `log` records alongside `tracing` events.
- **`tracing-subscriber` docs** (docs.rs/tracing-subscriber) — `EnvFilter` directive syntax including `[span{field=value}]=level`; `fmt::init()`/`try_init()`; `tracing-log` integration enabled by default.
- **`tracing-log` docs** (docs.rs/tracing-log) — `LogTracer::init()` captures `log` records into `tracing`.
- **tokio.rs blog, "Diagnostics with Tracing"** — rationale for spans in async/concurrent code; tokio + tokio-console built on `tracing`.
- **Library practice**: tokio, hyper (0.14+), sqlx, aws-sdk-rust emit `tracing`; rustls emits `log` (behind its `logging` feature — already enabled in `adb_client/Cargo.toml:42`).

## Related Specs / Task Docs

- `.trellis/tasks/06-12-.../prd.md` — task PRD; "controllable library observability" goal, confirms `log`-only state and no `env_logger`/`tracing` in `adb_client`.

## Caveats / Not Found

- This synthesis relies on the verified repo manifest/source plus well-established, published crate documentation and conventions for the named libraries. The per-library "emits log vs tracing" rows reflect each project's documented/source-level instrumentation; exact feature names and minor version boundaries (e.g. the precise hyper version that completed the `log`→`tracing` move) should be reconfirmed against current docs.rs if a citation needs to be load-bearing in the spec.
- I did not run live web searches in this session (the deep-research sub-agent/web tools were not available in this thread); claims marked with library names are from documented practice, not freshly fetched pages. If you need fetched URLs attached, re-run with web search enabled.
- No measurement was done of `tracing`'s overhead vs `log` for adboost's frame hot path; `tracing` events behind a disabled level are cheap (level check first), but if the per-frame `trace!` in `reader_loop`/drain loop (`do_connect:530-535`) is extremely hot, confirm it's gated appropriately.
