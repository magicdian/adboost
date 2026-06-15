# `adboost`

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](../LICENSE) [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../NOTICE)
[![Documentation](https://docs.rs/adboost/badge.svg)](https://docs.rs/adboost)
[![Crates.io Total Downloads](https://img.shields.io/crates/d/adboost)](https://crates.io/crates/adboost)
![MSRV](https://img.shields.io/crates/msrv/adboost)

Rust library implementing ADB protocol.

## Installation

Add `adboost` crate as a dependency by simply adding it to your `Cargo.toml`:

```toml
[dependencies]
adboost = "*"
```

## Crate features

|    Feature    |                            Description                            | Default? |
| :-----------: | :---------------------------------------------------------------: | :------: |
| `framebuffer` |                Enables _framebuffer_-related methods              |   Yes    |
|    `mdns`     |          Enables mDNS device discovery on local network.          |    No    |
|     `usb`     |               Enables interactions with USB devices.              |    No    |
|   `server`    | ADB **server** frontend serving native `adb`/`scrcpy` (implies `usb`). |    No    |

To deactivate some default features you can use the `default-features = false` option in your `Cargo.toml` file and manually specify the features you want to activate:

```toml
[dependencies]
adboost = { version = "*", default-features = false, features = ["mdns", "usb"] }
```

## Examples

Usage examples can be found in the `examples/` directory of this repository.

Some example are also provided in the various `README.md` files of modules.

## Benchmarks

Benchmarks run on `v2.0.6`, on a **Samsung S10 SM-G973F** device and an **Intel i7-1265U** CPU laptop

### `ADBServerDevice` push vs `adb push`

`ADBServerDevice` performs all operations by using adb server as a bridge.

| File size | Sample size | `ADBServerDevice` |   `adb`   |               Difference               |
| :-------: | :---------: | :---------------: | :-------: | :------------------------------------: |
|   10 MB   |     100     |     350,79 ms     | 356,30 ms | <div style="color:green">-1,57 %</div> |
|  500 MB   |     50      |      15,60 s      |  15,64 s  | <div style="color:green">-0,25 %</div> |
|   1 GB    |     20      |      31,09 s      |  31,12 s  | <div style="color:green">-0,10 %</div> |
