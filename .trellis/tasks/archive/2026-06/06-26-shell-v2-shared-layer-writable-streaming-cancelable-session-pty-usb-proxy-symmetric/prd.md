# shell-v2 shared layer: writable / streaming / cancelable session + PTY (USB + proxy symmetric)

## Goal

xdb needs, on the Android shell path, a v2 session that can **write stdin**, **close-stdin**,
**stream stdout/stderr/exit (no buffer-until-exit)**, **allocate a PTY**, and be **canceled**
(local disconnect → device-side process gets EOF/SIGHUP and exits cleanly). It needs this on
**both** transports — USB direct and proxy(:5037) — with symmetric semantics.

Rather than bolting these four capabilities onto two divergent code paths, this task converges
shell-v2 into **one shared layer** so USB and proxy physically share the same codec + session
logic, eliminating the existing duplication (two `ShellChannel` enums, two decoders) that is the
root cause of the recurring "async path lacks a guarantee the USB path already has" bug class.

## What I already know (from source analysis @ cd2e242)

- **Split read/write already exists**: `MultiplexedSession::into_split()` → `SessionReadHalf`
  (`impl AsyncRead`) + `SessionWriteHalf` (`impl AsyncWrite`, full windowed flow control).
  `persistent.rs:2409`, `:2820`. The A1 "split read/write" base does **not** need to be built.
- **`ShellV2Session` is read-only today**: holds the whole `MultiplexedSession` and does
  buffer-until-exit in `execute()` (`shell_v2_session.rs:143`); Stdin/CloseStdin are
  consume-and-ignore (`:191`).
- **CLSE-on-close/drop already present**: `MultiplexedSession::close()` (`persistent.rs:2389`),
  `PersistentConnection::close()` (`:2125`); `poll_read` is documented cancel-safe (P1-③).
  → "drop before exit frame without panic" is already guaranteed by existing Drop paths.
- **proxy can already write the device + cancel**: `get_raw_connection() -> &mut TcpStream`
  (`tcp_proxy_transport.rs:88`); `bidirectional_session` (`adb_proxy_device_commands.rs:221`)
  already does `tokio::io::split` + `select!` to drive both directions. drop TcpStream → EOF.
- **Two duplicated `ShellChannel` enums**: USB `shell_v2_session.rs:45` (ids 0..5, full) vs
  proxy `adb_proxy_device_commands.rs:15` (ids 1..3 only). Two decoders:
  `ShellV2Session::execute` vs `decode_shell_v2_stream` (`:296`).
- **PTY gap is mis-described by xdb**: AOSP service grammar is `shell[,v2][,TERM=…][,pty|raw]:cmd`
  — `pty` and `raw` are the **mutually-exclusive final segment**. Current renderer hardcodes
  `,raw:` (`adb_local_command.rs:53`) and uses a stringly-typed `Vec<String>`. So PTY cannot be a
  string pushed into args; the renderer must change. shell-v2 has **no signal frame** (ids 0..5 =
  stdin/stdout/stderr/exit/close-stdin/window-size); "signal" = close-stdin EOF or PTY HUP.
- **selftest exists** (`adboost_cli/src/selftest/`): `persistent_shell_v2` already exercises
  `open_shell_v2().execute()` (`cases.rs:44`); cases wired in `mod.rs` (`run_usb_direct_suite`,
  `run_suite`). Interactive phase exists for human-in-loop (replug/reboot).
- **sim harness** (`message_devices/usb/sim/`): byte-level; `Scenario::with_first_write_reply`
  can inject a crafted device→host frame; no shell-v2 frame producer yet.

## Requirements (evolving)

- R1 — Single shared shell-v2 codec: one `ShellChannel` (ids 0..5) + `encode(id,&[u8])` /
  `decode_header`. Delete the two duplicated enums/decoders.
- R2 — Transport-generic writable/streaming session over an `AsyncRead`+`AsyncWrite` pair:
  `read_frame()` (streaming), `write_stdin()`, `close_stdin()`, `close()`. USB feeds it
  `into_split()`, proxy feeds it `tokio::io::split`.
- R3 — Structured shell service-string builder replacing hardcoded `,raw:` + `Vec<String>`:
  expresses `{v1|v2, TERM, Pty|Raw, cmd}`; `pty`/`raw` mutually exclusive at the type level;
  keep `Raw(String)` verbatim passthrough for the server frontend.
- R4 — `PersistentUsbConnection` + proxy expose the new session; back-compat `execute()` becomes
  a thin "loop read_frame until exit" wrapper. `shell_exec` (v1) unchanged.
- R5 — Tests: codec encode/decode unit tests; streaming + mid-stream-drop (no panic) via sim
  injected frames; PTY service-string render unit test. Regressions land in the sim net.
- R6 — (TBD scope) extend `adboost_cli` selftest to cover write_stdin/close_stdin/streaming
  (and PTY on real device).

## Acceptance Criteria

- [x] One `ShellChannel`/codec; both duplicated copies removed; `cargo build`/clippy green. (S1)
- [x] `write_stdin`/`close_stdin` frame encoding correct (codec + session unit tests). (S1, S3)
- [x] Streaming read yields frames incrementally; drop/cancel before exit frame does not panic.
      (S3 unit tests + S5 end-to-end over real sim session). (S3, S5)
