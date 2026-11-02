//! Pure wire codecs for the device-tracker family.
//!
//! The two formats are decoded into the same [`crate::tracker::DeviceSnapshot`]
//! so callers never branch on the raw text/protobuf shape.

mod long_text;
mod proto;

use crate::tracker::TrackerError;
use crate::tracker::models::{DeviceSnapshot, TrackerFormat};

pub(crate) use long_text::parse_line;

#[cfg(test)]
pub(crate) use proto::test_support;

pub(crate) fn decode(format: TrackerFormat, body: &[u8]) -> Result<DeviceSnapshot, TrackerError> {
    match format {
        TrackerFormat::ProtoBinary => proto::decode(body),
        TrackerFormat::LongText => Ok(long_text::decode(body)),
    }
}

/// Normalize a bus address by removing the optional `usb:` scheme.
///
/// AOSP's text renderer writes `usb:4-1.2` for USB devices but older adb
/// versions write a bare `4-1.2`; the protobuf field carries `usb:4-1.2`.
/// Normalizing here is what lets the text/proto contract test compare
/// snapshots directly.
pub(crate) fn normalize_bus_address(value: &str) -> Option<String> {
    let value = value.trim();
    let value = value.strip_prefix("usb:").unwrap_or(value);
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::tracker::models::{ConnectionState, DeviceSnapshot, TrackerFormat, TransportKind};
    use crate::tracker::wire;
    use crate::tracker::wire::test_support::{FixtureDevice, devices};

    #[test]
    fn text_and_proto_decode_the_same_logical_device() {
        let text =
            b"ABC123                 device product:vext model:f57 device:f57 transport_id:42\n";
        let text_snapshot = wire::decode(TrackerFormat::LongText, text).expect("text snapshot");
        let proto_snapshot = wire::decode(
            TrackerFormat::ProtoBinary,
            &devices(&[FixtureDevice {
                serial: "ABC123",
                state: 8,
                bus_address: "",
                product: "vext",
                model: "f57",
                device: "f57",
                connection_type: 2,
                negotiated_speed: 480,
                max_speed: 5000,
                transport_id: 42,
            }]),
        )
        .expect("proto snapshot");

        let mut expected = text_snapshot.clone();
        expected.devices[0].negotiated_speed = Some(480);
        expected.devices[0].max_speed = Some(5000);
        assert_eq!(
            proto_snapshot, expected,
            "text and protobuf decoders drifted apart"
        );
        assert_eq!(text_snapshot.devices[0].state, ConnectionState::Device);
        assert_eq!(
            text_snapshot.devices[0].transport,
            TransportKind::Local,
            "a non-TCP, non-USB serial is a local transport"
        );
    }

    #[test]
    fn empty_text_and_proto_bodies_are_authoritative_empty_snapshots() {
        for format in [TrackerFormat::LongText, TrackerFormat::ProtoBinary] {
            let snapshot = wire::decode(format, b"").expect("empty snapshot");
            assert_eq!(snapshot, DeviceSnapshot::default());
        }
    }

    #[test]
    fn proto_rejects_a_serial_with_whitespace() {
        let proto_snapshot = wire::decode(
            TrackerFormat::ProtoBinary,
            &devices(&[FixtureDevice {
                serial: "bad serial",
                ..FixtureDevice::new("placeholder")
            }]),
        )
        .expect("proto");
        assert!(proto_snapshot.devices.is_empty());
        assert_eq!(proto_snapshot.malformed_rows, 1);
    }

    #[test]
    fn long_text_uses_the_first_whitespace_token_as_the_serial() {
        // AOSP's text grammar is whitespace-delimited, so a serial containing a
        // space is not representable there; the hard whitespace invariant lives
        // in `DeviceSerial` and in the protobuf decoder, where the serial is an
        // explicit field.
        let text_snapshot = wire::decode(
            TrackerFormat::LongText,
            b"bad serial device transport_id:1\n",
        )
        .expect("text");
        assert_eq!(text_snapshot.devices.len(), 1);
        assert_eq!(text_snapshot.devices[0].serial.as_str(), "bad");
        assert_eq!(text_snapshot.devices[0].state, ConnectionState::Unknown);
    }
}
