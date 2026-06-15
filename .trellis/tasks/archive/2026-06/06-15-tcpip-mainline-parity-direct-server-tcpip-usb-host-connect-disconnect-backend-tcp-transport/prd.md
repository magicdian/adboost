# tcpip mainline parity (PR1–5)

## Goal

Close the "tcpip mode" capability gap between adboost and official adb across
the **direct** (USB/TCP) and **server-frontend** modes, and complete the
self-sufficient wireless loop: `USB device → adb tcpip <port> → adb connect
<ip:port> → wireless TCP device`. This directly fixes the user-reported
`error: unknown host service: connect:127.0.0.1:8885` (adboost server has no
`host:connect`), and brings the family of device **control services**
(`tcpip:`/`usb:`/`root:`/`reboot:`/`remount:`/`*-verity:`) plus host queries
(`wait-for-*`, `reconnect-offline`) to parity.

## What I already know

- Three modes: `proxy` (via external daemon — most complete), `message_devices`
  direct (`ADBUSBDevice`/`ADBTcpDevice` impl `ADBDeviceExt`), `server`
  (`AdbServerFrontend` + `DeviceBackend`).
- `ADBLocalCommand::TcpIp(u16)` / `Usb` enums + wire encoding (`tcpip:<port>` /
  `usb:`) already exist (`adb_local_command.rs:24,91-94`); only `ADBProxyDevice`
  uses them (`proxy/device_commands/{tcpip,usb}.rs`).
- `ADBDeviceExt` (`adb_device_ext.rs`) has NO `tcpip`/`usb` method → direct
  devices cannot switch modes.
- Server `map_local_service` (`frontend.rs:696-727`) only bridges
  `shell:`/`tcp:`/`sync:`/`reverse:`; every control service falls through to
  `service not supported`.
- `dispatch_host_service` (`frontend.rs:198-300`) has no `connect:`/`disconnect:`
  arm → falls to `unknown host service` (`frontend.rs:295`) — the user's bug.
- `UsbDeviceBackend` (`usb_backend.rs`) only enumerates USB via nusb hotplug;
  no notion of dynamically-added TCP transports.
- Direct TCP transport already does TLS upgrade (STLS) — `tcp_transport.rs`,
  `ADBTcpDevice`. CNXN handshake machinery exists.
- CLI `DeviceCommands` (`opts.rs`) exposes shell/push/pull/install/reboot/root/…
  but NOT tcpip/usb/remount/verity/reconnect.
- selftest has a pre-wired tcpip channel that currently reports SKIPPED
  (`selftest/mod.rs:453-464`); parity cases drive a real `adb` against the
  in-process server (`selftest/parity.rs`).

## Scope (confirmed): PR1–5 tcpip mainline closed loop. Excludes pair/mDNS (PR6)
and keygen/sideload/bugreport/backup (PR7).

## Decisions (ADR-lite)

- **(D1) tcpip/usb API placement** → extend `ADBDeviceExt` trait with
  `tcpip(&mut self, port: u16) -> Result<String>` and `usb(&mut self) ->
  Result<()>`. Both direct (`ADBUSBDevice`/`ADBTcpDevice` via `ADBMessageDevice`)
  and proxy (`ADBProxyDevice`) implement them; the existing proxy inherent
  `tcpip`/`usb` become trait impls (keep thin inherent shims if needed for source
  compat). CLI calls generically. Rationale: unifies the API across all three
  modes; CLI/selftest don't special-case.

- **(D2) Server control-service bridge (PR3)** → one-shot request/response
  semantics: open the control service, relay the device's single textual ack
  back to the client as the OKAY payload (or FAIL on device error), then close.
  This is distinct from the bidirectional `shell:`/`tcp:` pump. Allow-list for
  PR3: `tcpip:`, `usb:`, `root:`, `reboot:`, `remount:`, `enable-verity:`,
  `disable-verity:`. `tcpip:` success tears down the USB connection (adbd
  restarts) — rely on `get_or_open`'s stale-connection replacement.

