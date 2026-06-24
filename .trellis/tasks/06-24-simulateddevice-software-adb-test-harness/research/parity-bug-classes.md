# Research: parity bug classes a SimulatedDevice harness can cover

- **Query**: Which scenarios in the two recurring bug classes (1. TCP/async missing USB guarantees; 2. host-protocol parity gaps) can a `SimulatedDevice` + `SimDeviceBackend` test harness deterministically cover in adboost's own `cargo test`?
- **Scope**: internal
- **Date**: 2026-06-24

## Orientation: where each guarantee actually lives today

The two transports are NOT divergent re-implementations anymore — they were
**unified onto one shared layer**:

- `adboost/src/message_devices/framed_read.rs` — `FrameReadBuffer` (sans-io,
  cancel-safe frame assembly). Its module doc (lines 1–32) states it exists
  precisely to end the "TCP path re-implemented framing and got it wrong" bug
  class. `try_parse` (`framed_read.rs:95`) is the single place a frame is
  consumed; bytes are retained until a whole frame is present.
- `adboost/src/message_devices/usb/usb_transport.rs` — `USBTransport` holds a
  `FrameReadBuffer` in `Connection.read_buffer` (`usb_transport.rs:77`); read
  loop at `:507`, write start-gate/`WriteTimeout` scheme at `map_write_status`
  (`:360`), `Cancelled → ReadTimeout` at `map_transfer_status` (`:337`).
- `adboost/src/message_devices/tcp/tcp_transport.rs` — `TcpTransport` holds the
  same `FrameReadBuffer` inside `FrameReader` (`tcp_transport.rs:116`); read loop
  `FrameReader::read_message` (`:146`), write scheme `write_frame_atomic`
  (`:201`), `ReadTimeout` on per-read timeout (`:161`).
- The contract both must honor: `ADBMessageTransport::read_message_with_timeout`
  (`adb_message_transport.rs:35-52`) — idle deadline MUST surface
  `RustADBError::ReadTimeout`, never a transport-private timeout shape.

**Consequence for the harness:** the *残余* parity risk is no longer "USB has a
guarantee TCP lacks" at the byte layer — those bugs already have byte-layer
regression tests (`tcp_transport.rs` tests at `:538-896`, `usb_transport.rs`
tests at `:572-689`, `framed_read.rs` tests at `:147-327`). The `SimulatedDevice`
value is **above** the transport interface: it proves the *consumers* of the
contract (the `PersistentConnection` reader/writer loop, the session state
machine, the server backend) behave identically no matter which transport is
underneath — i.e. it guards the contract from the consumer side, and lets a
single state machine drive scenarios that today need a physical phone.

`SimulatedDevice` implements `ADBMessageTransport`, so it sits where USB/TCP sit;
it can *emit* `ReadTimeout`/`WriteTimeout`/`Cancelled`-equivalent variants to
drive the consumer, but it does NOT prove the kernel/`nusb`/`tokio` actually
produces them (the PRD "Honest boundaries", prd.md:59-65, is correct).

Phase legend: **A** = handshake (CNXN/AUTH/STLS/delayed_ack, no `server`);
**B** = session state machine (OPEN/OKAY/WRTE/CLSE/flow-control, no `server`);
**C** = `SimDeviceBackend` driving the smartsocket frontend (`server` +
`test-support`).

---

## Bug class 1 — TCP/async path missing USB guarantees

The reusable seam these test against: the contract at
`adb_message_transport.rs:35-52` and its consumer, the `PersistentConnection`
reader/writer loop + `DeathSignal` (`persistent.rs:317`, `is_alive` ~`:1952`,
`wait_closed`/`closed` field at `persistent.rs:556`).

