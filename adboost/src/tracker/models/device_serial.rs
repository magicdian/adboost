use std::fmt;

use crate::tracker::TrackerError;

/// A validated ADB device serial.
///
/// The tracker never inserts a raw row as a device identity. A serial must be
/// non-empty, must not contain whitespace or control characters, and must fit
/// in 255 bytes (the AOSP transport serial limit).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceSerial(String);

impl DeviceSerial {
    /// Validate and wrap a raw serial.
    pub fn new(raw: impl Into<String>) -> Result<Self, TrackerError> {
        let raw = raw.into();
        let reason = if raw.is_empty() {
            Some("serial is empty")
        } else if raw.len() > 255 {
            Some("serial is longer than 255 bytes")
        } else if raw.chars().any(char::is_whitespace) {
            Some("serial contains whitespace")
        } else if raw.chars().any(char::is_control) {
            Some("serial contains a control character")
        } else {
            None
        };

        match reason {
            Some(reason) => Err(TrackerError::InvalidSerial {
                serial: raw,
                reason,
            }),
            None => Ok(Self(raw)),
        }
    }

    /// The serial as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceSerial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for DeviceSerial {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for DeviceSerial {
    type Error = TrackerError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for DeviceSerial {
    type Error = TrackerError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceSerial;
    use crate::tracker::TrackerError;

    #[test]
    fn accepts_real_serials() {
        let longest_valid = "a".repeat(255);
        for serial in [
            "YTGUSCNFMFAIK7ZP",
            "192.168.1.5:5555",
            "emulator-5554",
            longest_valid.as_str(),
        ] {
            DeviceSerial::new(serial)
                .unwrap_or_else(|error| panic!("serial {serial:?} should be valid: {error}"));
        }
    }

    #[test]
    fn rejects_empty_whitespace_control_and_overlong_serials() {
        let too_long = "a".repeat(256);
        for serial in [
            "",
            "has space",
            "has\ttab",
            "has\nnewline",
            "has\0nul",
            too_long.as_str(),
        ] {
            let error = DeviceSerial::new(serial).expect_err("serial should be rejected");
            assert!(
                matches!(error, TrackerError::InvalidSerial { .. }),
                "unexpected error for {serial:?}: {error:?}"
            );
        }
    }

    #[test]
    fn try_from_str_matches_new() {
        let serial = DeviceSerial::try_from("ABC123").expect("valid serial");
        assert_eq!(serial.as_str(), "ABC123");
    }
}
