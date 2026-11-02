use crate::tracker::models::{ConnectionState, DeviceSerial, TransportKind};

/// One device in a tracker snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The validated device serial (the authoritative identity).
    pub serial: DeviceSerial,
    /// The device's connection state.
    pub state: ConnectionState,
    /// How the device is attached.
    pub transport: TransportKind,
    /// The adb transport id, when known.
    pub transport_id: Option<u32>,
    /// USB bus address without the `usb:` scheme prefix (e.g. `4-1.2`).
    pub bus_address: Option<String>,
    /// Product name, when reported.
    pub product: Option<String>,
    /// Model name, when reported.
    pub model: Option<String>,
    /// Device codename, when reported.
    pub device: Option<String>,
    /// Negotiated USB speed in Mbps (protobuf tracker only).
    pub negotiated_speed: Option<u32>,
    /// Maximum USB speed in Mbps (protobuf tracker only).
    pub max_speed: Option<u32>,
}
