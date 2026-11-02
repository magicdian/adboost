use std::fmt;

/// The connection state of a device, mirroring AOSP's
/// `enum ConnectionState` and `connection_state_name()`.
///
/// The protobuf tracker maps enum values 0..=12 to the variants below; any
/// value a future adb adds is represented by [`Self::Unknown`] instead of
/// dropping the device. The long-text tracker maps known wire prefixes and
/// degrades unknown text the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    /// `Any` (`kCsAny`; proto value 0).
    Any,
    /// The device has not answered yet.
    Connecting,
    /// Authorizing with keys from `ADB_VENDOR_KEYS`.
    Authorizing,
    /// ADB keys were exhausted; the user must confirm.
    Unauthorized,
    /// Insufficient permissions to communicate with the device.
    NoPermission,
    /// The USB device is known but not opened/claimed.
    Detached,
    /// A peer was detected but communication has not started.
    Offline,
    /// The device is running fastboot/fastbootd.
    Bootloader,
    /// The device is ready for commands.
    Device,
    /// What a device sees from its end of a transport.
    Host,
    /// Bootloader loaded, no ROM OS.
    Recovery,
    /// Sideload mode.
    Sideload,
    /// Rescue mode.
    Rescue,
    /// A proto enum value or text state this build does not know.
    Unknown,
}

impl ConnectionState {
    pub(crate) fn from_proto(value: i32) -> Self {
        match value {
            0 => Self::Any,
            1 => Self::Connecting,
            2 => Self::Authorizing,
            3 => Self::Unauthorized,
            4 => Self::NoPermission,
            5 => Self::Detached,
            6 => Self::Offline,
            7 => Self::Bootloader,
            8 => Self::Device,
            9 => Self::Host,
            10 => Self::Recovery,
            11 => Self::Sideload,
            12 => Self::Rescue,
            _ => Self::Unknown,
        }
    }

    /// Parse the longest known state prefix at the start of `value`.
    ///
    /// Returns the state and the number of bytes consumed. AOSP's
    /// `no permissions` state is followed by a long parenthetical help text
    /// (`no permissions (user in plugdev group; ...)`), so the prefix - not the
    /// whole whitespace-delimited token - is the state.
    pub(crate) fn parse_prefix(value: &str) -> (Self, usize) {
        // Longest first: "no permissions" must win over any shorter prefix.
        const PREFIXES: &[(&str, ConnectionState)] = &[
            ("no permissions", ConnectionState::NoPermission),
            ("unauthorized", ConnectionState::Unauthorized),
            ("authorizing", ConnectionState::Authorizing),
            ("connecting", ConnectionState::Connecting),
            ("bootloader", ConnectionState::Bootloader),
            ("detached", ConnectionState::Detached),
            ("sideload", ConnectionState::Sideload),
            ("recovery", ConnectionState::Recovery),
            ("offline", ConnectionState::Offline),
            ("noperm", ConnectionState::NoPermission),
            ("device", ConnectionState::Device),
            ("rescue", ConnectionState::Rescue),
            ("host", ConnectionState::Host),
            ("any", ConnectionState::Any),
        ];

        let lower = value.to_ascii_lowercase();
        for (prefix, state) in PREFIXES {
            if lower
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.chars().next().is_none_or(char::is_whitespace))
            {
                return (*state, prefix.len());
            }
        }

        // Unknown state text: consume the first token so attribute parsing can
        // still find a trailing transport_id.
        let consumed = value.find(char::is_whitespace).unwrap_or(value.len());
        (Self::Unknown, consumed)
    }
}

impl fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("any"),
            Self::Connecting => f.write_str("connecting"),
            Self::Authorizing => f.write_str("authorizing"),
            Self::Unauthorized => f.write_str("unauthorized"),
            Self::NoPermission => f.write_str("no permissions"),
            Self::Detached => f.write_str("detached"),
            Self::Offline => f.write_str("offline"),
            Self::Bootloader => f.write_str("bootloader"),
            Self::Device => f.write_str("device"),
            Self::Host => f.write_str("host"),
            Self::Recovery => f.write_str("recovery"),
            Self::Sideload => f.write_str("sideload"),
            Self::Rescue => f.write_str("rescue"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConnectionState;

    #[test]
    fn proto_values_match_aosp() {
        let cases = [
            (0, ConnectionState::Any),
            (1, ConnectionState::Connecting),
            (2, ConnectionState::Authorizing),
            (3, ConnectionState::Unauthorized),
            (4, ConnectionState::NoPermission),
            (5, ConnectionState::Detached),
            (6, ConnectionState::Offline),
            (7, ConnectionState::Bootloader),
            (8, ConnectionState::Device),
            (9, ConnectionState::Host),
            (10, ConnectionState::Recovery),
            (11, ConnectionState::Sideload),
            (12, ConnectionState::Rescue),
            (99, ConnectionState::Unknown),
            (-1, ConnectionState::Unknown),
        ];
        for (value, expected) in cases {
            assert_eq!(
                ConnectionState::from_proto(value),
                expected,
                "proto value {value} decoded incorrectly"
            );
        }
    }

    #[test]
    fn parses_no_permissions_with_help_text() {
        let text = "no permissions (user in plugdev group; are your udev rules wrong?)";
        let (state, consumed) = ConnectionState::parse_prefix(text);
        assert_eq!(state, ConnectionState::NoPermission);
        assert_eq!(
            &text[consumed..],
            " (user in plugdev group; are your udev rules wrong?)"
        );
    }

    #[test]
    fn parses_known_states_and_degrades_unknown_text() {
        for (text, expected) in [
            ("device usb:1-5", ConnectionState::Device),
            ("offline", ConnectionState::Offline),
            ("unauthorized", ConnectionState::Unauthorized),
            ("weird-state product:x", ConnectionState::Unknown),
        ] {
            let (state, _) = ConnectionState::parse_prefix(text);
            assert_eq!(state, expected, "state prefix {text:?} decoded incorrectly");
        }
    }

    #[test]
    fn display_uses_aosp_wire_spelling() {
        assert_eq!(ConnectionState::NoPermission.to_string(), "no permissions");
        assert_eq!(ConnectionState::Device.to_string(), "device");
        assert_eq!(ConnectionState::Unknown.to_string(), "unknown");
    }
}
