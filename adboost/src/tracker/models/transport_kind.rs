use std::fmt;

/// How a device is attached to the adb server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// A USB transport.
    Usb,
    /// A local (emulator or other local-socket) transport.
    Local,
    /// A TCP transport (`host:port` serial).
    Tcp,
}

impl TransportKind {
    pub(crate) fn from_proto(
        connection_type: i32,
        serial: &str,
        bus_address: Option<&str>,
    ) -> Self {
        match connection_type {
            1 => Self::Usb,
            2 if looks_like_tcp_serial(serial) => Self::Tcp,
            2 => Self::Local,
            _ if bus_address.is_some_and(|bus| !bus.is_empty()) => Self::Usb,
            _ if looks_like_tcp_serial(serial) => Self::Tcp,
            _ => Self::Local,
        }
    }

    pub(crate) fn from_text(serial: &str, has_bus_address: bool) -> Self {
        if has_bus_address {
            Self::Usb
        } else if looks_like_tcp_serial(serial) {
            Self::Tcp
        } else {
            Self::Local
        }
    }
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usb => f.write_str("usb"),
            Self::Local => f.write_str("local"),
            Self::Tcp => f.write_str("tcp"),
        }
    }
}

pub(crate) fn looks_like_tcp_serial(serial: &str) -> bool {
    serial
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::{TransportKind, looks_like_tcp_serial};

    #[test]
    fn recognizes_tcp_shaped_serials() {
        for serial in ["192.168.1.5:5555", "device-host:1234", "[::1]:5555"] {
            assert!(
                looks_like_tcp_serial(serial),
                "{serial:?} should be TCP-shaped"
            );
        }
        for serial in ["YTGUSCNFMFAIK7ZP", "emulator-5554", "usb:4-1.2", "host:"] {
            assert!(
                !looks_like_tcp_serial(serial),
                "{serial:?} should not be TCP-shaped"
            );
        }
    }

    #[test]
    fn proto_connection_type_wins_then_serial_inference() {
        assert_eq!(
            TransportKind::from_proto(1, "whatever", None),
            TransportKind::Usb
        );
        assert_eq!(
            TransportKind::from_proto(2, "192.168.1.5:5555", None),
            TransportKind::Tcp
        );
        assert_eq!(
            TransportKind::from_proto(2, "emulator-5554", None),
            TransportKind::Local
        );
        assert_eq!(
            TransportKind::from_proto(0, "YTGUSCNFMFAIK7ZP", Some("4-1.2")),
            TransportKind::Usb
        );
        assert_eq!(
            TransportKind::from_proto(0, "192.168.1.5:5555", None),
            TransportKind::Tcp
        );
    }

    #[test]
    fn text_bus_address_means_usb() {
        assert_eq!(
            TransportKind::from_text("YTGUSCNFMFAIK7ZP", true),
            TransportKind::Usb
        );
        assert_eq!(
            TransportKind::from_text("192.168.1.5:5555", false),
            TransportKind::Tcp
        );
        assert_eq!(
            TransportKind::from_text("emulator-5554", false),
            TransportKind::Local
        );
    }
}
