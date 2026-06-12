# `adboost_cli`

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](../LICENSE) [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../NOTICE)

Rust binary providing an improved version of `adb` CLI, built on the async
`adb_client` library (adboost fork). Formerly `adb_cli`.

## Rust binary

This crate provides a lightweight binary based on the local `adb_client` crate.
It is fully `async` (`#[tokio::main]`) and links the local workspace library.

Usage is quite simple, and tends to look like `adb`:

- To use ADB server as a proxy:

```bash
user@laptop ~/adboost (main)> adboost_cli local --help
Device related commands using server

Usage: adboost_cli local [OPTIONS] <COMMAND>

Commands:
  shell          Spawn an interactive shell or run a list of commands on the device
  pull           Pull a file from device
  push           Push a file on device
  stat           Stat a file on device
  run            Run an activity on device specified by the intent
  reboot         Reboot the device
  install        Install an APK on device
  framebuffer    Dump framebuffer of device
  host-features  List available server features
  list           List a directory on device
  logcat         Get logs of device
  help           Print this message or the help of the given subcommand(s)

Options:
  -a, --address <ADDRESS>  [default: 127.0.0.1:5037]
  -s, --serial <SERIAL>    Serial id of a specific device. Every request will be sent to this device
  -h, --help               Print help
```

- To interact directly with end devices over USB:

```bash
user@laptop ~/adboost (main)> adboost_cli usb --help
USB device related commands

Usage: adboost_cli usb [OPTIONS] [COMMAND]

Options:
  -v, --vendor-id <VID>                    Hexadecimal vendor id of this USB device
  -p, --product-id <PID>                   Hexadecimal product id of this USB device
  -k, --private-key <PATH_TO_PRIVATE_KEY>  Path to a custom private key to use for authentication
  -l, --list                               List all connected Android devices
  -h, --help                               Print help
```

## Persistent-USB exerciser (`persistent`)

The `persistent` subcommand is a one-command, in-tree reproducer + reference for
the async USB / windowed `delayed_ack` path (where the recent protocol bugs
#1/#2/#3 lived). It formalizes the throwaway `/tmp` diagnostic harness that found
bug #3 into a permanent tool. It:

1. resolves the device (explicit `--vendor-id`/`--product-id`, else autodetects
   the first connected ADB USB device);
2. builds a `PersistentUsbConnection` with the chosen feature set (default =
   windowed/`delayed_ack`; `--no-delayed-ack` = classic stop-and-wait);
3. prints a **negotiation self-check**: advertised `DeviceFeatureSet`, whether
   `delayed_ack` negotiated, the banner sent to the device, and the first inbound
   frame after `OPEN` (so `OKAY` vs `CLSE` is immediately visible);
4. runs `shell <cmd>` (defaults to `getprop`) and prints stdout + exit code.

### Prerequisites (real device)

The persistent path opens the USB device **directly** (no adb server). Make sure
nothing else holds the USB interface:

```bash
adb kill-server     # stop any running Google adb server
xdb kill-server     # stop any running xdb server, if present
# connect the device over USB and authorize the host
```

### Run it

```bash
# autodetect device, default windowed (delayed_ack) mode, run `getprop`
adboost_cli persistent shell getprop

# explicit vid/pid (e.g. MediaTek 0e8d:201c)
adboost_cli persistent --vendor-id 0e8d --product-id 201c shell getprop

# the bug-#3 control experiment: classic stop-and-wait (no windowed flow control)
adboost_cli persistent --no-delayed-ack shell getprop
```

`--no-delayed-ack` reproduces the classic path; the default reproduces the
windowed path (now working after the bug-#3 fix).

## Logging / `RUST_LOG`

The CLI installs a `tracing` subscriber (the library stays a pure emitter).
`RUST_LOG` works out of the box and supports the full `EnvFilter` directive
syntax, including per-span and per-`local_id` filtering that plain `log` cannot
do:

```bash
RUST_LOG=adb_client=debug adboost_cli persistent shell getprop          # whole crate at debug
RUST_LOG=adb_client::message_devices::usb::persistent=trace adboost_cli ...   # just the USB multiplexer
RUST_LOG=[reader]=trace adboost_cli ...                                  # just the reader task
RUST_LOG=[writer]=trace adboost_cli ...                                  # just the writer task
RUST_LOG='[session{local_id=42}]=trace' adboost_cli ...                  # only one session
RUST_LOG=adb_client=info,[session]=debug adboost_cli ...                 # combine
```

If `RUST_LOG` is unset, the `--debug` flag selects `debug` (vs the default
`info`).

> Note: the persistent exerciser needs a real device and is therefore a manual
> smoke test — it is not part of automated CI.
