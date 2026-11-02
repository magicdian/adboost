use std::collections::BTreeSet;

/// The adb **server-level** feature set from `host:host-features`.
///
/// This is deliberately named `ServerFeatures`, not `HostFeatures`:
/// [`crate::HostFeatures`] is the **per-transport** `host:features` set used
/// for shell v1/v2 gating. The two queries are not interchangeable; the
/// tracker negotiates its format from the server-level set, exactly like
/// adblib's `SessionDeviceTracker`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerFeatures {
    features: BTreeSet<String>,
}

impl ServerFeatures {
    /// Whether the server advertised `feature`.
    #[must_use]
    pub fn contains(&self, feature: &str) -> bool {
        self.features.contains(feature)
    }

    /// Iterate over the advertised feature names in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.features.iter().map(String::as_str)
    }

    /// Whether the server supports `host:track-devices-proto-binary`
    /// (`devicetracker_proto_format`, platform-tools >= 35.0.2).
    #[must_use]
    pub fn supports_device_tracker_proto(&self) -> bool {
        self.contains("devicetracker_proto_format")
    }

    pub(crate) fn from_csv(csv: &str) -> Self {
        let features = csv
            .split(',')
            .map(str::trim)
            .filter(|feature| !feature.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        Self { features }
    }
}

#[cfg(test)]
mod tests {
    use super::ServerFeatures;

    #[test]
    fn parses_comma_separated_features() {
        let features = ServerFeatures::from_csv("cmd, shell_v2,,devicetracker_proto_format");
        assert!(features.contains("cmd"));
        assert!(features.contains("shell_v2"));
        assert!(!features.contains(""));
        assert!(features.supports_device_tracker_proto());
        assert_eq!(
            features.iter().collect::<Vec<_>>(),
            vec!["cmd", "devicetracker_proto_format", "shell_v2"],
            "feature iteration must be sorted"
        );
    }

    #[test]
    fn old_servers_do_not_support_proto() {
        let features = ServerFeatures::from_csv("cmd,shell_v2");
        assert!(!features.supports_device_tracker_proto());
    }

    #[test]
    fn empty_response_is_an_empty_set() {
        let features = ServerFeatures::from_csv("");
        assert!(features.iter().next().is_none());
    }
}
