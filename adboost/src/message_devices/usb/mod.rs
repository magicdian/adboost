mod adb_usb_device;
mod bridge;
mod flow_control;
pub mod persistent;
mod reverse_engine;
mod reverse_policy;
mod shell_v2_session;
/// Deterministic in-memory ADB-device simulator for protocol/timing tests.
///
/// Double-gated `#[cfg(any(test, feature = "test-support"))]`: adboost's own
/// inline tests get it for free under `cfg(test)`, and the opt-in `test-support`
/// feature exposes it to separate test crates (CLI selftest, downstream `xdb`)
/// that cannot see `cfg(test)` symbols. See the module docs for the honest
/// boundary of what a frame/byte-level simulator can and cannot prove.
#[cfg(any(test, feature = "test-support"))]
pub mod sim;
mod sync_session;
pub(crate) mod usb_transport;
mod utils;

pub use crate::message_devices::shell_v2_codec::ShellChannel;
pub use crate::message_devices::shell_v2_session::{ShellFrame, ShellV2Output};
pub use adb_usb_device::ADBUSBDevice;
pub use bridge::bridge_tcp_session;
pub use persistent::{
    MultiplexedSession, PersistentConnection, PersistentTcpConnection, PersistentUsbConnection,
    SessionReadHalf, SessionWriteHalf, TcpConnectOptions,
};
pub use reverse_engine::ReverseEngine;
pub use reverse_policy::ReversePolicy;
pub use shell_v2_session::ShellV2Session;
pub use sync_session::SyncSession;
pub use usb_transport::USBTransport;
pub use utils::{ADBDeviceInfo, find_all_connected_adb_devices};

// Re-export the raw wire types used by the committed stable raw-message API
// (`PersistentUsbConnection::{subscribe_raw, send_raw, incoming_opens}`), so
// callers can construct/inspect raw messages without reaching into crate
// internals.
pub use crate::message_devices::adb_transport_message::{
    ADBTransportMessage, ADBTransportMessageHeader,
};
pub use crate::message_devices::message_commands::MessageCommand;
