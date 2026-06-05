mod adb_usb_device;
mod flow_control;
pub mod persistent;
mod shell_v2_session;
mod sync_session;
pub(crate) mod usb_transport;
mod utils;

pub use adb_usb_device::ADBUSBDevice;
pub use persistent::{
    MultiplexedSession, PersistentUsbConnection, SessionReadHalf, SessionWriteHalf,
};
pub use shell_v2_session::{ShellChannel, ShellV2Output, ShellV2Session};
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
