# Relicense fork to Apache-2.0 with upstream attribution + rewrite README

## Goal

This project (`adboost`) is a courteous fork of [`adb_client`](https://github.com/cocool97/adb_client)
at tag **v3.2.2**. We have already migrated `rusb` → `nusb` and added features
(persistent-connection server capabilities, etc.). We want to:

1. Relicense **our own contributions** under **Apache-2.0**, while remaining
   fully compliant with the upstream **MIT** license.
2. Rewrite the README (placeholder is acceptable for now) that clearly and
   gratefully credits the original project.

## What I already know

- Upstream license: **MIT**, `Copyright (c) 2023-2024 Corentin LIAUD`.
- MIT §2 requires the original copyright + permission notice to be preserved in
  all copies / substantial portions. We **cannot** simply delete it.
- MIT and Apache-2.0 are compatible: a fork may keep upstream MIT code under MIT
  and license new/modified contributions under Apache-2.0 (combined-work model).
- Workspace metadata (`Cargo.toml`): `license = "MIT"`, `authors = ["Corentin LIAUD"]`,
  `homepage`/`repository` → `cocool97/adb_client`, `version = "3.2.2"`.
- `adb_cli/Cargo.toml` line 43 has a stray hardcoded `license = "MIT"`.
- Files referencing upstream license/author: root `Cargo.toml`, root `README.md`,
  subcrate READMEs (`adb_client/`, `adb_cli/`, `pyadb_client/`).
- No existing `NOTICE` / `AUTHORS` file.
- git remote: `git@github.com:magicdian/adboost.git`.
- Contributors in history (top): Corentin LIAUD, cocool97, jdjingdian (us), + others.

## Decision (ADR-lite)

**Context**: Need to introduce Apache-2.0 without breaching upstream MIT.

**Decision**: Dual/combined licensing.
- Keep upstream MIT text in a dedicated file (e.g. `LICENSE-MIT`) preserving
  Corentin LIAUD's copyright.
- Add `LICENSE-APACHE` (Apache-2.0 full text) for our contributions.
- Add a `NOTICE` file (Apache-2.0 convention) crediting the upstream project.
- Document the licensing split in README.

**Consequences**: Compliant + courteous. Downstream consumers see both licenses.
Cargo `license` field becomes a SPDX expression.

## Resolved Decisions

- **Q1 License layout**: Single `LICENSE` = Apache-2.0 full text; `NOTICE` embeds
  the upstream MIT original text verbatim + grateful attribution. (MIT §2 satisfied
  because the notice ships in a distributed file.)
- **Q2 SPDX**: `Apache-2.0 AND MIT` (combined-work semantics — most accurate).
- **Q3 Cargo metadata**: Preserve original author info as much as possible. Keep
  `Corentin LIAUD` in authors (add `jdjingdian`). Point `repository`/`homepage` to
  the fork. Keep `version = 3.2.2` baseline. **Do NOT rename crates yet** — the
  rename to `adboost` + major refactor/trim happens in a later task.
- **Q4 README**: Grateful placeholder — explain the fork, thank upstream, note the
  nusb migration + licensing; leave feature/usage details as TODO.

## Requirements

- Preserve upstream MIT copyright notice verbatim (in `NOTICE`).
- Add Apache-2.0 as the primary `LICENSE`.
- `NOTICE` gratefully credits upstream `adb_client` / Corentin LIAUD.
- `Cargo.toml` workspace `license = "Apache-2.0 AND MIT"`; fix stray
  `license = "MIT"` in `adb_cli/Cargo.toml`.
- Keep original author, add fork author; repository/homepage → fork.
- README rewritten as grateful placeholder.

## Acceptance Criteria

- [ ] Upstream MIT notice preserved verbatim in `NOTICE`.
- [ ] `LICENSE` contains Apache-2.0 full text.
- [ ] `NOTICE` credits upstream project + author.
- [ ] Workspace `Cargo.toml` `license = "Apache-2.0 AND MIT"`, authors + repo updated.
- [ ] Stray `adb_cli/Cargo.toml` `license = "MIT"` removed/aligned to workspace.
- [ ] README rewritten with explicit thanks + fork explanation + TODO markers.
- [ ] `cargo metadata --no-deps` resolves (valid SPDX, no broken field).

## Out of Scope

- Trellis tooling files under `.cursor/.qoder/.opencode/...` (false-positive grep hits).
- Re-licensing or removing any third-party dependency licenses.
- Publishing to crates.io.

## Technical Notes

- Files to touch: `LICENSE` → split, new `LICENSE-APACHE`, new `NOTICE`,
  `Cargo.toml`, `adb_cli/Cargo.toml` (stray license line), `README.md`,
  subcrate READMEs (optional).
