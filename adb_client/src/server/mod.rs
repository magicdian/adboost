//! ADB **server** frontend (feature `server`).
//!
//! This module is adboost acting *as* an ADB server: it listens on TCP
//! (default `:5037`), speaks the smartsocket **host protocol** to native `adb`
//! and `scrcpy` clients, and bridges their local services (`shell:` / `tcp:`)
//! onto a [device backend][`DeviceBackend`]. It is the mirror image of
//! [`crate::proxy`], which is a *client* that talks to someone else's adb server.
//!
//! # Layers
//!
//! - [`protocol`] — pure, I/O-free smartsocket host-protocol wire encode/decode.
//! - [`DeviceBackend`] — the seam: where devices come from and how a local
//!   service is opened. adboost ships [`UsbDeviceBackend`] over the existing
//!   [`PersistentUsbConnection`][crate::usb::PersistentUsbConnection]; inject
//!   your own to add custom discovery / relay / auth.
//! - the listening frontend (assembled in a later phase) ties them together.

pub mod protocol;

mod backend;
mod capabilities;
mod frontend;
mod usb_backend;

pub use backend::{DeviceBackend, DeviceEntry, DeviceState};
pub use capabilities::{KillPolicy, ServerCapabilities};
pub use frontend::{AdbServerFrontend, AdbServerFrontendBuilder};
pub use usb_backend::UsbDeviceBackend;
