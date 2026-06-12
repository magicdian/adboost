# P4 reverse — staged-degradation decision

- **Date**: 2026-06-12
- **Scope**: internal (adb_client server frontend + persistent USB connection)
- **Outcome**: reverse is **honestly staged** — recognized + explicit FAIL +
  never advertised. End-to-end host-side servicing is deferred. This matches the
  PRD's P4 acceptance: "端到端打通为目标；过重则诚实分阶段降级 FAIL（不半实现、不虚假通告）".

## What end-to-end reverse requires

`reverse:forward:tcp:<R>;tcp:<L>` semantics (AOSP):
1. Host forwards the `reverse:forward:...` *local service* to adbd; adbd opens a
   listener on **device** port `R`.
2. When something on the device connects to `R`, adbd sends a **device-initiated**
   `A_OPEN(tcp:<L>)` back to the host.
3. The host must **accept** that OPEN: pick a host local-id, reply
   `OKAY(host_local_id, device_local_id)`, register session channels in the
   reader map, connect to host `127.0.0.1:<L>`, and bridge the two byte streams.

Step 3 is the "acceptor" role — the mirror of `open_session`'s "opener" role.

## Why it is not safely shippable in this task

1. **No acceptor API.** `PersistentUsbConnection` surfaces device OPENs only as
   raw frames via `incoming_opens()` (returns `ADBTransportMessage`). There is no
   method to turn a device OPEN into a `MultiplexedSession` (reply OKAY, register
   `SessionChannels` via `ReaderControl::Register`, seed acceptor-role flow
   control). `open_session` (`persistent.rs:929`) only implements the opener role
   and seeds `send_flow` from the *device's* first OKAY — which never arrives in
   the acceptor case (the device's OPEN is the first frame).
2. **`incoming_opens(&mut self)` is single-consumer `&mut`** (`persistent.rs:469`),
   while `UsbDeviceBackend` shares each connection as `Arc<PersistentUsbConnection>`
   across all clients (`usb_backend.rs`). The receiver can be taken once, before
   the Arc — there is no `&self` path today.
3. **Flow-control risk without a device.** The acceptor role needs windowed
   `OKAY` / `delayed_ack` handling as *receiver*. `adb-wire-protocol-contract.md`
   documents that this exact coupling caused **three** downstream regressions. No
   USB device is available in this environment to validate the device-initiated
   path, so shipping unvalidated acceptor flow-control code is high-risk.
4. **Half = harmful.** Forwarding only the `reverse:forward:` command (without the
   host-side accept+connect) makes adbd set up its listener, but every device
   connection then hits a host that drops the OPEN into the unconsumed
   `pending_opens` queue → the tunnel is silently dead while the client believes
   reverse succeeded. An explicit FAIL is strictly better.

## What was shipped for P4

- `reverse:` / `reverse:forward:` / `reverse:killforward*` post-transport local
  services get a **dedicated, explicit FAIL**: `"reverse not supported by this
  server"` (distinct from the generic "service not supported"), recognized in
  `frontend::map_local_service`.
- **No reverse capability is ever advertised** (`BackendCapabilities` has no
  reverse flag; `host:features` negotiation cannot add one). Honest banner intact.
- Unit test asserts the explicit reverse FAIL.

## To complete end-to-end later (extension points)

1. Add `PersistentUsbConnection::accept_device_open(open_msg) -> Result<MultiplexedSession>`
   (acceptor role: pick local-id, `ReaderControl::Register`, reply OKAY, seed
   acceptor flow control). Add a `&self` way to consume device OPENs (e.g. a
   broadcast/registration via the control channel) so an `Arc`-shared backend can
   pump them.
2. Add `DeviceBackend::open_reverse` + a `BackendCapabilities::reverse` flag
   (default false); `UsbDeviceBackend` overrides + sets the flag.
3. Frontend: on `reverse:forward:`, forward the command to the device, then run a
   per-connection pump that accepts device OPENs and bridges them to host
   `127.0.0.1:<local>`. Maintain a reverse-rule registry mirroring `ForwardRegistry`.
4. Validate against a real device (the device-initiated OPEN path cannot be
   exercised by the hardware-free MockBackend).
</content>
