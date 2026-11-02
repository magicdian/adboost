use crate::tracker::models::DeviceInfo;

/// One complete device-list snapshot.
///
/// The adb server sends one length-prefixed frame per device-set change; each
/// frame maps to exactly one `DeviceSnapshot`. A zero-length frame is an
/// authoritative empty snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceSnapshot {
    /// The validated devices in this snapshot.
    pub devices: Vec<DeviceInfo>,
    /// Rows that failed validation or decoding. They are never inserted into
    /// `devices`; callers can log or alert on this count.
    pub malformed_rows: usize,
}

impl DeviceSnapshot {
    /// Whether the snapshot contains no devices.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceSnapshot;

    #[test]
    fn default_snapshot_is_authoritatively_empty() {
        let snapshot = DeviceSnapshot::default();
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.malformed_rows, 0);
    }
}
