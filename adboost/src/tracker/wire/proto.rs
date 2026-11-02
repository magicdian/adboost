use quick_protobuf::{BytesReader, MessageRead};

use crate::tracker::TrackerError;
use crate::tracker::models::{
    ConnectionState, DeviceInfo, DeviceSerial, DeviceSnapshot, TransportKind,
};
use crate::tracker::wire::normalize_bus_address;

/// The host protocol's four-hex length prefix caps a frame at 0xFFFF bytes.
/// The framing layer already enforces this; the decoder keeps a defensive
/// check so a future caller cannot hand it an unbounded buffer.
const MAX_PROTO_FRAME_LEN: usize = 0xFFFF;

/// Decode one `host:track-devices-proto-binary` frame.
pub(crate) fn decode(body: &[u8]) -> Result<DeviceSnapshot, TrackerError> {
    if body.len() > MAX_PROTO_FRAME_LEN {
        return Err(TrackerError::Decode {
            row: 0,
            detail: format!(
                "protobuf tracker frame is too large ({} bytes, max {MAX_PROTO_FRAME_LEN})",
                body.len()
            ),
        });
    }

    let mut reader = BytesReader::from_bytes(body);
    let devices =
        Devices::from_reader(&mut reader, body).map_err(|error| TrackerError::Decode {
            row: 0,
            detail: format!("invalid adb.proto.Devices frame: {error}"),
        })?;

    let mut snapshot = DeviceSnapshot::default();
    for device in devices.entries {
        let Ok(serial) = DeviceSerial::new(device.serial) else {
            snapshot.malformed_rows += 1;
            continue;
        };
        let bus_address = normalize_bus_address(&device.bus_address);
        let transport = TransportKind::from_proto(
            device.connection_type,
            serial.as_str(),
            bus_address.as_deref(),
        );

        snapshot.devices.push(DeviceInfo {
            serial,
            state: ConnectionState::from_proto(device.state),
            transport,
            transport_id: u32::try_from(device.transport_id).ok(),
            bus_address,
            product: non_empty(device.product),
            model: non_empty(device.model),
            device: non_empty(device.codename),
            negotiated_speed: u32::try_from(device.negotiated_speed).ok(),
            max_speed: u32::try_from(device.max_speed).ok(),
        });
    }

    Ok(snapshot)
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

#[derive(Default)]
struct Devices {
    entries: Vec<Device>,
}

#[derive(Default)]
struct Device {
    serial: String,
    state: i32,
    bus_address: String,
    product: String,
    model: String,
    codename: String,
    connection_type: i32,
    negotiated_speed: i64,
    max_speed: i64,
    transport_id: i64,
}

impl<'a> MessageRead<'a> for Devices {
    fn from_reader(reader: &mut BytesReader, bytes: &'a [u8]) -> quick_protobuf::Result<Self> {
        let mut message = Self::default();
        while !reader.is_eof() {
            match reader.next_tag(bytes) {
                Ok(10) => message.entries.push(reader.read_message::<Device>(bytes)?),
                Ok(tag) => {
                    reader.read_unknown(bytes, tag)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(message)
    }
}

impl<'a> MessageRead<'a> for Device {
    fn from_reader(reader: &mut BytesReader, bytes: &'a [u8]) -> quick_protobuf::Result<Self> {
        let mut message = Self::default();
        while !reader.is_eof() {
            match reader.next_tag(bytes) {
                // adb.proto.Device: serial=1, state=2, bus_address=3,
                // product=4, model=5, device=6, connection_type=7,
                // negotiated_speed=8, max_speed=9, transport_id=10.
                Ok(10) => message.serial = reader.read_string(bytes)?.to_string(),
                Ok(16) => message.state = reader.read_enum(bytes)?,
                Ok(26) => message.bus_address = reader.read_string(bytes)?.to_string(),
                Ok(34) => message.product = reader.read_string(bytes)?.to_string(),
                Ok(42) => message.model = reader.read_string(bytes)?.to_string(),
                Ok(50) => message.codename = reader.read_string(bytes)?.to_string(),
                Ok(56) => message.connection_type = reader.read_enum(bytes)?,
                Ok(64) => message.negotiated_speed = reader.read_int64(bytes)?,
                Ok(72) => message.max_speed = reader.read_int64(bytes)?,
                Ok(80) => message.transport_id = reader.read_int64(bytes)?,
                Ok(tag) => {
                    reader.read_unknown(bytes, tag)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(message)
    }
}

/// Test-only encoder for `adb.proto.Devices` frames.
///
/// Keeping the encoder independent of the decoder catches field-number and
/// varint drift that a round-trip through shared code would hide.
#[cfg(test)]
pub(crate) mod test_support {
    #[derive(Clone, Copy)]
    pub(crate) struct FixtureDevice<'a> {
        pub serial: &'a str,
        pub state: i32,
        pub bus_address: &'a str,
        pub product: &'a str,
        pub model: &'a str,
        pub device: &'a str,
        pub connection_type: i32,
        pub negotiated_speed: i64,
        pub max_speed: i64,
        pub transport_id: i64,
    }

    impl<'a> FixtureDevice<'a> {
        pub(crate) fn new(serial: &'a str) -> Self {
            Self {
                serial,
                state: 8,
                bus_address: "",
                product: "",
                model: "",
                device: "",
                connection_type: 1,
                negotiated_speed: 0,
                max_speed: 0,
                transport_id: 0,
            }
        }
    }

    pub(crate) fn devices(devices: &[FixtureDevice<'_>]) -> Vec<u8> {
        let mut output = Vec::new();
        for device in devices {
            push_bytes(&mut output, 1, &device_bytes(device));
        }
        output
    }

    pub(crate) fn device_bytes(device: &FixtureDevice<'_>) -> Vec<u8> {
        let mut output = Vec::new();
        push_bytes(&mut output, 1, device.serial.as_bytes());
        push_varint_field(&mut output, 2, int64_to_varint(i64::from(device.state)));
        push_bytes(&mut output, 3, device.bus_address.as_bytes());
        push_bytes(&mut output, 4, device.product.as_bytes());
        push_bytes(&mut output, 5, device.model.as_bytes());
        push_bytes(&mut output, 6, device.device.as_bytes());
        push_varint_field(
            &mut output,
            7,
            int64_to_varint(i64::from(device.connection_type)),
        );
        push_varint_field(&mut output, 8, int64_to_varint(device.negotiated_speed));
        push_varint_field(&mut output, 9, int64_to_varint(device.max_speed));
        push_varint_field(&mut output, 10, int64_to_varint(device.transport_id));
        output
    }

    pub(crate) fn append_unknown_field(message: &mut Vec<u8>, field: u32, value: &[u8]) {
        push_bytes(message, field, value);
    }

    fn int64_to_varint(value: i64) -> u64 {
        u64::from_ne_bytes(value.to_ne_bytes())
    }

    fn push_varint_field(output: &mut Vec<u8>, field: u32, value: u64) {
        push_tag(output, field, 0);
        push_varint(output, value);
    }

    fn push_bytes(output: &mut Vec<u8>, field: u32, value: &[u8]) {
        push_tag(output, field, 2);
        push_varint(
            output,
            u64::try_from(value.len()).expect("test frame length fits u64"),
        );
        output.extend_from_slice(value);
    }

    fn push_tag(output: &mut Vec<u8>, field: u32, wire_type: u32) {
        push_varint(output, (u64::from(field) << 3) | u64::from(wire_type));
    }

    fn push_varint(output: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            let byte = u8::try_from(value & 0x7F).expect("varint payload is 7 bits");
            output.push(byte | 0x80);
            value >>= 7;
        }
        output.push(u8::try_from(value).expect("final varint payload is 7 bits"));
    }
}

#[cfg(test)]
mod tests {
    use super::decode;
    use super::test_support::{FixtureDevice, append_unknown_field, devices};
    use crate::tracker::TrackerError;
    use crate::tracker::models::{ConnectionState, TransportKind};

    #[test]
    fn decodes_a_live_capture_shaped_device() {
        let body = devices(&[FixtureDevice {
            serial: "YTGUSCNFMFAIK7ZP",
            state: 8,
            bus_address: "usb:4-1.2",
            product: "vext_f57aes",
            model: "f57aes",
            device: "f57aes",
            connection_type: 1,
            negotiated_speed: 480,
            max_speed: 5000,
            transport_id: 187,
        }]);
        let snapshot = decode(&body).expect("valid Devices message");
        assert_eq!(snapshot.malformed_rows, 0);
        assert_eq!(snapshot.devices.len(), 1);
        let device = &snapshot.devices[0];
        assert_eq!(device.serial.as_str(), "YTGUSCNFMFAIK7ZP");
        assert_eq!(device.state, ConnectionState::Device);
        assert_eq!(device.transport, TransportKind::Usb);
        assert_eq!(device.bus_address.as_deref(), Some("4-1.2"));
        assert_eq!(device.product.as_deref(), Some("vext_f57aes"));
        assert_eq!(device.model.as_deref(), Some("f57aes"));
        assert_eq!(device.device.as_deref(), Some("f57aes"));
        assert_eq!(device.negotiated_speed, Some(480));
        assert_eq!(device.max_speed, Some(5000));
        assert_eq!(device.transport_id, Some(187));
    }

    #[test]
    fn skips_unknown_fields_and_maps_unknown_enums_to_unknown() {
        let mut device = super::test_support::device_bytes(&FixtureDevice {
            serial: "ABC123",
            state: 99,
            bus_address: "",
            product: "",
            model: "",
            device: "",
            connection_type: 99,
            negotiated_speed: 0,
            max_speed: 0,
            transport_id: 5,
        });
        append_unknown_field(&mut device, 99, b"future field");
        let body = {
            let mut body = Vec::new();
            super::test_support::append_unknown_field(&mut body, 1, &device);
            body
        };
        let snapshot = decode(&body).expect("unknown fields must be skipped");
        assert_eq!(snapshot.devices.len(), 1);
        assert_eq!(snapshot.devices[0].state, ConnectionState::Unknown);
        assert_eq!(snapshot.devices[0].transport, TransportKind::Local);
        assert_eq!(snapshot.devices[0].transport_id, Some(5));
    }

    #[test]
    fn invalid_serial_increments_malformed_rows() {
        let body = devices(&[
            FixtureDevice {
                serial: "GOOD123",
                ..FixtureDevice::new("GOOD123")
            },
            FixtureDevice {
                serial: "bad serial",
                ..FixtureDevice::new("bad serial")
            },
        ]);
        let snapshot = decode(&body).expect("valid frame with one bad row");
        assert_eq!(snapshot.devices.len(), 1);
        assert_eq!(snapshot.devices[0].serial.as_str(), "GOOD123");
        assert_eq!(snapshot.malformed_rows, 1);
    }

    #[test]
    fn empty_strings_and_negative_ints_become_none() {
        let body = devices(&[FixtureDevice {
            serial: "192.168.1.5:5555",
            state: 6,
            bus_address: "",
            product: "",
            model: "",
            device: "",
            connection_type: 2,
            negotiated_speed: -1,
            max_speed: -1,
            transport_id: -1,
        }]);
        let snapshot = decode(&body).expect("valid frame");
        let device = &snapshot.devices[0];
        assert_eq!(device.bus_address, None);
        assert_eq!(device.product, None);
        assert_eq!(device.model, None);
        assert_eq!(device.device, None);
        assert_eq!(device.negotiated_speed, None);
        assert_eq!(device.max_speed, None);
        assert_eq!(device.transport_id, None);
        assert_eq!(device.transport, TransportKind::Tcp);
    }

    #[test]
    fn empty_devices_message_is_an_empty_snapshot() {
        let snapshot = decode(&[]).expect("empty protobuf message");
        assert!(snapshot.devices.is_empty());
        assert_eq!(snapshot.malformed_rows, 0);
    }

    #[test]
    fn truncated_message_is_a_decode_error() {
        let error = decode(&[0x0a, 0x05, 0x0a]).expect_err("truncated message");
        assert!(
            matches!(error, TrackerError::Decode { row: 0, .. }),
            "truncated protobuf must be a decode error: {error:?}"
        );
    }
}
