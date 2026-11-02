use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::tracker::models::{ReconnectPolicy, TrackerFormat};

/// The default local adb server address.
pub const DEFAULT_SERVER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037);

/// How a [`crate::tracker::DeviceTracker`] connects and recovers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerConfig {
    /// The adb server address.
    pub addr: SocketAddr,
    /// An explicit wire format, or `None` to negotiate via
    /// `host:host-features`.
    pub format: Option<TrackerFormat>,
    /// Reconnect policy, or `None` to return stream failures to the caller.
    pub reconnect: Option<ReconnectPolicy>,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            addr: DEFAULT_SERVER_ADDR,
            format: None,
            reconnect: Some(ReconnectPolicy::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::{DEFAULT_SERVER_ADDR, TrackerConfig};

    #[test]
    fn default_targets_the_local_adb_server() {
        assert_eq!(
            DEFAULT_SERVER_ADDR,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037)
        );
        let config = TrackerConfig::default();
        assert_eq!(config.addr, DEFAULT_SERVER_ADDR);
        assert!(config.format.is_none());
        assert!(config.reconnect.is_some());
    }
}
