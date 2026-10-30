# PRD: StreamTransport over arbitrary byte streams + shared framed_stream core

- **Source**: downstream feature request (`xpeng-debug-bridge` MTK8676 T-Box nested ADB) at
  `/private/tmp/adboost-streamtransport-feature-request.md`
- **Kind**: additive public API + internal refactor (no wire-protocol change)
- **Priority**: P2
- **Scope**: `adboost` crate — `message_devices/{framed_stream,stream,tcp,usb}` and the
  transport/framing sections of the backend spec

---

## 1. Background

`PersistentConnection<T: ADBMessageTransport>` already implements the full ADB device state
machine (CNXN, AUTH, multiplexed OPEN/OKAY/WRTE/CLSE, shell v1/v2, SYNC) generically over a
transport. Two transports exist: `USBTransport` and `TcpTransport`.

`TcpTransport` is hardwired to `tokio::net::TcpStream` through a private
`CurrentConnection { Tcp, Tls }` enum, and the load-bearing framing helpers are not reusable:

- `FrameReadBuffer` (`framed_read.rs`) is crate-private but sans-io and reusable.
- `FrameReader`, `write_frame_atomic`, `READ_CHUNK_LEN`, `WRITE_COMPLETION_TIMEOUT`, and the
  split-half lock/poison policy are private to `tcp_transport.rs` and type-specific.

A downstream caller cannot run the proven state machine over a stream that is not a `TcpStream`
(e.g. the raw byte pipe of another ADB device's `tcp:` local service, a `MultiplexedSession`, a
Unix socket, a TLS tunnel) without copying the protocol — which the downstream spec forbids,
because one misaligned frame tears down every session on the multiplexed connection.

## 2. Requirements

### R1 — `StreamTransport` (P0)

`ADBMessageTransport` over an **already-connected** full-duplex stream
(`AsyncRead + AsyncWrite + Unpin + Send + 'static`).

- `StreamTransport::new(stream, label)`; `connect()` is a no-op, `disconnect()` shuts down the
  write half.
- `from_tcp_stream(stream, label) -> Result<Self>` sets `TCP_NODELAY` and propagates failure.
- `label` appears in diagnostics only; never payload bytes.
- Must reuse `FrameReadBuffer` for all framing (no header parsing in the transport).

### R2 — Shared framing core (P0)

Extract the private TCP framing into a crate-internal driver used by **both** `TcpTransport` and
`StreamTransport`:

- `FrameReader<R: AsyncRead + Unpin>` — per-read timeout, cancel-safe chunk feeding.
- `write_frame_atomic<W: AsyncWrite + Unpin>` — start-gate `WriteTimeout` vs fatal truncation.
- `FramedStream<S>` — split halves, independent read/write locks, write-poison policy.
- `TcpTransport` keeps its `CurrentConnection` and TLS upgrade; behavior stays byte-identical.
- No transport may re-derive the lock topology or poison policy.

### R3 — Persistent constructors (P0)

`PersistentConnection<StreamTransport>::from_stream` and `from_stream_with_features`, mirroring
`new` / `new_with_features`. `PersistentConnection::new(StreamTransport::new(...), key)` must also
work.

### R4 — STLS must be refused (P0)

`upgrade_connection()` returns `RustADBError::UpgradeError` naming the stream label. Answering
STLS and continuing in plaintext is forbidden.

### R5 — Feature gating / compatibility (P1)

- `stream` transport module is always compiled (it does not depend on `usb`).
- The multiplexer constructors remain where `PersistentConnection` lives (under `usb`).
- No new cargo features, no new dependencies, no MSRV change.
- `TcpTransport`, `PersistentTcpConnection`, `PersistentUsbConnection` unchanged.

### R6 — Acceptance tests (P0)

Handshake over `tokio::io::duplex` (legacy banner → classic flow control), partial frame/header
across a read timeout, concurrent read/write, write start-gate recoverability, mid-frame poison,
two sessions demultiplexed by local id, oversize `data_length` / bad magic / unknown command,
STLS refusal with label. Existing TCP regression tests stay green.

## 3. Non-goals

- No TLS/STLS support for wrapped streams (caller wraps TLS before passing the stream).
- No change to `TcpTransport` TLS upgrade behavior.
- No new ADB protocol features; transport-only.
- No reconnect policy (caller-owned).
- No move of `PersistentConnection` out of the `usb` feature (tracked separately).

## 4. Acceptance criteria

- [x] `StreamTransport` + `AsyncStream` + `StreamConnection` public and re-exported at crate root.
- [x] `PersistentConnection::{from_stream, from_stream_with_features}` compile under `usb`.
- [x] `FramedStream` is the only split-half/poison implementation for TCP and stream.
- [x] STLS returns `UpgradeError` containing the label.
- [x] `cargo fmt --all --check` clean.
- [x] `cargo test -p adboost --all-features` green (426 tests).
- [x] `cargo test -p adboost` green (114 tests + doctests).
- [x] `cargo build -p adboost_cli` green.
- [x] Changed files produce zero clippy findings under `--all-features --all-targets -D warnings`
      (repository-wide clippy still fails on pre-existing lints in untouched files).
- [x] Spec updated: module layout + shared-driver rule; lock-invariant ownership; STLS case.

## 5. Evidence

See `implement.md` §4 and `research/stream-transport-neutral-design.md` §7.
