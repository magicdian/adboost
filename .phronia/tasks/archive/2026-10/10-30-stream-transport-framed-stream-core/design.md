# Design: StreamTransport + shared framed_stream core

## 1. Problem shape

Two axes were tangled in `tcp_transport.rs`:

1. **Framing + connection lifecycle**: how bytes become frames, timeouts, lock topology, write
   poisoning. This is transport-neutral for any `AsyncRead + AsyncWrite`.
2. **TCP specifics**: `SocketAddr` connect, `TCP_NODELAY`, the `CurrentConnection` TCP/TLS
   enum, the rustls upgrade.

`StreamTransport` needs axis 1 and must not copy it. The design therefore splits the file rather
than parameterizing `TcpTransport`.

## 2. Layering

```
adb_transport_message        wire types, HEADER_LENGTH, MAX_PAYLOAD, integrity
        │
framed_read                  sans-io FrameReadBuffer:
        │                    header decode → data_length bound → payload assembly →
        │                    magic check; consumes only complete frames
        ▼
framed_stream                async driver:
        │                    FrameReader<R>  (per-read timeout, persistent buffer)
        │                    write_frame_atomic<W> (start gate vs. truncation)
        │                    FramedStream<S>  (split halves, independent locks, poison)
        ▼
tcp/       stream/           policy only:
        │                    connect / TLS upgrade / label / STLS refusal
        ▼
usb/persistent               PersistentConnection<T>: CNXN/AUTH, sessions, flow control
```

Rule: a new `AsyncRead + AsyncWrite` transport is a thin adapter over `FramedStream`.

## 3. Contracts

### 3.1 `FrameReader<R>`

- Owns `R`, a `FrameReadBuffer`, and a reusable `Box<[u8]>` scratch.
- `read_message(timeout, peer)` loops: `try_parse` first; otherwise one `read()` wrapped in
  `tokio::time::timeout`.
- `ReadTimeout` is returned only when the timed-out read made no progress; bytes from previous
  reads are already in the buffer. A timed-out read that delivered bytes returns via
  `Ok(res)` and is pushed before looping.
- EOF (`n == 0`) → `UnexpectedEof` with `"{peer} connection closed by peer"`.

### 3.2 `write_frame_atomic<W>`

- Serializes header + payload into one buffer (one contiguous write stream).
- The first `write` uses `start_timeout`; subsequent writes and `flush` use
  `completion_timeout` (10 s constant).
- Outcomes: `Ok`, `WriteTimeout` (nothing committed, recoverable), other `Err` (truncation or
  IO failure, fatal).
- `Ok(0)` is `WriteZero` (fatal), not `WriteTimeout`.

### 3.3 `FramedStream<S>`

- `tokio::io::split(stream)` at construction.
- `read_half: Arc<Mutex<Option<FrameReader<ReadHalf<S>>>>>`,
  `write_half: Arc<Mutex<Option<WriteHalf<S>>>>`, `peer: Arc<str>`.
- Clone shares the Arcs. `Debug` prints only `peer` (`finish_non_exhaustive`).
- `read_message` / `write_message` lock only their own half.
- `write_message` sets the write half to `None` on any non-`WriteTimeout` error.
- `take_stream()` locks both halves and `unsplit`s them back (used only by the TCP TLS
  upgrade); the caller must install a fresh `FramedStream`.
- `shutdown()` shuts down the write half, best-effort.

### 3.4 `StreamTransport`

- Holds `FramedStream<StreamConnection>` and nothing else; the peer label lives in the driver.
- `AsyncStream` exists because a trait object may name only one non-auto trait.
- `connect` = ready future, `disconnect` = shutdown, `upgrade_connection` = `UpgradeError`.
- `from_tcp_stream` sets `TCP_NODELAY` and returns `Result`.

### 3.5 `PersistentConnection` constructors

`from_stream` delegates to `from_stream_with_features` with `DeviceFeatureSet::default()`;
that delegates to `new_with_features(StreamTransport::new(stream, "stream"), key, features)`.
Custom labels use `new(StreamTransport::new(stream, label), key)`.

## 4. Decisions and rejected alternatives

| Decision | Alternative | Why |
|---|---|---|
| Shared `FramedStream` core | Only genericize `FrameReader` (original FR §5) | Would duplicate split locks + poison policy in `StreamTransport`; the invariants would drift. |
| `stream` module always compiled | `#[cfg(feature = "usb")] pub mod stream` (FR preferred) | The transport needs no USB symbols; only `PersistentConnection` does. Gating the transport on `usb` is the wrong layer. |
| `from_stream` lives under `usb` | Move `PersistentConnection` to a neutral module now | Moving ~4000 lines + re-exports is a separate refactor; keeping it avoids an unreviewable diff. |
| `from_tcp_stream -> Result` | `-> Self` (FR) | Silently ignoring `set_nodelay` failure is a correctness lie. |
| STLS → `UpgradeError` | Default no-op (trait default) | A no-op would answer STLS and continue cleartext. |
| Boxed `dyn AsyncStream` | Generic `StreamTransport<T>` | Transport must be `Clone + Send + 'static` and erased for the multiplexer; a generic type would leak `T` through every constructor and alias. |
| Fatal-path test uses immediate mock error | Wait out the 10 s completion deadline | Keeps tests fast; the timer branch is unit-tested with explicit deadlines. |

## 5. Invariants preserved (must remain true)

1. A read timeout is observed only between whole frames; buffered bytes survive cancellation.
2. A frame is never partially written followed by another frame.
3. A blocked read never holds a lock the writer needs.
4. Write poisoning is fatal, start-gate timeout is recoverable.
5. Framing (header decode, bound, magic) runs only in `FrameReadBuffer`.
6. TCP wire behavior and error text are unchanged.

## 6. Compatibility / rollout / rollback

- Additive API; no dependency/feature/MSRV change.
- Downstream: bump pinned rev, refresh lockfile, replace the temporary loopback bridge with
  `from_stream` / `from_tcp_stream`.
- Rollback: revert the working tree; the TCP framing still exists in git history. The only
  cross-module edge to remove is `persistent.rs → stream::StreamTransport`.

## 7. Risks

See `research/stream-transport-neutral-design.md` §5 for the full risk register (API naming,
dyn dispatch, STLS limitation, `usb` coupling, fixed write deadline, debug-build keygen cost,
`tokio::io::split` semantics, TLS-upgrade half replacement, error-variant coupling).
