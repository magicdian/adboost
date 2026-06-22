# Fix TcpTransport missing TCP_NODELAY (interactive shell lag)

## Goal

`TcpTransport::connect()` builds a `TcpStream` but never disables Nagle's
algorithm. ADB multiplexing over TCP/IP is small-packet + interactive (each
keystroke in `adb shell` produces a tiny TCP segment that waits for the prior
segment's ACK), so Nagle stacks an RTT-scale delay onto every keystroke — visible
lag in interactive shells reached via `adb connect <host:port>`. Set
`TCP_NODELAY` on connect to match what every real ADB/SSH client does.

## What I already know (verified in source)

* `adboost/src/message_devices/tcp/tcp_transport.rs:148-152` — `connect()` does a
  bare `TcpStream::connect(...).await?` and stores it; **no `set_nodelay`**.
* `grep nodelay` across `message_devices/tcp/` is empty → Nagle stays default-on.
* The TLS upgrade path (`upgrade_connection`, line 248-308) consumes the *same*
  `TcpStream` into `TlsConnector::connect`; setting nodelay on the plain stream at
  `connect()` time covers the TLS case too (the option lives on the kernel socket,
  survives the TLS layering).
* USB transport doesn't use TCP → unaffected.
* Reference: xdb's own SSH client sets `nodelay: true` (`xdb-core/src/ssh.rs:56`),
  so the SSH path doesn't lag — the contrast that surfaced this bug.

## Requirements

* In `TcpTransport::connect()`, after the `TcpStream` is established, call
  `stream.set_nodelay(true)?` before storing it as the current connection.
* Failure to set the option propagates as an error from `connect()` (same `?`
  convention as the connect call — a socket that can't take TCP_NODELAY is
  degenerate and should surface, not be silently ignored).
* Add a comment explaining *why* (ADB mux = small-packet interactive; Nagle adds
  RTT lag), matching the surrounding code's comment density.

## Acceptance Criteria

* [ ] `TcpTransport::connect()` sets `TCP_NODELAY` on the underlying socket.
* [ ] A unit test connects to a loopback `TcpListener`, drives `connect()`, and
      asserts the stored stream's `nodelay()` getter returns `true`.
* [ ] `cargo test` / `cargo clippy` green; no behavior change to USB or to the
      message read/write contract.

## Definition of Done

* Unit test added in `tcp_transport.rs` (same module → can read the private
  `current_connection` to assert the option).
* `cargo clippy --all-targets` and `cargo test` pass.
* No public API change (connect signature unchanged).

## Technical Approach

```rust
async fn connect(&mut self) -> Result<()> {
    let stream = TcpStream::connect(self.address).await?;
    // ADB multiplexing over TCP is small-packet + interactive (shell echoes one
    // tiny segment per keystroke); Nagle would hold each behind the prior ACK and
    // add an RTT of lag. Disable it so interactive shells stay responsive. The
    // option lives on the kernel socket, so it also covers the later TLS upgrade
    // (which consumes this same TcpStream).
    stream.set_nodelay(true)?;
    self.current_connection = Some(Arc::new(Mutex::new(Some(CurrentConnection::Tcp(stream)))));
    Ok(())
}
```

**Test sketch** (hermetic, no real device):

```rust
#[tokio::test]
async fn connect_sets_tcp_nodelay() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { let _ = listener.accept().await; });

    let mut t = TcpTransport::new(addr, /* dummy key path */ ...);
    t.connect().await.unwrap();

    let lock = t.current_connection.as_ref().unwrap().lock().await;
    match lock.as_ref().unwrap() {
        CurrentConnection::Tcp(s) => assert!(s.nodelay().unwrap()),
        _ => panic!("expected plain Tcp connection"),
    }
}
```

`TcpTransport::new` takes a `private_key_path` but `connect()` never reads it
(only the TLS upgrade does), so the test can pass any path.

## Out of Scope

* Bug 2 (per-device `shell_v2` capability over-advertising) — tracked as a
  separate task; needs banner-feature plumbing + a brainstorm on direction.
* Any selftest interactive/tcpip wiring — the testbed (hypervisor Yocto-Linux
  adbd behind an Android `tcp:6665` forward) is special hardware; nodelay is
  covered by the hermetic unit test above, which needs no device.
* Tuning other socket options (SO_KEEPALIVE, buffer sizes).

## Technical Notes

* `tokio::net::TcpStream::set_nodelay(&self, bool) -> io::Result<()>` and
  `nodelay(&self) -> io::Result<bool>` are both stable — getter enables the test.
* `?` on `set_nodelay` returns `io::Error`, which `RustADBError` converts from via
  the existing `From<io::Error>` (same as `TcpStream::connect`'s error).
* One bug = one task = one commit (repo convention).