- **(D3) Backend = unified transport registry, RENAMED** → extend the default
  backend to hold BOTH a USB connection pool and a dynamically-managed TCP
  transport pool behind a single device table + merged change stream + single
  transport-id ordering space (mirrors AOSP's unified `transport_list`). Because
  `DeviceBackend` is NOT dyn-compatible (`trait_variant` + AFIT → RPITIT), a
  `CompositeBackend` would still be a concrete 2-field generic shim — i.e. the
  same thing with extra boilerplate. **Rename** the public type
  `UsbDeviceBackend` → `DefaultDeviceBackend`, keeping `UsbDeviceBackend` as a
  `#[deprecated]` alias. Internally factor a `TransportRegistry` (usb + tcp
  pools, unified `list`/`subscribe`/`transport-id`).

- **(D4) host:connect identity** → `host:connect:<ip:port>` builds an
  `ADBTcpDevice::new(addr)` (full CNXN+STLS handshake already implemented),
  registers it in the TCP pool keyed by `<ip:port>` (its serial). `disconnect`
  removes it. `devices`/`devices-l`/`track-devices`/`get-state` reflect TCP
  devices in the unified table. AOSP-compatible reply strings
  (`connected to <addr>` / `already connected to <addr>` / failure text).

- **(D5) selftest tcpip coverage** → automated phase runs a SAFE control-service
  round-trip (protocol/encoding + a non-destructive control like the existing
  reconnect path) — no real mode switch, no flake. The INTERACTIVE phase performs
  the real end-to-end: `tcpip <port>` → `host:connect` → assert shell over the
  TCP device → switch back to `usb` to restore the device to its original state.

## Requirements (evolving)

- Direct USB/TCP devices support `tcpip(port)` and `usb()`.
- CLI exposes `tcpip`/`usb` (+ remount/verity/reconnect gap-fill).
- Server bridges device control services so native `adb tcpip`/`reboot`/`root`/…
  reach the USB device.
- Server implements `host:connect:<addr>` / `host:disconnect:<addr>` with a
  backend that manages dynamically-added TCP transports; `devices`/`devices-l`/
  `track-devices`/`get-state` reflect them.
- Server host queries: `host:wait-for-*`, `host:reconnect-offline`.
- Unit tests for every protocol/encoding change; selftest runtime detection.

## Acceptance Criteria (evolving)

- [ ] `adboost_cli usb tcpip 5555` switches a USB device to TCP/IP mode.
- [ ] Native `adb -P <port> tcpip 5555` via adboost server works.
- [ ] `adb connect <ip:port>` via adboost server registers a TCP device that then
      shows in `adb devices` and accepts `adb shell`.
- [ ] User's exact repro (`forward tcp:8885 tcp:6665` then `adb connect
      127.0.0.1:8885`) succeeds.
- [ ] Unit tests added for new enums/services/host arms.
- [ ] selftest gains runtime tcpip coverage (no longer a static SKIP).

## Definition of Done

- Tests added/updated (unit + selftest).
- Lint / typecheck / build green (`cargo clippy`, `cargo test`).
- README "ADB server mode" supported-services list updated.
- Each PR independently mergeable & verifiable.

## Out of Scope (explicit)

- `adb pair` + mDNS wireless-debugging (Android 11+) — future PR6.
- `keygen` / `sideload` / `bugreport` / `backup` — future PR7.

## Implementation Plan (PRs, each independently mergeable + tested)

- **PR1 — direct tcpip/usb (no deps)**: add `tcpip`/`usb` to `ADBDeviceExt`;
  implement for `ADBMessageDevice` (direct USB+TCP) and `ADBProxyDevice` (fold in
  existing `proxy/device_commands/{tcpip,usb}.rs`). Unit tests for wire encoding
  (`tcpip:<port>`/`usb:`) + ack parsing.
- **PR2 — CLI device control verbs (deps: PR1)**: expose `tcpip`/`usb` on
  `usb`/`tcp`/`local` device command sets; gap-fill `remount`/`enable-verity`/
  `disable-verity`/`reconnect`. Handlers + arg models.
- **PR3 — server control-service bridge (no hard deps)**: `map_local_service` +
  backend gain a one-shot control path for the D2 allow-list; native
  `adb tcpip`/`reboot`/`root`/`remount`/`verity` reach the USB device. Unit tests
  with a mock backend asserting OKAY/FAIL framing; add a parity case.
- **PR4a — rename backend + TCP device registry + host:connect/disconnect +
  unified device table (deps: PR1)**: rename `UsbDeviceBackend`→
  `DefaultDeviceBackend` (deprecated alias). Add a TCP device registry to the
  default backend (connect via `ADBTcpDevice` handshake). Add
  `host:connect:<addr>`/`host:disconnect:<addr>` arms; merge USB+TCP into
  `list_devices`/`devices-l`/`track-devices`/`get-state`/transport-id. **LOW
  RISK — does NOT touch the multiplexer.** Fixes the user's `unknown host
  service: connect:` bug; `adb connect` succeeds and the device lists. A local
  service (shell) against a TCP device returns a clear "not yet supported" until
  PR4b. Unit tests for connect/disconnect arms + unified listing.
- **PR4b — generalize the persistent multiplexer over `ADBMessageTransport`
  (deps: PR4a). HIGH RISK.** `persistent.rs` (3140 lines, regression-locked) +
  `MultiplexedSession` are hard-typed to `USBTransport`; generalize so the server
  bridges `shell:`/`tcp:`/`sync:` to a `host:connect`-ed TCP device. MUST
  preserve all 3 device-verified wire regressions (delayed_ack/data_check, CNXN
  no-NUL banner, CLSE routing). Extensive unit + selftest.

  > **Scope discovery (PR4 split):** brainstorm assumed one PR4. The session
  > bridge type (`MultiplexedSession`) turned out USB-specific, so "register a TCP
  > device" (cheap, fixes the reported bug) and "bridge a client shell *through*
  > to a TCP device" (deep multiplexer refactor) are separated. User approved the
  > split: ship PR4a first.
- **PR5 — server host queries (deps: PR4a)**: `host:wait-for-*`,
  `host:reconnect-offline`. Unit tests per arm. (Only needs the unified device
  table from PR4a, not the TCP bridge.)

## Technical Notes

- Key files: `adb_device_ext.rs`, `message_devices/adb_message_device_commands.rs`,
  `proxy/device_commands/{tcpip,usb,reconnect}.rs`, `server/frontend.rs`,
  `server/backend.rs`, `server/usb_backend.rs`, `models/adb_local_command.rs`,
  `models/adb_host_command.rs`, `adboost_cli/src/models/opts.rs`,
  `adboost_cli/src/selftest/*`.
- Reuse: `UsbDeviceBackend::get_or_open` already replaces stale connections —
  relevant because `tcpip:` restarts adbd and kills the USB connection.
