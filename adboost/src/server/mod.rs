//! ADB **server** frontend (feature `server`).
//!
//! This module is adboost acting *as* an ADB server: it listens on TCP
//! (default `:5037`), speaks the smartsocket **host protocol** to native `adb`
//! and `scrcpy` clients, and bridges their local services onto a
//! [device backend][crate::server::DeviceBackend]. It is the mirror image of
//! [`crate::proxy`], which is a *client* that talks to someone else's adb server.
//!
//! # Supported services
//!
//! | Service | Layer | Status |
//! |---|---|---|
//! | `shell:` (v1) | local (backend) | ✅ bridged |
//! | `tcp:<port>` | local (backend) | ✅ bridged |
//! | `sync:` (push/pull) | local (backend) | ✅ bridged verbatim, advertised as `sync_v2` only when the backend implements it |
//! | `shell,v2` | local (backend) | ✅ bridged verbatim, advertised as `shell_v2` only when the backend implements it |
//! | `host:forward` / `killforward` / `killforward-all` / `list-forward` | host (frontend) | ✅ host-side listener + per-conn bridge, AOSP double-OKAY framing, `tcp:0` auto-assign |
//! | `reverse:forward` / `killforward` / `killforward-all` / `list-forward` | host (frontend) | ✅ device-initiated-OPEN acceptor + per-conn host-dial bridge; rule registry mirrors forward; `(reverse)` marker comes from the device. No new `host:features` flag (adb reverse has none); gated on `BackendCapabilities::reverse`. iperf3 reverse device-verified (sender+receiver both >0). |
//!
//! Optional features (`sync_v2` / `shell_v2`) are advertised in `host:features`
//! **only** when the injected backend reports it implements them
//! ([`BackendCapabilities`][crate::server::BackendCapabilities]) — the honest
//! banner: the server never offers a richer wire framing it cannot satisfy.
//!
//! # Layers
//!
//! - [`crate::server::protocol`] — pure, I/O-free smartsocket host-protocol wire encode/decode.
//! - [`crate::server::DeviceBackend`] — the seam: where devices come from and how
//!   a local service is opened. adboost ships [`crate::server::DefaultDeviceBackend`]
//!   over the existing [`PersistentUsbConnection`][crate::usb::PersistentUsbConnection];
//!   inject your own to add custom discovery / relay / auth.
//! - the listening [`crate::server::AdbServerFrontend`] ties them together.

pub mod protocol;

mod backend;
mod capabilities;
mod default_backend;
mod forward;
mod frontend;

pub use backend::{BackendCapabilities, DeviceBackend, DeviceEntry, DeviceState, ReversePolicy};
pub use capabilities::{KillPolicy, ServerCapabilities};
pub use default_backend::DefaultDeviceBackend;
#[allow(deprecated)]
pub use default_backend::UsbDeviceBackend;
pub use frontend::{AdbServerFrontend, AdbServerFrontendBuilder};
