/// USB-related definitions
#[cfg(feature = "usb")]
#[cfg_attr(docsrs, doc(cfg(feature = "usb")))]
pub mod usb;

/// Device reachable over TCP related definition
pub mod tcp;

pub(crate) mod adb_message_device;
mod adb_message_device_commands;
pub(crate) mod adb_message_transport;
pub mod adb_session;
pub(crate) mod adb_transport_message;
mod commands;
pub(crate) mod framed_read;
pub(crate) mod message_commands;
mod models;
/// Shared shell-v2 inner-frame codec (used by both the USB and proxy paths).
pub mod shell_v2_codec;
/// Transport-generic shell-v2 session (writable / streaming / cancelable).
pub mod shell_v2_session;
mod utils;

pub use adb_message_device::ADBMessageDevice;
pub use shell_v2_codec::{FrameHeader, ShellChannel, decode_header, encode};
pub use shell_v2_session::{ShellFrame, ShellV2Output, ShellV2Session};
pub use utils::BinaryDecodable;
