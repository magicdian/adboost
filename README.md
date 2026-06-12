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

## Workspace layout

| Crate           | Description                                                        |
| --------------- | ------------------------------------------------------------------ |
| `adb_client`    | Core Rust library implementing the ADB server & device protocols.  |
| `adboost_cli`   | CLI binary built on top of the library.                            |
| `pyadb_client`  | Python bindings exposing the library to Python.                    |
| `examples/mdns` | Example: mDNS device discovery.                                    |

## TODO

- [ ] Document features (server proxy mode, direct USB/TCP device connections, framebuffer, …)
- [ ] Installation instructions (library, CLI, Python package)
- [ ] Usage examples
- [ ] Document the new persistent-connection capabilities
- [ ] Finalize naming/branding once the planned refactor lands
