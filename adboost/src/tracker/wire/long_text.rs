use crate::tracker::models::{
    ConnectionState, DeviceInfo, DeviceSerial, DeviceSnapshot, TransportKind,
};
use crate::tracker::wire::normalize_bus_address;

/// Decode a `host:track-devices-l` frame body into one snapshot.
///
/// A row that cannot be decoded (invalid UTF-8, invalid serial, bad
/// `transport_id`) increments `malformed_rows` and is skipped; it is never
/// inserted under a raw line.
pub(crate) fn decode(body: &[u8]) -> DeviceSnapshot {
    let mut snapshot = DeviceSnapshot::default();
    for row in body.split(|byte| *byte == b'\n') {
        let row = row.strip_suffix(b"\r").unwrap_or(row);
        if row.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match parse_line(row) {
            Ok(device) => snapshot.devices.push(device),
            Err(()) => snapshot.malformed_rows += 1,
        }
    }
    snapshot
}

/// Decode one long-text row.
///
/// The grammar is AOSP's `transport.cpp::append_transport`:
///
/// ```text
/// <serial padded to 22 with spaces> <state>[ <bus>][ product:<p>]
/// [ model:<m>][ device:<d>] transport_id:<n>
/// ```
///
/// adboost's own server frontend uses `serial\tstate ...` instead; both are
/// accepted. The state is matched as the longest known prefix because AOSP's
/// `no permissions` state is followed by a parenthetical help string.
pub(crate) fn parse_line(row: &[u8]) -> Result<DeviceInfo, ()> {
    let row = std::str::from_utf8(row).map_err(|_| ())?;
    let row = row.trim_end_matches(['\r', '\n']);
    // AOSP renders a transport without a serial as "(no serial number)"; the
    // first whitespace token would be the plausible-looking "(no", so reject
    // the placeholder explicitly rather than inserting a phantom device.
    if row.starts_with("(no serial number)") {
        return Err(());
    }
    let (serial, remainder) = split_serial(row)?;
    let serial = DeviceSerial::new(serial).map_err(|_| ())?;

    let remainder = remainder.trim_start();
    if remainder.is_empty() {
        return Err(());
    }
    let (state, consumed) = ConnectionState::parse_prefix(remainder);
    let attributes = remainder.get(consumed..).unwrap_or("").trim_start();
    let parsed = parse_attributes(attributes)?;
    let transport = TransportKind::from_text(serial.as_str(), parsed.has_bus_address);

    Ok(DeviceInfo {
        serial,
        state,
        transport,
        transport_id: parsed.transport_id,
        bus_address: parsed.bus_address,
        product: parsed.product,
        model: parsed.model,
        device: parsed.device,
        negotiated_speed: None,
        max_speed: None,
    })
}

fn split_serial(row: &str) -> Result<(&str, &str), ()> {
    let Some(index) = row.find(char::is_whitespace) else {
        return Err(());
    };
    let serial = &row[..index];
    if serial.is_empty() {
        return Err(());
    }
    Ok((serial, &row[index..]))
}

#[derive(Default)]
struct ParsedAttributes {
    bus_address: Option<String>,
    product: Option<String>,
    model: Option<String>,
    device: Option<String>,
    transport_id: Option<u32>,
    has_bus_address: bool,
}

