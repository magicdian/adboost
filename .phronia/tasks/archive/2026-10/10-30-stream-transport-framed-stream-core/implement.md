# Implement: StreamTransport + shared framed_stream core

> Retrospective execution record. The work was implemented before the task was created (per the
> developer's direct request); this file records the ordered plan that was executed so the archive
> is reproducible.

## 1. Ordered checklist

1. **Inventory the existing seam.** Confirm `PersistentConnection<T>` is already transport-generic;
   confirm `FrameReadBuffer` is crate-private/sans-io and `FrameReader` / `write_frame_atomic` are
   private and `CurrentConnection`-specific.
2. **Extract `message_devices/framed_stream.rs`.**
   - `READ_CHUNK_LEN`, `WRITE_COMPLETION_TIMEOUT`, `not_connected()`.
   - `FrameReader<R: AsyncRead + Unpin>` with the per-read timeout loop and `into_reader()`.
   - `write_frame_atomic<W>` with the start-gate / completion-deadline contract and a `peer`
     label in error text.
   - `FramedStream<S>` with split halves, independent locks, `peer` label, read/write/shutdown,
     `take_stream()` for the TLS upgrade, and write poisoning.
   - Move the write-semantics unit tests here.
3. **Refactor `tcp/tcp_transport.rs`** to hold `Option<FramedStream<CurrentConnection>>`; delete
   the local `FrameReader`, `write_frame_atomic`, guards, constants. Keep `CurrentConnection`,
   `connect`, `disconnect`, the TLS upgrade, and all TCP tests. `upgrade_connection` uses
   `take_stream()` → TLS → new `FramedStream`.
4. **Add `message_devices/stream/`.**
   - `AsyncStream` (one non-auto trait for the trait object), `StreamConnection` alias.
   - `StreamTransport` with `new` / `from_tcp_stream(-> Result)` / `label`, `Clone` + manual
     `Debug`, `ADBTransport` (no-op connect, shutdown write), `ADBMessageTransport` (delegate
     read/write, `UpgradeError` on STLS).
   - Module README and `#![doc = include_str!(...)]`.
5. **Wire exports.** `pub mod stream;` in `message_devices/mod.rs`; re-export
   `AsyncStream`, `StreamConnection`, `StreamTransport`; crate root gets them via
   `pub use message_devices::*`.
6. **Add constructors** in `usb/persistent.rs`:
   `PersistentConnection<StreamTransport>::{from_stream, from_stream_with_features}`.
7. **Tests.**
   - `framed_stream`: start-gate recoverable, mid-frame stall fatal, whole frame, no-poison on
     start-gate, poison on mid-frame error.
   - `stream`: legacy handshake + shell echo over `duplex`, partial payload/partial header across
     timeout, concurrent read/write, transport-level start-gate and poison, two-session demux,
     oversize / unknown command / bad magic, STLS label.
8. **Spec + docs.** Update `directory-structure.md` (module map, shared-driver rule) and
   `adb-wire-protocol-contract.md` (lock ownership, STLS third case); add the README module row.
9. **Quality gate.** fmt, clippy (all features, changed files), tests (default + all features),
   CLI build, docs warning check for new files.

## 2. Validation commands

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo clippy -p adboost --all-features --all-targets -- -D warnings
cargo test -p adboost
cargo test -p adboost --all-features
cargo build -p adboost_cli
RUSTDOCFLAGS='-D warnings' cargo doc -p adboost --all-features --no-deps
```

## 3. Review gates

- [x] `FramedStream` owns the only split-half/poison implementation (no second copy).
- [x] TCP regression tests unchanged and green.
- [x] STLS returns `UpgradeError` and names the label.
- [x] No new crate dependency, feature, or MSRV bump.
- [x] Wire behavior untouched (only transport plumbing moved).
- [x] New public items documented; private-item doc links removed.

## 4. Verification results

```
cargo test -p adboost --all-features   → 426 passed, 0 failed (run twice)
cargo test -p adboost                  → 114 passed, 0 failed + doctests
cargo build -p adboost_cli             → ok
cargo fmt --all --check                → ok
clippy (changed files)                 → zero findings
clippy (repository)                    → pre-existing failures only, in untouched
                                         files (server/, usb/sim/, emulator/),
                                         from clippy 1.98 lints vs. the 1.88 baseline
```

Test-only caveat: the two `PersistentConnection` integration tests generate a 2048-bit RSA key
(no fixture exists). The 60 s `timeout` around them is a hang guard, not an assertion; every
deterministic framing test runs without a key.

## 5. Rollback points

- **Before step 3**: `framed_stream.rs` is additive; remove the module declaration and the file.
- **After step 3**: revert `tcp_transport.rs` together with `framed_stream.rs`; the old private
  implementation is in git history.
- **After step 6**: remove the `stream` module + the `persistent.rs` constructors + the
  `message_devices/mod.rs` re-exports; no other code references them.
