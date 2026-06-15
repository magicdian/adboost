# Rename `adb_client` crate → `adboost` (library + CLI; exclude Python)

## Goal

The project is branded **adboost** everywhere (workspace, repo, CLI binary,
daemon pid files) **except the core library crate, which is still named
`adb_client`**. This half-renamed state is the single biggest source of
confusion in the codebase. Upstream patch-pulling is no longer a real workflow
(fully detached fork, async rewrite, reshaped API — see analysis in the
brainstorm), so there is no reason to keep the upstream crate name as a
compatibility anchor.

This task completes the rename of the **library crate** `adb_client` → `adboost`
and updates everything on the **main line** (the two active workspace members:
the library + `adboost_cli`) to match. `pyadb_client` and `examples/mdns` are
explicitly **not** part of this task (not on the main line, already broken
against the current API).

## What I already know (from repo inspection)

- Active workspace members (root `Cargo.toml`): `adb_client`, `adboost_cli`.
  `pyadb_client` and `examples/mdns` are already excluded.
- Library crate: dir `adb_client/`, `[package] name = "adb_client"`.
- `adboost_cli` references `adb_client::*` in **16 `.rs` files** (verified).
- Root `Cargo.toml` has `[patch.crates-io] adb_client = { path = "./adb_client" }`
  — this exists to override the *published upstream* `adb_client`; once our crate
  is named `adboost` we no longer depend on a published `adb_client`, so this
  stanza becomes meaningless and should be removed.
- Handshake host identity string is `adb_client@<version>`
  (`adb_client/src/message_devices/models/adb_rsa_key.rs:104`) — shown in the
  device "allow USB debugging" dialog.
- Release CI publishes with `cargo publish -p adb_client`
  (`.github/workflows/rust-release.yml:33`).
- Bench at `benches/benchmark_adb_push.rs` uses `adb_client::proxy::*` + a
  `BenchmarkId::new("adb_client", "push")`; bench `path` in library Cargo.toml is
  `../benches/benchmark_adb_push.rs`.
- Log-target docs reference `RUST_LOG=adb_client=...` in `lib.rs` rustdoc and
  `adboost_cli/README.md`.
- `.trellis/spec/backend/*.md` describe the workspace with the old crate/dir name
  and note `adboost` is "reserved for the future rename".

## Decisions (locked via brainstorm)

