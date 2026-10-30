# P4 reverse — end-to-end: device-initiated OPEN acceptor + frontend reverse + iperf3 validation

> Subtask of `06-12-adboost-server-capability-expansion-...`. Replaces the staged
> reverse FAIL with a real end-to-end implementation.

## Goal

Implement `reverse:forward:` / `reverse:killforward` / `reverse:killforward-all`
/ `reverse:list-forward` in the adboost server frontend so external `adb`/`scrcpy`
clients can set up reverse tunnels through adboost's `:5037` server. Validate the
data plane with iperf3 over a real device (both connected devices have
`/system/bin/iperf3` 3.6+).

## Background (from research)

- `research/aosp-reverse-protocol.md` — exact AOSP wire protocol.
- `research/adboost-acceptor-inventory.md` — exact reuse map for the new
  `accept_device_open` path.

Key facts:
- Client sends `reverse:forward:[norebind:]<remote>;<local>` as a **device-bound
  local service** AFTER selecting a transport. Order is REMOTE(device-listen)`;`
  LOCAL(host-connect) — opposite of forward.
- The **device** binds the listener; the **host binds nothing**. The host's only
  job is to service inbound `A_OPEN(arg0=device_id, arg1=window|0, payload="<local>\0")`.
- Accept = `A_OKAY(arg0=our_id, arg1=device_id, payload=[4B LE 32MiB if delayed_ack])`,
  then dial host `<local>` and bridge. Reject = `A_CLSE(0, device_id)`.
- Acceptor seeds its send-window from the OPEN's `arg1`; grants its own 32 MiB
  receive window in the reply OKAY payload.
- `incoming_opens()` can be taken once at connection creation (before the `Arc`
  wrap), so the pump task owns the `Receiver` — no `&self`-consume problem.

## Requirements

### Library (adb_client)
1. **`PersistentUsbConnection::accept_device_open(&self, open_msg) -> Result<MultiplexedSession>`**
   — acceptor path mirroring `open_session` minus OPEN send/await (per inventory §7):
   register session, reply `OKAY(our_id, device_id, window-grant)`, seed
   `send_flow` (windowed→seed from `open_msg.arg1`; classic→none), build session.
2. **Reverse rule registry + pump** at the backend layer:
   - `UsbDeviceBackend` takes `incoming_opens()` at connection creation and spawns
     a per-connection **reverse pump** task owning the receiver.
   - The pump, for each device OPEN: parse the target (`payload`), check it against
     the connection's reverse allow-list; if allowed → `accept_device_open` and
     hand `(target, MultiplexedSession)` to the frontend via a channel; if not →
     reject with `A_CLSE(0, device_id)` (send_raw).
3. **`DeviceBackend` trait extension** (backward-compatible defaults):
   - `BackendCapabilities::reverse` flag (default false; `UsbDeviceBackend` → true).
   - `open_reverse(serial, remote, local)` — open the `reverse:forward:remote;local`
     device service, read the device reply, register the allow-list rule. Default
     unsupported.
   - `reverse_remove(serial, remote)` / `reverse_remove_all(serial)` /
     `list_reverse(serial)` — manage rules + tunnel the kill/list to the device.
   - `reverse_connections(serial) -> Receiver<ReverseConn>` where
     `ReverseConn { target: String, session: MultiplexedSession }` — the stream of
     accepted reverse opens for the frontend to dial+bridge. Default empty.
4. **Honest banner**: advertise nothing new in `host:features` for reverse (adb
   reverse has no feature flag); gate frontend acceptance on
   `BackendCapabilities::reverse`.
4a. **Configurable reverse security policy** (library does NOT hardcode policy —
   the caller chooses). `ReversePolicy` enum on the frontend builder:
   - `RejectUnconfigured` *(recommended default)* — accept only inbound OPENs
     whose target matches a configured reverse rule; others → `A_CLSE`.
   - `AllowAll` — accept any device-initiated OPEN (relay/advanced use; documented
     as unsafe).
   - `Custom(Arc<dyn Fn(&str) -> bool + Send + Sync>)` — caller decides per-target.
   The bundled CLI/daemon uses `RejectUnconfigured`. Never panic on reject (AOSP
   `LOG(FATAL)`s; we must only close the stream).