- [x] PTY service string renders `shell,v2,…,pty:` (not `…,pty,raw:`); unit test asserts grammar. (S2)
- [x] proxy v2 cancelable: `open_shell_v2_service` owns the socket, drop → TCP closed → device EOF. (S4)
- [x] Regression: `shell_exec` v1 and `ShellV2Session::execute` behavior unchanged (full suite green). (all)
- [x] selftest cases for stdin write / close-stdin / streaming (automated) + interactive PTY-HUP. (S6)
- [x] Real-device PTY-HUP verification: executable interactive case + documented procedure. (S6, S7)
      *Note: the MTK 8676 hardware PASS is an operator step (not CI) — run `adboost_cli selftest`.*

## Definition of Done

- Tests added (codec unit, sim streaming/drop, service-string render).
- Lint / typecheck / CI green.
- Doc comments updated where the session contract changes.
- One bug/capability = one task = one commit decomposition preserved.

## Decisions (ADR-lite)

- **D1 — Full convergence now.** One shared codec + one transport-generic writable/streaming
  session serving both USB and proxy; both duplicated `ShellChannel` enums and both decoders
  deleted. *Consequence:* highest upfront churn, but USB/proxy symmetry becomes structural
  (shared code) instead of copied — directly attacks the [[tcp-async-path-missing-usb-guarantees]]
  bug class.
- **D2 — Full PTY incl. real-device verification.** Typed service-string builder (pty/raw
  mutually exclusive) + correct `shell,v2,…,pty:` render + **a real-device MTK 8676 gate**
  (tcpdump child exits + flush on host close). *Consequence:* the task is not Done until verified
  on hardware; the hardware gate lives in the interactive selftest phase and cannot pass in CI —
  CI covers everything up to (not including) the HUP-on-real-hardware assertion.
- **D3 — Automated stdin/streaming cases + interactive PTY.** USB-direct automated cases for
  `write_stdin`/`close_stdin`/streaming; PTY-HUP / cancel→device-exit under the existing
  interactive phase (real device). Matches the current selftest automated/interactive split.

## Implementation Plan (subtasks → small commits)

Sequenced by dependency; each is one focused commit.

- **S1 — Shared shell-v2 codec** (R1): extract one `ShellChannel` (ids 0..5) + `encode(id,&[u8])`
  / `decode_header` into a shared module; rewrite USB `ShellV2Session::execute` and proxy
  `decode_shell_v2_stream` on top of it; delete both duplicates. Pure refactor, behavior
  unchanged. Tests: codec unit tests. *(foundation, no transport dep)*
- **S2 — Typed shell service-string builder** (R3): replace `ShellCommand(String, Vec<String>)`
  + hardcoded `,raw:` with a typed builder expressing `{v1|v2, TERM, Pty|Raw, cmd}`; keep
  `Raw(String)` verbatim. Render unit tests incl. `…,pty:` grammar. *(independent of S1)*
- **S3 — Transport-generic writable/streaming session** (R2, R4 USB side): `ShellV2Session`
  generic over split `AsyncRead`+`AsyncWrite` halves with `read_frame` / `write_stdin` /
  `close_stdin` / `close`; `execute()` becomes a thin loop-until-exit wrapper. USB feeds
  `into_split()`; `open_shell_v2` gains a PTY-capable opener. *(depends on S1, S2)*
- **S4 — proxy symmetric session** (R2, R4 proxy side): drive the generic session via
  `tokio::io::split` over the `RawConnection`; cancelable by drop → TCP close → device EOF.
  *(depends on S1, S3)*
- **S5 — sim shell-v2 frame producer + regressions** (R5): teach the sim to emit shell-v2
  frames so streaming + mid-stream-drop (no panic) can be asserted; land regressions in the net.
  *(depends on S1)*
- **S6 — selftest extension** (R6, D3): automated USB-direct `write_stdin`/`close_stdin`/
  streaming cases (e.g. `cat` round-trip: write → read echoed frames → close-stdin → exit);
  interactive PTY-HUP / cancel→device-exit case. *(depends on S3, S4, S5)*
- **S7 — real-device PTY-HUP verification gate** (D2): MTK 8676 manual/interactive validation
  (tcpdump child exits + flush on host close); documented procedure + interactive selftest case.
  *(depends on S6; hardware-gated, not CI)*

## Out of Scope

- shell-v2 window-size resize UX beyond encoding the frame.
- xdb-side T1→T6 consumption.
- Non-MTK platform PTY-HUP behavior (only MTK 8676 Android 16 is the verification target).

## Technical Notes

- Files: `message_devices/usb/shell_v2_session.rs`, `message_devices/usb/persistent.rs`,
  `models/adb_local_command.rs`, `proxy/adb_proxy_device_commands.rs`,
  `message_devices/usb/sim/*`, `adboost_cli/src/selftest/*`.
- Related memory: [[tcp-async-path-missing-usb-guarantees]], [[host-protocol-parity-gaps]],
  [[prefer-root-cause-fix-at-contract-layer]], [[sim-harness-regression-net]].
