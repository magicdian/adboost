# Fix `select_tport` any-branch error message for multi-device

## Goal

When multiple devices are connected (USB server mode) and a user runs `adb shell`
(or `adb forward --remove/--list`, `adb reverse --list`) **without `-s`**, the
server replies `error: device not found`. The AOSP-correct message is
`more than one device`. (Requiring `-s` with multiple devices is correct
behavior — only the error *message* is wrong/misleading.)

## What I already know (root cause confirmed by code inspection)

Modern `adb` selects a transport via `host:tport:any` (the 8-byte-id variant)
**before** sending the local service (`shell:`) or the forward/reverse command.
The bug is in `select_tport` at `adb_client/src/server/frontend.rs:541-581`:

```rust
let chosen = if rest.is_empty() || rest == "any" || rest == "-any" {
    match devices.as_slice() {
        [one] => Some(one.serial.clone()),
        _ => None,                 // [] AND multi-device both collapse to None
    }
}
...
} else {
    stream.write_all(&protocol::fail("device not found")).await?;  // wrong msg
}
```

All three failure modes (no devices / multiple devices / serial-or-id not found)
collapse into a single `Option`, so they all report `device not found`.

By contrast `select_transport_any` (`frontend.rs:477-495`) is correct:
`[] => "no devices"`, `[one] => ok`, `_ => "more than one device"`.

The forward path via `serve_host_forward` → `resolve_single_serial`
(`frontend.rs:341-364`) is *also* correct on its own, but the real client never
reaches it without first doing `tport:any`, so the wrong message surfaces there
too. **Single root cause: `select_tport`'s `any`/empty branch.**

## Requirements

* `host:tport:any` (and empty `tport:`) with **0 devices** → `FAIL("no devices")`
* `host:tport:any` with **>1 devices** → `FAIL("more than one device")`
* `host:tport:any` with **exactly 1 device** → unchanged (OKAY + 8-byte LE id)
* `host:tport:serial:<X>` / `host:tport:<X>` not found → unchanged (`device not found`)
* `host:tport:id:<N>` / `-id:<N>` not found → `FAIL("no device for transport id")`
  to match `select_transport_by_id` (`frontend.rs:532`)
* Invalid id (non-numeric) → keep a clear failure (currently falls through to the
  generic message; align with `select_transport_by_id`'s `"invalid transport id"`)

## Acceptance Criteria

* [x] New unit test: `tport_any_with_multiple_devices_fails_more_than_one`
* [x] New unit test: `tport_any_with_no_devices_fails_no_devices`
* [x] Existing single-device `tport` test still passes
* [x] `tport:serial:<unknown>` still returns `device not found`
* [x] `tport:id:<unknown>` returns `no device for transport id`
* [x] New **selftest** parity case: in the multi-device scenario, run official
  `adb -P <port> shell` **without `-s`** against adboost's in-process server and
  assert the error contains `more than one device` (and NOT `device not found`).
  Skipped when single-device or no `adb` binary — matching existing parity cases.
* [x] `cargo test`, `cargo clippy`, `cargo fmt --check` green

## Definition of Done

* Tests added for multi-device and no-device tport paths
* Lint / typecheck / tests green
* No behavior change for the single-device happy path

## Technical Approach

Rewrite `select_tport` so each selector branch carries its own AOSP-correct error
instead of collapsing to a shared `Option` → generic `device not found`. The
cleanest shape: have each branch resolve to a `Result<String, &'static str>`
(serial on success, error message on failure), then a single match writes
`okay_tport(id)` or `fail(msg)`. The `any` branch reuses the same 3-way logic as
`select_transport_any`.

## Out of Scope

* Changing `select_transport_any`, `resolve_single_serial`, or the
  `host:transport*` (non-tport) paths — they are already correct.
* Any client-side / CLI changes; this is purely the server frontend reply.

## Technical Notes

* File: `adb_client/src/server/frontend.rs`
  * `select_tport` — `541-581` (the fix)
  * `select_transport_any` — `477-495` (reference for correct 3-way)
  * `select_transport_by_id` — `516-536` (reference for id error messages)
  * existing tport/transport tests — ~`861-1313`
* `protocol::okay_tport`, `protocol::fail`, `protocol::transport_id_for*`.
