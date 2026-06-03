# nusb API mapping (rusb → nusb 0.2.3)

> Confirmed against docs.rs/nusb/0.2.3 during brainstorm. nusb is pure-Rust
> (no libusb / C toolchain). The project uses ONLY bulk transfers, no hotplug,
> no control/interrupt, fully synchronous — which lands in nusb's best-supported
> zone.

## Crate / Cargo.toml

- Remove: `rusb = { version = "0.9.4", features = ["vendored"], optional = true }`
- Add: `nusb = { version = "0.2", optional = true }`
- Keep feature gate `usb = ["dep:nusb"]` (was `dep:rusb`).
- No async runtime feature needed: `transfer_blocking` / `wait()` block natively;
  transfers are natively async at the OS level and need NO runtime.

## API mapping table

| rusb (current) | nusb 0.2.3 | Notes |
|---|---|---|
| `Context::new()?` | (none) | nusb has no explicit context object |
| `context.devices()?.iter()` | `nusb::list_devices().wait()?` | returns iterator of `DeviceInfo` |
| `device.device_descriptor()` | `DeviceInfo::vendor_id()` / `product_id()` etc. | fields available pre-open |
| `descriptor.vendor_id()` / `product_id()` | `DeviceInfo::vendor_id()` / `product_id()` | |
| `device.open()?` | `device_info.open().wait()?` → `Device` | |
| `handle.claim_interface(n)?` | `device.claim_interface(n).wait()?` → `Interface` | |
| `handle.release_interface(n)` | drop `Interface` | auto-released on drop |
| `device.config_descriptor(n)` / interfaces/endpoints | `Device::configurations()` / `descriptors` module | for find_endpoints / is_adb_device |
| `endpoint_desc.transfer_type()` == Bulk | descriptor `transfer_type()` in `descriptors` module | |
| `endpoint_desc.address()` / `max_packet_size()` | descriptor fields / `Endpoint::max_packet_size()` | |
| `handle.write_bulk(addr, buf, timeout)?` | `interface.endpoint::<Bulk, Out>(addr)?` then `endpoint.transfer_blocking(buf.into(), timeout)` | `Completion` carries status; timeout → `TransferError::Cancelled` |
| `handle.read_bulk(addr, buf, timeout)?` | `interface.endpoint::<Bulk, In>(addr)?` then `transfer_blocking(Buffer::new(len), timeout)` | IN `requested_len` MUST be multiple of max_packet_size; short packet ends early |
| `handle.read_manufacturer_string_ascii(&des)` | `Device::get_string_descriptor(idx, lang, timeout)` (manual decode) | no ascii convenience helper |
| `handle.read_product_string_ascii(&des)` | same | |
| `rusb::Error::Busy` | `nusb::Error` / `ErrorKind` (Busy equivalent) | maps to `RustADBError::DeviceBusy` |
| `rusb::Error` (`#[from]`) | `nusb::Error` + `nusb::transfer::TransferError` | error.rs needs both |

## Timeout handling — CRITICAL

- `Endpoint::transfer_blocking(buf, timeout) -> Completion`: blocks up to `timeout`.
  On timeout the `Completion.status` is `TransferError::Cancelled` (NOT a "timed out"
  string).
- `persistent.rs:215` currently does `err_str.contains("timed out") || contains("Timeout")`
  to distinguish "normal timeout → keep looping" from "disconnect → break".
  This MUST become a structured match on the nusb error type (e.g. a dedicated
  `RustADBError::UsbTimeout` variant produced from `TransferError::Cancelled`),
  otherwise the reader loop misclassifies normal timeouts as disconnects.

## Endpoint ownership

- `Endpoint<EpType, Dir>` is `&mut self` exclusive, NOT `Clone`.
- IN and OUT are two independent `Endpoint`s — natural fit for reader-thread
  (IN) vs writer (`Arc<Mutex>` OUT) split.
- `USBTransport` loses `#[derive(Clone)]`. Only `persistent.rs:78`
  `transport.clone()` depends on it → refactor to explicit read/write endpoint
  hand-off. `adb_usb_device.rs` does NOT depend on Clone.

## IN transfer length alignment

- nusb IN `requested_len` must be a nonzero multiple of `max_packet_size`.
- Current code reads exactly 24-byte header then exact payload via a fill loop.
  When porting, request `max_packet_size`-aligned buffers and keep the
  accumulate-until-full loop (short packet may return fewer bytes).

## Platform

- Windows = WinUSB (target ADB device confirmed as WinUSB device), Linux = usbfs
  (udev perms, same as libusb), macOS = IOKit. No vendored C build.
