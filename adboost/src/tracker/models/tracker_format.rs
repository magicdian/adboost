use std::fmt;

/// The wire format the tracker negotiated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrackerFormat {
    /// `host:track-devices-proto-binary` (adb >= 35.0.2).
    ProtoBinary,
    /// `host:track-devices-l` (AOSP space-padded long text).
    LongText,
}

impl TrackerFormat {
    pub(crate) const fn service(self) -> &'static str {
        match self {
            Self::ProtoBinary => "host:track-devices-proto-binary",
            Self::LongText => "host:track-devices-l",
        }
    }
}

impl fmt::Display for TrackerFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtoBinary => f.write_str("proto-binary"),
            Self::LongText => f.write_str("long-text"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TrackerFormat;

    #[test]
    fn service_names_match_the_host_protocol() {
        assert_eq!(
            TrackerFormat::ProtoBinary.service(),
            "host:track-devices-proto-binary"
        );
        assert_eq!(TrackerFormat::LongText.service(), "host:track-devices-l");
    }
}