fn parse_attributes(mut attributes: &str) -> Result<ParsedAttributes, ()> {
    let mut parsed = ParsedAttributes::default();

    // AOSP puts the (possibly bare) bus address immediately after the state.
    if let Some((token, remainder)) = split_first_token(attributes) {
        if let Some(bus) = token.strip_prefix("usb:") {
            if is_bus_address(bus) {
                parsed.bus_address = normalize_bus_address(token);
                parsed.has_bus_address = true;
                attributes = remainder.trim_start();
            }
        } else if is_bus_address(token) {
            parsed.bus_address = normalize_bus_address(token);
            parsed.has_bus_address = true;
            attributes = remainder.trim_start();
        }
    }

    let markers = attribute_markers(attributes);
    for (index, (position, key)) in markers.iter().enumerate() {
        let value_start = position + key.len();
        let value_end = markers
            .get(index + 1)
            .map_or(attributes.len(), |(next, _)| *next);
        let value = attributes[value_start..value_end].trim();

        match *key {
            "product:" => parsed.product = non_empty(value),
            "model:" => parsed.model = non_empty(value),
            "device:" => parsed.device = non_empty(value),
            "transport_id:" => {
                parsed.transport_id = Some(value.parse::<u32>().map_err(|_| ())?);
            }
            "usb:" if is_bus_address(value) => {
                parsed.bus_address = normalize_bus_address(value);
                parsed.has_bus_address = true;
            }
            _ => {}
        }
    }

    Ok(parsed)
}

const ATTRIBUTE_KEYS: [&str; 5] = ["product:", "model:", "device:", "transport_id:", "usb:"];

fn attribute_markers(value: &str) -> Vec<(usize, &'static str)> {
    let mut markers = Vec::new();
    for key in ATTRIBUTE_KEYS {
        let mut search_from = 0;
        while let Some(relative) = value[search_from..].find(key) {
            let position = search_from + relative;
            let at_boundary = position == 0 || value.as_bytes()[position - 1].is_ascii_whitespace();
            if at_boundary {
                markers.push((position, key));
            }
            search_from = position + key.len();
        }
    }
    markers.sort_unstable_by_key(|(position, _)| *position);
    markers
}