| Scenario | Protocol exchange | What a sim could assert | Sim-reproducible? | Phase |
|---|---|---|---|---|
| Idle read returns `ReadTimeout`, not a fatal error | Sim's outbound queue is empty; consumer calls `read_message_with_timeout(d)` | The transport returns `RustADBError::ReadTimeout` and the reader loop keeps looping (does not tear down). This is AC in prd.md:79. | **yes** — sim emits `ReadTimeout` on empty queue by construction; directly drivable above the interface. | A |
| Reader-death unblocks `wait-for-disconnect` | adbd "restart": sim stops answering / drops its end; reader task hits fatal break → `DeathSignal` fires | `is_alive()` flips false sub-second; `closed`/`wait_closed` resolves; backend can publish `TransportReset`. Guards `wait_for_disconnect_unblocks_on_reader_death` (prd.md:55). | **yes** — sim can deterministically transition to a "dead" mode and the death plumbing is transport-generic. | A (death seam) / C (TransportReset publish) |
| Transient connect errors are ridden out, then succeed | Sim fails the first N `write_message` with a transient `UsbTransferError`, then answers CNXN | `do_connect` recovers within `CONNECT_TRANSIENT_MAX_ATTEMPTS`; `PersistentConnection::new` succeeds. Generalizes the 3 existing `ScriptedTransport` tests (`persistent.rs:2861+`) and guards `cnxn_retries_then_succeeds_on_transient`. | **yes** — this is exactly what `ScriptedTransport` already does; `SimulatedDevice` is its generalization. | A |
| Fail-fast when transients exceed in-place budget | Sim emits the transient FOREVER | `do_connect` gives up after the small in-place budget (does not burn `CNXN_MAX_ATTEMPTS`), error propagates to outer reopen layer. Mirrors `do_connect_fails_fast_when_transients_exceed_attempts` (`persistent.rs:2940+`). | **yes** — value-level error injection above the interface. | A |
| Frame reassembly across an idle timeout stays aligned | Sim delivers a frame in two halves with an intervening empty-queue window (→ `ReadTimeout`), then the rest + next frame | Reader treats the mid-frame `ReadTimeout` as keep-looping, reassembles frame intact, next frame decodes (no `ConversionError` desync) | **partial** — the *pure* form is already covered by `framed_read.rs::byte_at_a_time_delivery_stays_aligned` (`:223`) and `tcp_transport.rs::frame_split_across_read_timeout_stays_aligned` (`:660`). A sim adds the *consumer-side* assertion (reader loop survives), but cannot prove kernel/nusb chunk-boundary behavior — that stays in the byte-layer tests. | A/B |
| Write start-gate `WriteTimeout` is recoverable; mid-frame stall is fatal | Sim back-pressures the first OUT transfer (→ `WriteTimeout`) vs stalls after first byte (→ fatal) | Persistent writer keeps looping on `WriteTimeout`, tears down on the fatal variant; write half is/ isn't poisoned accordingly | **partial** — the poison/recover logic is already unit-tested per-transport against mock writers (`tcp_transport.rs:776-895` `StallingWriter`, `usb_transport.rs:578-619`). A sim could assert the *writer-loop* reaction, but the byte-level frame-atomic semantics are NOT re-testable above the message interface (the sim writes whole `ADBTransportMessage`s, not partial frames). | B (writer-loop reaction only) |
| Illegal command word / oversize `data_length` → recoverable vs fatal | Sim emits a frame whose header decodes to an unknown command / oversize length | Consumer surfaces `ConversionError` / bounded-length rejection without hanging | **no** — `FrameReadBuffer::try_parse` enforces this at the byte layer (`framed_read.rs:95-144`), already tested (`illegal_command_word_is_conversion_error`, `:314`). A sim built on `ADBTransportMessage::try_new` cannot construct an illegal header; this is intrinsically a byte-layer test. | — (byte layer) |
| STLS/TLS upgrade re-frames cleanly | `upgrade_connection` consumes the socket, drops pre-upgrade buffer, reads post-STLS CNXN | post-upgrade banner read once, no plaintext carried into encrypted stream | **no** — TLS is real-socket-only (`tcp_transport.rs:407-483`). PRD out-of-scope (prd.md:64). The sim's `upgrade_connection` default is a no-op (`adb_message_transport.rs:28`). | — (hardware/socket) |

**Net:** the cancel-safety / framing parity bugs themselves are now byte-layer
and already covered. The sim's unique, high-value contribution in class 1 is the
**consumer-side** guarantees — the `ReadTimeout` idle contract honored by the
reader loop, and the reader-death → `DeathSignal` → `wait-for-disconnect`
liveness chain — which today require a physical phone + manual `adb root` loop.

---

## Bug class 2 — host-protocol parity gaps

