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
//! # Disconnect cleanup
//!
//! When a device's transport vanishes (USB unplug, TCP `host:disconnect`, or its
//! persistent connection's reader dying), its `forward` / `reverse` rules are
//! **released by default** — matching standard `adb`, which does not leave a
//! host-side listener bound to a device that is gone. The behavior is configured
//! via [`AdbServerFrontendBuilder::on_disconnect`][crate::server::AdbServerFrontendBuilder::on_disconnect]
//! with an [`OnDisconnect`][crate::server::OnDisconnect] policy
//! ([`ReleaseAll`][crate::server::OnDisconnect::ReleaseAll] default,
//! [`Retain`][crate::server::OnDisconnect::Retain], or
//! [`Notify`][crate::server::OnDisconnect::Notify]). Callers managing release
//! themselves use [`ForwardHandle`][crate::server::ForwardHandle] (obtained from
//! [`AdbServerFrontend::handle`][crate::server::AdbServerFrontend::handle] before
//! serving). See the backend seam
//! [`DeviceBackend::subscribe_lifecycle`][crate::server::DeviceBackend::subscribe_lifecycle].
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
mod forward_handle;
mod frontend;
mod on_disconnect;
/// In-memory [`DeviceBackend`] over the [`sim`](crate::message_devices::usb::sim)
/// harness, for driving the smartsocket frontend end-to-end in tests without
/// hardware. Double-gated like the transport sim: free for adboost's own tests
/// via `cfg(test)`, exposed to external test crates via `test-support`.
#[cfg(any(test, feature = "test-support"))]
pub mod sim_backend;

pub use backend::{
    BackendCapabilities, DeviceBackend, DeviceEntry, DeviceState, LifecycleEvent, ReversePolicy,
    TransportKind,
};
pub use capabilities::{KillPolicy, ServerCapabilities};
pub use default_backend::DefaultDeviceBackend;
#[allow(deprecated)]
pub use default_backend::UsbDeviceBackend;
pub use forward_handle::ForwardHandle;
pub use frontend::{AdbServerFrontend, AdbServerFrontendBuilder};
pub use on_disconnect::OnDisconnect;
#[cfg(any(test, feature = "test-support"))]
pub use sim_backend::{SimDeviceBackend, SimRegistry};