fn split_first_token(value: &str) -> Option<(&str, &str)> {
    if value.is_empty() {
        return None;
    }
    match value.find(char::is_whitespace) {
        Some(index) => Some((&value[..index], &value[index..])),
        None => Some((value, "")),
    }
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn is_bus_address(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(bus) = parts.next() else {
        return false;
    };
    let Some(port) = parts.next() else {
        return false;
    };
    !bus.is_empty()
        && bus.chars().all(|character| character.is_ascii_digit())
        && !port.is_empty()
        && port
            .split('.')
            .all(|segment| !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()))
        && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::{decode, parse_line};
    use crate::tracker::models::{ConnectionState, TransportKind};

    #[test]
    fn decodes_the_aosp_space_padded_long_line() {
        let body = b"YTGUSCNFMFAIK7ZP       device usb:4-1.2 product:vext_f57aes model:f57aes device:f57aes transport_id:164\n";
        let snapshot = decode(body);
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
        assert_eq!(device.transport_id, Some(164));
        assert_eq!(device.negotiated_speed, None);
        assert_eq!(device.max_speed, None);
    }

    #[test]
    fn decodes_the_adboost_tab_padded_long_line() {
        let body = b"ABC123\tdevice product:foo model:bar device:baz transport_id:7\n";
        let snapshot = decode(body);
        assert_eq!(snapshot.malformed_rows, 0);
        let device = &snapshot.devices[0];
        assert_eq!(device.serial.as_str(), "ABC123");
        assert_eq!(device.transport, TransportKind::Local);
        assert_eq!(device.bus_address, None);
        assert_eq!(device.product.as_deref(), Some("foo"));
        assert_eq!(device.model.as_deref(), Some("bar"));
        assert_eq!(device.device.as_deref(), Some("baz"));
        assert_eq!(device.transport_id, Some(7));
    }

    #[test]
    fn accepts_a_legacy_bare_bus_address() {
        let body = b"7a5158f05122195aa       device 1-5 product:gts210vewifixx model:SM_T813 device:gts210vewifi transport_id:4\n";
        let snapshot = decode(body);
        let device = &snapshot.devices[0];
        assert_eq!(device.bus_address.as_deref(), Some("1-5"));
        assert_eq!(device.transport, TransportKind::Usb);
    }

    #[test]
    fn infers_tcp_and_local_transports_from_the_serial() {
        let body = b"192.168.100.192:5555   device product:alioth transport_id:97\nemulator-5554          device product:sdk transport_id:101\n";
        let snapshot = decode(body);
        assert_eq!(snapshot.devices[0].transport, TransportKind::Tcp);
        assert_eq!(snapshot.devices[1].transport, TransportKind::Local);
    }

    #[test]
    fn decodes_no_permissions_with_its_help_text() {
        let body = b"ABC123                 no permissions (user in plugdev group; are your udev rules wrong?); see [http://developer.android.com/tools/device.html] transport_id:9\n";
        let snapshot = decode(body);
        assert_eq!(snapshot.malformed_rows, 0);
        let device = &snapshot.devices[0];
        assert_eq!(device.state, ConnectionState::NoPermission);
        assert_eq!(device.transport_id, Some(9));
    }

    #[test]
    fn keeps_spaces_inside_attribute_values() {
        let body = b"192.168.100.192:5555     device product:m425 plus-a model:M425 PLUS A device:m450a transport_id:1\n";
        let snapshot = decode(body);
        let device = &snapshot.devices[0];
        assert_eq!(device.product.as_deref(), Some("m425 plus-a"));
        assert_eq!(device.model.as_deref(), Some("M425 PLUS A"));
        assert_eq!(device.device.as_deref(), Some("m450a"));
    }

    #[test]
    fn unknown_state_and_attributes_degrade_without_dropping_the_device() {
        let body = b"ABC123                 weird-state product:x transport_id:3\n";
        let snapshot = decode(body);
        assert_eq!(snapshot.malformed_rows, 0);
        let device = &snapshot.devices[0];
        assert_eq!(device.state, ConnectionState::Unknown);
        assert_eq!(device.product.as_deref(), Some("x"));
        assert_eq!(device.transport_id, Some(3));
    }

    #[test]
    fn empty_body_is_an_authoritative_empty_snapshot() {
        let snapshot = decode(b"");
        assert!(snapshot.devices.is_empty());
        assert_eq!(snapshot.malformed_rows, 0);
    }

    #[test]
    fn crlf_and_trailing_newlines_are_tolerated() {
        let snapshot = decode(b"ABC123 device transport_id:1\r\n");
        assert_eq!(snapshot.devices.len(), 1);
        assert_eq!(snapshot.malformed_rows, 0);
    }

    #[test]
    fn malformed_rows_are_counted_and_skipped() {
        let body = b"GOOD device transport_id:1\n\xff device transport_id:2\nBAD device transport_id:nope\n";
        let snapshot = decode(body);
        assert_eq!(snapshot.devices.len(), 1);
        assert_eq!(snapshot.devices[0].serial.as_str(), "GOOD");
        assert_eq!(snapshot.malformed_rows, 2);
    }

    #[test]
    fn rejects_the_aosp_no_serial_placeholder() {
        let snapshot = decode(b"(no serial number)     device transport_id:3\n");
        assert!(
            snapshot.devices.is_empty(),
            "the placeholder must never become a device serial"
        );
        assert_eq!(snapshot.malformed_rows, 1);
    }

    #[test]
    fn transport_id_is_optional_for_the_tracker() {
        let device = parse_line(b"ABC123 device\n").expect("row without transport_id");
        assert_eq!(device.transport_id, None);
    }

    #[test]
    fn parse_line_accepts_a_legacy_trailing_newline() {
        let device = parse_line(b"ABC123\tdevice\n").expect("legacy row");
        assert_eq!(device.serial.as_str(), "ABC123");
    }

    #[test]
    fn tab_and_space_rendering_produce_equal_snapshots() {
        let tabbed = decode(b"ABC123\tdevice product:foo transport_id:7\n");
        let padded = decode(b"ABC123                 device product:foo transport_id:7\n");
        assert_eq!(tabbed, padded, "the two renderers must decode identically");
    }
}