The smartsocket frontend (`adboost/src/server/frontend.rs`, 3366 lines) is the
resolver; `DeviceBackend` (`server/backend.rs:220`) is the injection seam. A
`SimDeviceBackend` implements the trait directly (the trait is already
transport-neutral — session methods return `MultiplexedSession`/`SyncSession`,
NOT `PersistentUsbConnection`; only `DefaultDeviceBackend::get_or_open`
(`default_backend.rs:371`) is USB-locked, per prd.md:29-34). The existing
`MockBackend` (`frontend.rs:1589`) + `round_trip`/`round_trip_select`/
`round_trip_tport` harness (`:1666`/`:1695`/`:1724`) is the precedent: these
already assert AOSP wire bytes for many services. A `SimDeviceBackend` extends
this by driving real session bridging (it can answer `open_local_service` with a
sim-backed `MultiplexedSession`), not just list/echo.

Services the frontend already resolves (so the sim asserts wire-correctness, not
discovers a gap): `host:version`/`features`/`devices`/`devices-l`
(`frontend.rs:374-377`), `host:track-devices` (`:287`,`:1066`),
`host:transport-any`/`-usb`/`-local`/`transport-id:`/`transport:`/`tport:`
(`:309-331`), `host-serial:`/`host-usb:`/`host-local:`/`host-transport-id:`
families (`:382-557`, kind-pinned `-d`/`-e` resolver `resolve_single_by_kind`
`:795`), `get-state`/`get-serialno` (`:437-448`), `host:connect`/`disconnect`
(`:350-355`,`:576-617`), forward family (`:559`,`:814-918`), reverse
(`:1231-1300`), `host:kill` (`:291`), `host:reconnect-offline` (`:335`).
`protocol.rs` owns reply framing: `okay`/`okay_data`/`fail`/`okay_twice`/
`okay_tport`/`transport_id_for` (`protocol.rs:95-180`).

| Scenario | Protocol exchange | What a sim could assert | Sim-reproducible? | Phase |
|---|---|---|---|---|
| `host-usb:`/`transport-usb` selects only USB-kind device (`adb -d`) | Two sim devices, one `TransportKind::Usb` one `Local`; client sends `host-usb:<sub>` / `host:transport-usb` | Frontend resolves to the USB one; `host-local:`/`transport-local` resolves the other; AOSP `-d`/`-e` semantics. This is the named bug class 2 example. | **yes** — `SimDeviceBackend::list_devices` tags `kind` via `DeviceEntry::with_kind` (`backend.rs:122`); `resolve_single_by_kind` (`:795`) is pure over the list. Already partially covered by MockBackend kind tests; sim makes it end-to-end with a bridge. | C |
| transport-id assignment & `tport` 8-byte LE reply | `host:tport:*` / `host:transport-id:<N>` | `transport_id_for` (`protocol.rs`) assigns 1-based sorted ids; `okay_tport` returns OKAY+8-byte LE. | **yes** — pure over the device list; `round_trip_tport` harness exists (`frontend.rs:1724`). | C |
| `host:features` honest per-device negotiation | post-`host:transport:` `host:features`, or `host-serial:<s>:features` | Frontend advertises only features the device's banner carried (`shell_v2`/`sync_v2`), gating via `device_capabilities` (`backend.rs:341`); never offers a framing the bridge can't satisfy (`capabilities`/`BackendCapabilities`, `backend.rs:193`,`:321`). | **yes** — sim sets `DeviceEntry::capabilities` from its `DeviceProfile` banner and implements `device_capabilities`; `serve_local_service` gating at `frontend.rs:1088-1145`. | C (banner driven by A) |
| `host:devices` / `devices-l` body format & state strings | `host:devices`, `host:devices-l` | `format_devices` output, `DeviceState::as_wire` (`backend.rs:173`: device/offline/unauthorized). | **yes** — `SimDeviceBackend::list_devices` returns crafted entries; `MockBackend` already does this shape. | C |
| `host:track-devices` snapshot-on-change stream | `host:track-devices` then a sim device appears/vanishes | OKAY then a full snapshot per change via `subscribe_changes` (`backend.rs:228`,`serve_track_devices` `:1066`). | **yes** — `SimRegistry` pushes snapshots; sim controls add/remove deterministically. | C |
| adbd-restart → `TransportReset` → `wait-for-disconnect` reconnect | `wait-for-any-disconnect` blocks; sim connection dies (root/unroot) then re-enumerates | `subscribe_lifecycle` emits `TransportReset` (NOT `Disconnected` — rules retained, `backend.rs:135-156`); `serve_wait_for` (`frontend.rs:636`) unblocks sub-second; forward/reverse rules survive. Guards `back_to_back_root_unroot_recovers_via_reopen` (prd.md:56). | **yes** — `SimDeviceBackend` overrides `transport_alive` (`backend.rs:278`) off the sim connection's `is_alive()`, and re-enumeration is a `SimRegistry` state change. This is the headline Phase-C unlock. | C |
| Disconnect releases forward/reverse vs restart retains them | `LifecycleEvent::Disconnected` vs `TransportReset` | `handle_disconnects` releases rules on `Disconnected`, retains on `TransportReset`. | **yes** — already covered structurally by `DisconnectBackend` tests (`frontend.rs:1755`,`:1811-1898`); sim makes the *source* (real sim death vs unplug) deterministic instead of hand-fed events. | C |
| `host:connect` / `host:disconnect` AOSP status strings | `host:connect:<addr>` / `host:disconnect:<addr>` | OKAY+framed "connected to .." / FAIL on refusal; empty addr disconnects all. | **partial** — wire framing already asserted via `MockBackend::connect/disconnect` (`frontend.rs:1638-1657`). A sim adds a real TCP-kind device joining `list_devices`, but the actual socket connect stays `DefaultDeviceBackend`/hardware. | C (status only) |
| Missing native service/prefix (the *discovery* of a new gap) | any service native adb sends that the frontend's `match` (`:269`+) doesn't handle | Frontend should FAIL cleanly (not hang/panic); a sim cannot invent the AOSP-correct response for a service nobody implemented | **partial/no** — a sim can assert "unknown service → clean FAIL", but finding *which* prefix is missing requires comparing against the real adb binary (per memory note "host-protocol-parity-gaps": verify strings against the adb binary), which is outside the sim's reach. | C (negative-path only) |