### Frontend (server)
5. Replace the staged `reverse:` FAIL: on a post-transport `reverse:forward:...`,
   call `backend.open_reverse`, relay the device's OKAY (one connect-OKAY + the
   device's status) to the client. `reverse:killforward*` / `reverse:list-forward`
   route to the backend and relay the framed reply (`(reverse)` serial marker is
   produced by the device).
6. Run a frontend **reverse bridge task** per device draining
   `reverse_connections`: for each `ReverseConn`, dial host `127.0.0.1:<target port>`
   and `bridge_session` it to the accepted device session. Dial failure → drop the
   session (device sees CLSE).
7. A reverse-rule registry mirroring `ForwardRegistry` (keyed by device-remote
   port) for `list`/`kill` bookkeeping where the host needs it.

### CLI / self-test
8. selftest: add reverse data-plane cases (through_server channel), replacing the
   reverse SKIP:
   - **`reverse_echo`** (always): host binds a tiny TCP echo server on an ephemeral
     port, set reverse `tcp:<P>;tcp:<hostP>`, then on the device
     `echo <marker> | nc 127.0.0.1 <P>` (or toybox equivalent) and assert the
     marker echoes back. Basic connectivity.
   - **`reverse_iperf3`** (auto, only if `/system/bin/iperf3` or `which iperf3`
     present on the device): host `iperf3 -s` on an ephemeral port, device
     `iperf3 -c 127.0.0.1 -p <P>` through the reverse tunnel, assert non-zero
     throughput (also surfaces a USB-link bandwidth number). SKIPPED when iperf3
     is absent on the device.
9. Update the server capability matrix doc (reverse → ✅).

## Acceptance Criteria

- [ ] `accept_device_open` unit-tested (pure parts: arg ordering, window seed) via
      the existing `new_for_test`-style harness where possible.
- [ ] `DeviceBackend` reverse methods have defaults; existing MockBackend compiles
      unchanged.
- [ ] frontend `reverse:forward:` no longer FAILs; routes through the backend.
- [ ] reverse rule add/remove/list bookkeeping unit-tested (pure registry).
- [ ] **End-to-end**: `adb reverse` through adboost server + device iperf3 client →
      host iperf3 server reports non-zero throughput (manual/selftest on real device).
- [ ] selftest `reverse` case passes on a real device; capability matrix updated.
- [ ] clippy pedantic clean (default / server / usb); all unit + doctests green.

## Out of Scope

- `localabstract:` / `localfilesystem:` reverse targets (tcp: only, like forward).
- Reverse over tcpip transport (USB only this round; mirrors forward).

## Decisions (confirmed)

1. **Security policy is library-configurable** via `ReversePolicy`
   (`RejectUnconfigured` default / `AllowAll` / `Custom`). The CLI uses
   `RejectUnconfigured`. Reject = close the stream, never panic.
2. **Pump lifecycle**: lazy — started on first `open_reverse` for a serial.
3. **selftest reverse**: `reverse_echo` always (basic connectivity) + `reverse_iperf3`
   auto-continued when the device has iperf3 (else that case SKIPPED).

## Technical Notes

- Reuse: `accept_device_open` mirrors `open_session` (inventory §1/§7);
  `bridge_session` (frontend) for the byte pump; `ForwardRegistry` shape for the
  reverse registry; `protocol::okay`/`okay_data`/`fail` for replies.
- Arg-order caveat: follow `open_session`'s CODE order
  (`OKAY arg0=our_id, arg1=device_id`), NOT the `incoming_opens` doc comment.
- `reverse:forward:` arg order is `remote;local` (device;host) — opposite forward.
</content>
