#![doc = include_str!("./README.md")]

mod adb_tcp_device;
pub(crate) mod tcp_transport;

pub use adb_tcp_device::ADBTcpDevice;