| # | Decision | Choice |
|---|----------|--------|
| 1 | Rename the directory too? | **Yes** — `git mv adb_client adboost`, plus `name = "adboost"`. Cleanest; git records it as a rename. |
| 2 | Handshake host identity `adb_client@` | **Change to `adboost@`**. One-time effect: already-authorized devices may re-prompt for USB debugging (host name changed). Acceptable. |
| 3 | CLI command name | **Keep `adboost_cli`** unchanged. This task is a *library* rename only; no new `[[bin]]`, no deb/rpm/docs churn. |
| 4 | Publish positioning | **Prepare for future `adboost` crates.io publish.** Resolve the now-meaningless `[patch.crates-io] adb_client` stanza (expected: remove it) and ensure the renamed crate carries valid publish metadata. |
| 5 | `pyadb_client` / `examples/mdns` | **Do not touch at all.** They are off the main line and already broken against the current API; their `../adb_client` path deps will dangle after the dir rename, but main-line workspace build is unaffected (they're not members). Left for their own future migration task. |

## Requirements

**R1 — Library crate rename**
- `git mv adb_client adboost` (directory).
- `adboost/Cargo.toml`: `name = "adboost"`. Keep `readme = "README.md"`.
- Fix the bench `[[bench]] path` so it still resolves (`../benches/...` is
  relative to the crate dir; verify after move).

**R2 — Workspace root `Cargo.toml`**
- `members = ["adboost", "adboost_cli"]`.
- Update the top-of-file comment that names `adb_client`.
- Remove the `[patch.crates-io] adb_client = { path = "./adb_client" }` stanza
  (meaningless post-rename — confirm during implementation; if a patch is still
  wanted it would have to target `adboost`, but there is no published `adboost`
  to override, so removal is correct).

**R3 — `adboost_cli` source + manifest**
- All 16 `.rs` files: `use adb_client::…` / `adb_client::…` → `adboost::…`.
- `adboost_cli/Cargo.toml`: path dep `adb_client = { path = "../adb_client", … }`
  → `adboost = { path = "../adboost", … }` (features unchanged).
- Update the dev-dep comment referencing `adb_client`.
- `adboost_cli/README.md`: crate-name references + `RUST_LOG=adb_client=…`
  examples → `adboost`. (Keep `cocool97/adb_client` attribution links intact.)
- Retarget the bug-report URL in `adb_cli_error.rs` (and any other issue-tracker
  link) from `cocool97/adb_client/issues` → `magicdian/adboost/issues`.

**R4 — Library internal strings / docs**
- `RUST_LOG=adb_client=…` examples in `adboost/src/lib.rs` rustdoc → `adboost`.
- Handshake identity `adb_client@{version}` → `adboost@{version}`
  (`adb_rsa_key.rs`), plus any test asserting on it.
- Doc-comment examples (`# use adb_client::…` in rustdoc, e.g.
  `reverse_engine.rs`) → `adboost::…` so doctests compile.
- `adboost/README.md`: `docs.rs/adb_client` badge → `docs.rs/adboost`; crate-name
  mentions in tables → `adboost`. **Preserve** upstream attribution links to
  `github.com/cocool97/adb_client`.

**R5 — Benches**
- `benches/benchmark_adb_push.rs`: `use adb_client::…` → `adboost::…`; update
  `BenchmarkId::new("adb_client", …)` label and the doc comments.

**R6 — CI**
- `.github/workflows/rust-release.yml`: `cargo publish -p adb_client` →
  `-p adboost`.

**R7 — Spec docs (DoD-level sync)**
- Update `.trellis/spec/backend/*.md` descriptive references to the crate
  name/dir (`adb_client/`, "reserved for future rename" note, tree diagrams) to
  reflect the completed rename. Keep `cocool97/adb_client` upstream-attribution
  references unchanged.

## Acceptance Criteria

- [ ] `cargo build` (workspace) green: members `adboost` + `adboost_cli`.
- [ ] `cargo build -p adboost --features server` (and `usb`, `mdns`) green.
- [ ] `cargo test -p adboost` + `cargo test -p adboost_cli` green.
- [ ] `cargo test --doc -p adboost` green (rustdoc examples updated).
- [ ] `cargo clippy --workspace --all-targets` — 0 warnings (pedantic, per repo bar).
- [ ] `cargo bench --no-run` (or build) resolves the renamed bench import.
- [ ] No remaining `adb_client` references on the main line **except** intentional
      upstream-attribution/thanks links to `github.com/cocool97/adb_client`. Verify:
      `grep -rn "adb_client" adboost adboost_cli benches .github | grep -v "cocool97/adb_client"` → empty.
- [ ] No issue-tracker/bug-report link points at `cocool97/adb_client/issues`;
      they target `magicdian/adboost/issues`. Verify:
      `grep -rn "cocool97/adb_client/issues" .` → empty.
- [ ] Handshake identity emits `adboost@<version>` (unit test / doctest asserts).
- [ ] `git status` shows the directory move recorded as a rename (history preserved).

## Definition of Done

- All acceptance criteria green.
- `trellis-check` passes (lint / type / test / spec compliance).
- Spec docs (`.trellis/spec/backend/*`) updated to the new name (R7).
- Journal entry recorded.

## Out of Scope (explicit)

- `pyadb_client` — not touched at all (R5 decision). Its `../adb_client` path dep
  will dangle; that crate is already broken against the current API and is a
  separate future migration.
- `examples/mdns` — not touched at all (same rationale).
- CLI binary/command rename (`adboost_cli` stays; no `[[bin]] name = "adboost"`).
- deb/rpm packaging changes (follow from binary name, which is unchanged).
- Actually publishing to crates.io (we only *prepare* metadata; no publish run).
- Fixing the broken old-API references in pyadb/examples (`server_device::`, etc).
- Any upstream-attribution link rewrites (those `cocool97/adb_client` links stay).

## Technical Notes

- Sequence matters: do the `git mv` first, then fix all `path = "../adb_client"`
  / `"./adb_client"` deps, then the `use` rewrites, then docs/CI/spec. Build after
  each cluster to localize breakage.
- The 16 cli `.rs` files (from grep): `utils.rs`, `daemon.rs`, `main.rs`,
  `selftest/{reverse_cases,cases,channels,interactive,mod}.rs`,
  `models/{host,adb_cli_error,reboot_type,persistent}.rs`,
  `handlers/{host_commands,persistent_command,local_commands,emulator_commands}.rs`.
- `adb_cli_error.rs:21` contains a `cocool97/adb_client/issues` URL — **retarget
  to `github.com/magicdian/adboost/issues`** (decided): bug reports must go to our
  repo, not disturb the upstream project. Sweep for any other `.../issues` link
  pointing at upstream and retarget those too.
- Watch the **three** distinct `adb_client` reference classes:
  1. **crate-name** `adb_client` → rename to `adboost`.
  2. **upstream-attribution / thanks** links to `github.com/cocool97/adb_client`
     (and license/NOTICE credit) → **preserve as-is** (courtesy).
  3. **issue-tracker / bug-report** links pointing at `cocool97/adb_client/issues`
     → **retarget to `magicdian/adboost/issues`** (don't route our bugs upstream).
  The acceptance grep allows class 2 only.

## Research References

(none — this is a mechanical rename with locked decisions; no external research
needed.)
