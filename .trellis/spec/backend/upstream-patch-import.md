# Upstream Patch Import

> How to import patches into this fork (`xp_adb_client`, forked from
> `cocool97/adb_client` at v3.2.2 and renamed).

---

## Why this exists

Upstream `adb_client` rejected our changes, so we forked v3.2.2 and develop
independently. Some changes still arrive as `.patch` files that were authored
against a **different upstream version** and/or a **different repo layout**
(e.g. the standalone-publish layout where `adb_client` is detached from the
workspace). Applying such patches naively corrupts our workspace or fails on
context drift. This document is the standard import procedure.

---

## Convention: Importing an upstream/external patch

**What**: When importing a `.patch` produced against a different version of
`adb_client`, apply only the **functional** hunks and re-derive any hunk that
fails on context drift. Never apply packaging/manifest hunks blindly.

**Why**: Patches authored for standalone publishing rewrite `Cargo.toml` to
detach the crate from the workspace; our fork keeps the workspace. Patches
authored against an older version assume an older module layout / enum shape,
so some hunks fail to apply at the recorded line context.

### Procedure

1. **Read the whole patch first.** Identify which hunks are functional code vs.
   packaging/manifest (`Cargo.toml`, lints, version).

2. **Dry-run the apply** from the repo root:
   ```bash
   git apply --check --verbose <patch>
   ```
   Note which files apply cleanly and which fail with
   `patch does not apply` (= context drift vs. our version).

3. **Apply functional hunks, excluding manifest + drifted files:**
   ```bash
   git apply \
     --exclude='adb_client/Cargo.toml' \
     --exclude='<each file that failed --check>' \
     <patch>
   ```

4. **Hand-port the excluded functional hunks.** For each drifted file, locate
   the intended addition in the patch and apply it manually at the correct spot
   in our current source.

5. **Resolve compile errors from layout/API drift.** Patch imports/call sites
   may reference an older module path or API shape. Find the correct v3.2.2
   path (`grep`), adjust imports/call sites, preserve behavior — do **not** stub
   out or delete functionality.

6. **Verify** (quality gate):
   ```bash
   cargo build -p adb_client --features usb
   cargo test  -p adb_client
   cargo clippy -p adb_client --features usb   # pedantic warnings on verbatim
                                               # patch code are acceptable
   cargo build -p adb_cli                      # dependents still build
   ```

---

## Don't: Apply the `Cargo.toml` hunk from a standalone-publish patch

**Problem**:
```diff
-authors.workspace = true
+authors = ["Corentin LIAUD"]
-edition.workspace = true
+edition = "2024"
-version.workspace = true
+version = "3.2.1"
-[lints]
-workspace = true
+[lints.clippy]
+pedantic = { level = "warn", priority = -1 }
```

**Why it's bad**: Our fork keeps the full workspace. Root `Cargo.toml` owns
`authors` / `edition` / `license` / `version` (3.2.2) / `workspace.lints` via
`*.workspace = true` inheritance in `adb_client/Cargo.toml`. Applying this hunk
detaches the crate, downgrades the version to 3.2.1, and duplicates lint config
— breaking sibling workspace members and version consistency.

**Instead**: Skip the hunk entirely (`--exclude='adb_client/Cargo.toml'`). Keep
workspace inheritance and version 3.2.2.

---

## Common Mistake: Enum/struct context drift across versions

**Symptom**: `git apply` reports `patch does not apply` on a file like
`adb_client/src/models/adb_local_command.rs`, even though the change itself is
trivial (e.g. add one enum variant).

**Cause**: The patch was authored against v3.2.1. Between v3.2.1 and v3.2.2 the
`ADBLocalCommand` enum gained a `#[cfg(feature = "framebuffer")] FrameBuffer`
variant after `Root`, so the patch's recorded context (`Root,` immediately
followed by `}`) no longer matches.

**Fix**: Hand-port the two intended additions:
- enum variant `TcpConnect(u16)` — placed after `Root,`, before the
  `#[cfg(feature = "framebuffer")]` block.
- `Display` arm `Self::TcpConnect(port) => write!(f, "tcp:{port}"),` — after
  `Self::Root => write!(f, "root:"),`.

**Prevention**: Always run `git apply --check` first and treat any
`does not apply` as "hand-port this functional hunk", not "the patch is broken".

---

## Reference: the `0001-xdb-usb-extensions` import

First application of this procedure. Imported these functional additions
(skipping `Cargo.toml`):

| Symbol | Location |
|---|---|
| `ADBSessionStream<T>` | `message_devices::session_stream` (new file, `Read+Write` over a session) |
| `PersistentUsbConnection`, `MultiplexedSession`, `SessionReadHalf`, `SessionWriteHalf`, `SessionChannels` | `message_devices::usb::persistent` (new file, session multiplexing over one CNXN+AUTH'd USB connection w/ background reader thread) |
| `ADBLocalCommand::TcpConnect(u16)` → `tcp:{port}` | `models::adb_local_command` (hand-ported) |
| `ADBUSBDevice::inner_mut()` | `message_devices::usb::adb_usb_device` |
| `ADBMessageDevice::open_session` made `pub` | `message_devices::adb_message_device` |
| `ADBMessageDevice` re-export | `message_devices::mod` |

The patch's persistent.rs imports happened to resolve cleanly against v3.2.2
(`rand::RngExt`, `crate::message_devices::models::{ADBRsaKey, read_adb_private_key}`,
`crate::utils::get_default_adb_key_path`, the `AUTH_*` consts) — no fixups
needed. Future patches may not be so lucky; follow step 5.
