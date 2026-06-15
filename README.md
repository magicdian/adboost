# adboost

> **A courteous fork of [`adb_client`](https://github.com/cocool97/adb_client).**
> Android Debug Bridge (ADB) client implementation in pure Rust.

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

---

> ⚠️ **Work in progress.** This README is an interim placeholder. Detailed
> features, installation, and usage documentation are coming soon — see the
> [TODO](#todo) below.

## About this fork

**adboost** is forked from [`adb_client`](https://github.com/cocool97/adb_client)
at tag **v3.2.2**. It builds on that excellent foundation while evolving in a few
directions of its own:

- **USB backend migrated from `rusb` to [`nusb`](https://github.com/kevinmehall/nusb)**
  — a pure-Rust, async-friendly USB stack with no libusb C dependency.
- **USB-backed ADB *server* mode** (feature `server`): adboost can act *as* an
  adb server on `:5037`, speaking the smartsocket host protocol to native
  `adb` / `scrcpy` clients and bridging to USB devices directly — no Google adb
  server required. See [ADB server mode](#adb-server-mode).
- Additional capabilities under active development (e.g. persistent-connection
  server features).

A larger refactor is planned, after which unneeded surface area will be trimmed
down.

## 🙏 Acknowledgements

This project would not exist without the original
[**adb_client**](https://github.com/cocool97/adb_client) by **Corentin LIAUD**
and its contributors. We are sincerely grateful for the high-quality, pure-Rust
ADB implementation that adboost is built upon. Please consider starring and
supporting the upstream project.

The original publications describing the ADB protocol internals remain excellent
reading:

- [Diving into ADB protocol internals (1/2)](https://www.synacktiv.com/publications/diving-into-adb-protocol-internals-12)
- [Diving into ADB protocol internals (2/2)](https://www.synacktiv.com/publications/diving-into-adb-protocol-internals-22)

## License

adboost is distributed under the terms of **`Apache-2.0 AND MIT`**:

- The **original `adb_client` code** remains under its **MIT** license,
  © 2023-2024 Corentin LIAUD. The full MIT text is preserved verbatim in
  [`NOTICE`](./NOTICE), as required by that license.
- **Modifications and new contributions** made in adboost are licensed under the
  **Apache License, Version 2.0** — see [`LICENSE`](./LICENSE).

See [`NOTICE`](./NOTICE) for the complete attribution and the upstream MIT text.

## ADB server mode

adboost can run *as* an ADB server (feature `server`), so native `adb` and
`scrcpy` clients connect to **adboost** on `:5037` instead of Google's adb
server — which lets adboost own the USB device directly.

```bash
# start adboost as the adb server (background daemon)
adboost_cli server start --address 127.0.0.1:5037
#   --foreground   run in the foreground instead of daemonizing
#   --pid-file P   PID file location (default: per-user runtime/home dir)
#   --log-file P   daemon log location (default: next to the PID file)

# now point any standard adb client at it
ADB_SERVER_SOCKET=tcp:127.0.0.1:5037 adb devices
ADB_SERVER_SOCKET=tcp:127.0.0.1:5037 adb -s <serial> shell

# stop it
adboost_cli server kill
```

> `server start` / `server kill` manage **adboost's own** server. They are
> distinct from `host kill`, which tells an *external* adb daemon to quit.

**Library API.** A zero-config USB server is a few lines; inject a custom
[`DeviceBackend`] to weave in bespoke discovery / relay / auth without touching
any protocol code:

```rust,ignore
// requires the `server` feature
use std::sync::Arc;
use adb_client::server::{AdbServerFrontend, DefaultDeviceBackend};

# async fn run() -> std::io::Result<()> {
let backend = Arc::new(DefaultDeviceBackend::new());
AdbServerFrontend::builder(backend)
    .addr("127.0.0.1:5037".parse().unwrap())
    .serve()
    .await
# }
```

Supported today: `host:version`/`features`/`devices`/`devices-l`/`track-devices`,
transport selection (`transport`/`transport-any`/`transport-id`/`tport`),
`host-serial:*` queries, `host:connect`/`disconnect` (TCP/IP devices join the
device list), `host:wait-for-*-device` and `host:reconnect-offline`, the port
`forward`/`reverse` families, and the `shell:` (v1) / `tcp:` / `sync:` /
`shell,v2` local services plus the device **control** services
(`tcpip:`/`usb:`/`root:`/`reboot:`/`remount:`/`*-verity:`). Local services are
bridged through to **both** USB and `host:connect`ed TCP/IP devices — the
persistent multiplexer is transport-generic, so `adb -s <ip>:<port> shell`/
`push`/`pull` against a wireless device works the same as against a USB one.

[`DeviceBackend`]: https://docs.rs/adb_client

## Workspace layout

| Crate           | Description                                                        |
| --------------- | ------------------------------------------------------------------ |
| `adb_client`    | Core Rust library: ADB client (`proxy` to an external daemon, direct `usb`/`tcp`) + ADB `server` frontend. |
| `adboost_cli`   | CLI binary built on top of the library.                            |
| `pyadb_client`  | Python bindings exposing the library to Python.                    |
| `examples/mdns` | Example: mDNS device discovery.                                    |

### Library module roles

| Module | Role |
| ------ | ---- |
| `adb_client::proxy` | **Client** that proxies commands through an *external* adb server daemon (`ADBProxyServer` / `ADBProxyDevice`). Formerly `server` / `server_device`. |
| `adb_client::usb` / `tcp` | Clients that connect **directly** to a device. |
| `adb_client::server` | adboost acting **as** an ADB server (feature `server`). |

## TODO

- [ ] Installation instructions (library, CLI, Python package)
- [ ] Document direct USB/TCP device connections + framebuffer
- [ ] Document the persistent-connection capabilities
- [ ] ADB server: port `forward` family + `shell_v2`/`sync`
- [ ] Migrate `pyadb_client` / `examples` to the renamed `proxy` API
- [ ] Finalize naming/branding once the planned refactor lands
