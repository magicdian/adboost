# Set TCP_NODELAY on client-facing frontend sockets (SEG A nodelay miss)

## Goal

The earlier nodelay fix (`a3d14da` / commit `e90ab60`) set `TCP_NODELAY` only on
the **device-facing** socket (SEG B, `TcpTransport`). Interactive shell echo
crosses **two** independent TCP hops:

```
adb client ──SEG A──> adboost :5037 frontend ──SEG B──> device adbd
           <─echo SEG A──                     <──echo SEG B──
```

SEG A — the **client→frontend** socket accepted in `serve()` — never has nodelay
set. So every keystroke's echo is held an RTT by Nagle on SEG A, and the shell
lags regardless of whether the device is IP-direct or reached via a forwarded
port (SEG A is shared by all devices). Single commands don't expose it (one big
output block, no small-packet/ACK loop); only interactive per-keystroke echo
does. xdb cannot fix this — the client socket lives entirely inside the adboost
frontend (`accept → handle_client → bridge_tcp_session`); the backend only ever
sees `(serial, cmd)`, never this `TcpStream`.

## What I already know (verified in source)

* All existing `set_nodelay` calls are on **device/outbound** sockets:
  `tcp_transport.rs:156` (SEG B), `proxy/tcp_proxy_transport.rs:192`,
  `usb/reverse_engine.rs:323` (reverse host-dial). None covers SEG A.
* Two client-facing `accept()` sites in `frontend.rs`, both flowing to
  `bridge_tcp_session`:
  1. **Main accept loop** `frontend.rs:131` — the `:5037` client. Runs the full
     host protocol (`host:transport`, `host:features`, … — themselves
     small-packet round-trips) *before* the local-service bridge
     (`bridge_tcp_session` at `frontend.rs:810`).
  2. **Forward listener accept** `frontend.rs:1102` — a `host:forward` port
     client, bridged at `frontend.rs:1114`.
* `bridge_tcp_session` (`usb/bridge.rs:24`) just `into_split()`s the socket and
  copies both directions — it never touches socket options.

## Decision: set nodelay at each accept site, NOT inside bridge_tcp_session

`bridge_tcp_session` is the wrong layer: it also bridges the reverse host-dial
socket, which already set nodelay (`reverse_engine.rs:323`) → double-set / muddy
ownership. And the main accept socket needs nodelay for the *protocol handshake*
phase too, which happens before the bridge. Setting it right after each
`accept()` is symmetric with the SEG B fix, covers both handshake + bridge, and
keeps each socket's option set exactly once at its origin.

## Requirements

* After the main accept (`frontend.rs:131`), set `TCP_NODELAY` on the accepted
  client stream before spawning `handle_client`.
* After the forward-listener accept (`frontend.rs:1102`), set `TCP_NODELAY` on
  the accepted client stream before bridging.
* A failure to set the option must NOT drop the connection (unlike SEG B's
  `connect()` where `?` is fine): these are already-accepted live client sockets
  mid-serve; log at `debug`/`warn` and proceed (mirrors the reverse-engine
  pattern at `reverse_engine.rs:323-324`, which logs and continues). Nagle-on is
  a latency regression, not a correctness failure — better a slightly laggy shell
  than a dropped client.
* Comment explaining why (interactive echo = small-packet; Nagle adds RTT),
  matching surrounding density.

## Acceptance Criteria

* [ ] Main `:5037` accept sets TCP_NODELAY on the client socket.
* [ ] Forward-listener accept sets TCP_NODELAY on the client socket.
* [ ] Setting failure is logged and tolerated (connection continues).
* [ ] A unit/integration test connects a client to the bound frontend and
      asserts the server-side accepted socket has nodelay enabled (hermetic; the
      existing frontend tests already bind a real loopback listener + connect a
      client, so this fits the established harness).
* [ ] `cargo test --features server` + `cargo clippy --all-targets --features server` green.

## Definition of Done

* Both accept sites covered; helper to avoid duplicating the set+log if it reads
  cleaner.
* Test added in the `server` frontend test module.
* No public API change.

## Out of Scope

* SEG B / proxy / reverse sockets — already have nodelay.
* `bridge_tcp_session` internals — intentionally not the chosen layer (see
  Decision).
* Bug 1 (SEG B) and Bug 2 (per-device caps) — already landed.

## Technical Notes

* `tokio::net::TcpStream::set_nodelay(&self, bool) -> io::Result<()>` /
  `nodelay()` getter — both stable, getter enables the test.
* The main accept currently destructures `(stream, peer)`; `stream` is owned and
  moved into the spawned task — set nodelay before the move.
* Helper sketch: `fn enable_nodelay(stream: &TcpStream, peer)` that logs on Err,
  called at both sites.
