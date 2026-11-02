use std::fmt;

/// The tracker connection's health.
///
/// The tracker publishes `Disconnected` exactly once per failure episode (it
/// does not repeat it for every failed reconnect attempt) and `Connected` when
/// a connection is established again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackerHealth {
    /// The tracker has a live connection.
    Connected,
    /// The last connection failed or closed; the reason is diagnostic text.
    Disconnected {
        /// Why the connection ended.
        reason: String,
    },
}

impl TrackerHealth {
    /// Whether the tracker currently has a live connection.
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

impl fmt::Display for TrackerHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected => f.write_str("connected"),
            Self::Disconnected { reason } => write!(f, "disconnected: {reason}"),
        }
    }
}