**Net:** nearly every *positive* host-protocol resolution is sim-reproducible at
Phase C, and a `SimDeviceBackend` upgrades the existing list/echo `MockBackend`
coverage into **end-to-end** coverage (real session bridging + real lifecycle
death). The one thing a sim cannot do is *discover* an unimplemented native
prefix — that gap-finding still needs diffing against the real `adb` binary.

---

## Existing frontend test coverage & gaps

- `frontend.rs` has ~90 `#[tokio::test]`/`#[test]` (`:1581-2980`), driven through
  `MockBackend` (`:1589`) — a list/echo backend whose `open_local_service`
  is `unimplemented!()` (`:1611`): **the bridge path is never exercised in unit
  tests today.** A `SimDeviceBackend` answering `open_local_service` with a
  sim-backed `MultiplexedSession` closes exactly this gap.
- `DisconnectBackend` (`:1755`) tests `handle_disconnects` by **hand-feeding**
  `LifecycleEvent`s into a channel (`:1821-1827`); it does not prove a real
  connection death *produces* the event. A `SimDeviceBackend` whose connection
  actually dies would close the gap between "handler reacts to event" and
  "event is emitted on real death".
- `protocol.rs` tests (`:206-300`) cover reply framing + `transport_id_for` well.
- No existing test drives a `host:transport*` selection **followed by** a
  local-service request end-to-end (the `round_trip_select` harness stops at the
  bare OKAY, `:1695-1718`) — Phase C with a real bridge would cover the full
  select→`shell:`/`sync:`/`tcp:` flow.

## Caveats / Not Found

- I did NOT find an existing `sim` module, `SimulatedDevice`, `SimDeviceBackend`,
  `SimRegistry`, or a `test-support` cargo feature — consistent with the PRD
  framing this as new work.
- The byte-layer parity guarantees (cancel-safe framing, frame-atomic writes,
  `Cancelled`→`ReadTimeout` mapping) are **already** regression-tested per
  transport and share `FrameReadBuffer`; the sim's contribution is consumer-side,
  not a re-test of those byte-layer guarantees.
- TLS/STLS, real USB byte boundaries, and real OS error codes/latency are
  explicitly out of the sim's reach (prd.md:59-65) and remain hardware tests.
- I read the frontend dispatch via grep + the test module; I did not exhaustively
  read all 3366 lines, so a long-tail service arm may exist that I did not cite
  by line. The service inventory above is from the dispatch `match` at
  `frontend.rs:269-360` and the host-serial/kind families at `:382-557`.
