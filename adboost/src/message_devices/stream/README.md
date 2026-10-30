# Stream transport (nested / layered ADB links)

`StreamTransport` runs the **device** side of the ADB wire protocol over an
already-connected, full-duplex byte stream — anything that implements
`AsyncRead + AsyncWrite + Unpin + Send + 'static`:

- the raw byte pipe of another ADB device's local service
  (`host:transport:<serial>` + `tcp:<port>`, or
  `PersistentUsbConnection::open_session(TcpConnect(port))`),
- a `tokio::io::duplex` pair (tests / in-process relays),
- a Unix socket, a TLS tunnel established elsewhere, etc.

The point is *reuse*: `PersistentConnection<T>` is already transport-generic, so
wrapping a stream in `StreamTransport` gives you CNXN/AUTH, shell v1/v2,
`sync:`, multiplexing, and the reverse-open queue without re-implementing a
single frame.

```rust,ignore
let conn = adboost::usb::PersistentConnection::from_stream(pipe, None).await?;
let session = conn
    .open_session(&adboost::ADBLocalCommand::ShellCommand("id".into()))
    .await?;
```

## Contract

`StreamTransport::new(stream, label)` means **already connected**:
`ADBTransport::connect()` is a no-op, and the caller owns reconnection.

All framing invariants are shared with `TcpTransport` through
`message_devices::framed_stream` (cancel-safe reads, frame-atomic writes,
independent read/write locks, write poisoning). In particular:

- a `ReadTimeout` is only ever observed between whole frames; bytes already
  received stay buffered,
- a write that stalls before any byte is committed is the recoverable
  `WriteTimeout`; a write that fails mid-frame poisons the write half,
- `disconnect()` shuts down the write half (the caller still owns the read side
  of its pipe).

`STLS` (the device-requested TLS upgrade) is **refused** with
`RustADBError::UpgradeError`, naming the stream label. A wrapped stream cannot be
transparently re-established as TLS — silently continuing in plaintext after
answering `STLS` would be a protocol lie. Callers that need TLS should establish
it before wrapping the stream (in which case the upgrade is unnecessary).

## Diagnostics

The `label` appears in error/trace messages (e.g. `stream transport "tbox@YTG"`).
It must never contain payload bytes. Use `PersistentConnection::new(
StreamTransport::new(stream, label), key)` when you need a custom label;
`PersistentConnection::from_stream` uses the default label `"stream"`.
