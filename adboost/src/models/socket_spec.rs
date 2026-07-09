use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

/// Host-side listener endpoint (forward LOCAL).
///
/// Only specs valid as a host listener appear here. `vsock` and `jdwp` are
/// remote-only in the AOSP protocol and intentionally excluded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LocalSocketSpec {
    Tcp(u16),
}

/// Device-side connect endpoint (forward REMOTE).
///
/// Sent verbatim as the service string in the `A_OPEN` payload to adbd.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RemoteSocketSpec {
    Tcp(u16),
    Vsock { cid: u32, port: u32 },
}

// ---------------------------------------------------------------------------
// Display — produces the wire-format string
// ---------------------------------------------------------------------------

impl Display for LocalSocketSpec {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(port) => write!(f, "tcp:{port}"),
        }
    }
}

impl Display for RemoteSocketSpec {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(port) => write!(f, "tcp:{port}"),
            Self::Vsock { cid, port } => write!(f, "vsock:{cid}:{port}"),
        }
    }
}

// ---------------------------------------------------------------------------
// FromStr — parses user / protocol input
// ---------------------------------------------------------------------------

impl FromStr for LocalSocketSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(port_str) = s.strip_prefix("tcp:") {
            let port = port_str
                .parse::<u16>()
                .map_err(|_| format!("bad local spec: invalid tcp port in \"{s}\""))?;
            return Ok(Self::Tcp(port));
        }
        Err(format!("bad local spec: unsupported scheme in \"{s}\""))
    }
}

impl FromStr for RemoteSocketSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(port_str) = s.strip_prefix("tcp:") {
            let port = port_str
                .parse::<u16>()
                .map_err(|_| format!("bad remote spec: invalid tcp port in \"{s}\""))?;
            return Ok(Self::Tcp(port));
        }
        if let Some(rest) = s.strip_prefix("vsock:") {
            let (cid_str, port_str) = rest.split_once(':').ok_or_else(|| {
                format!(
                    "bad remote spec: vsock requires cid and port (vsock:<cid>:<port>), got \"{s}\""
                )
            })?;
            let cid = cid_str
                .parse::<u32>()
                .map_err(|_| format!("bad remote spec: invalid vsock cid in \"{s}\""))?;
            let port = port_str
                .parse::<u32>()
                .map_err(|_| format!("bad remote spec: invalid vsock port in \"{s}\""))?;
            return Ok(Self::Vsock { cid, port });
        }
        Err(format!("bad remote spec: unsupported scheme in \"{s}\""))
    }
}

// ---------------------------------------------------------------------------
// Convenience accessors
// ---------------------------------------------------------------------------

impl LocalSocketSpec {
    /// Returns the TCP port if this is a `Tcp` spec.
    pub fn tcp_port(&self) -> u16 {
        match self {
            Self::Tcp(port) => *port,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- LocalSocketSpec ---

    #[test]
    fn local_parse_tcp() {
        assert_eq!(
            "tcp:8080".parse::<LocalSocketSpec>().unwrap(),
            LocalSocketSpec::Tcp(8080),
            "should parse valid tcp port"
        );
    }

    #[test]
    fn local_parse_tcp_zero() {
        assert_eq!(
            "tcp:0".parse::<LocalSocketSpec>().unwrap(),
            LocalSocketSpec::Tcp(0),
            "port 0 means auto-assign"
        );
    }

    #[test]
    fn local_parse_tcp_invalid_port() {
        let err = "tcp:99999".parse::<LocalSocketSpec>().unwrap_err();
        assert!(
            err.contains("invalid tcp port"),
            "should mention invalid port: {err}"
        );
    }

    #[test]
    fn local_parse_tcp_non_numeric() {
        let err = "tcp:abc".parse::<LocalSocketSpec>().unwrap_err();
        assert!(
            err.contains("invalid tcp port"),
            "should mention invalid port: {err}"
        );
    }

    #[test]
    fn local_parse_unsupported_scheme() {
        let err = "vsock:2:5555".parse::<LocalSocketSpec>().unwrap_err();
        assert!(
            err.contains("unsupported scheme"),
            "vsock not valid as local: {err}"
        );
    }

    #[test]
    fn local_display_tcp() {
        assert_eq!(
            LocalSocketSpec::Tcp(1234).to_string(),
            "tcp:1234",
            "Display should produce wire format"
        );
    }

    // --- RemoteSocketSpec ---

    #[test]
    fn remote_parse_tcp() {
        assert_eq!(
            "tcp:5555".parse::<RemoteSocketSpec>().unwrap(),
            RemoteSocketSpec::Tcp(5555),
            "should parse valid tcp remote"
        );
    }

    #[test]
    fn remote_parse_vsock() {
        assert_eq!(
            "vsock:2:46668".parse::<RemoteSocketSpec>().unwrap(),
            RemoteSocketSpec::Vsock {
                cid: 2,
                port: 46668
            },
            "should parse valid vsock spec"
        );
    }

    #[test]
    fn remote_parse_vsock_large_values() {
        assert_eq!(
            "vsock:4294967295:4294967295"
                .parse::<RemoteSocketSpec>()
                .unwrap(),
            RemoteSocketSpec::Vsock {
                cid: u32::MAX,
                port: u32::MAX
            },
            "should accept u32::MAX for both cid and port"
        );
    }

    #[test]
    fn remote_parse_vsock_missing_port() {
        let err = "vsock:2".parse::<RemoteSocketSpec>().unwrap_err();
        assert!(
            err.contains("vsock requires cid and port"),
            "should require both cid and port: {err}"
        );
    }

    #[test]
    fn remote_parse_vsock_invalid_cid() {
        let err = "vsock:abc:123".parse::<RemoteSocketSpec>().unwrap_err();
        assert!(
            err.contains("invalid vsock cid"),
            "should mention invalid cid: {err}"
        );
    }

    #[test]
    fn remote_parse_vsock_invalid_port() {
        let err = "vsock:2:abc".parse::<RemoteSocketSpec>().unwrap_err();
        assert!(
            err.contains("invalid vsock port"),
            "should mention invalid port: {err}"
        );
    }

    #[test]
    fn remote_parse_vsock_overflow_cid() {
        let err = "vsock:4294967296:123"
            .parse::<RemoteSocketSpec>()
            .unwrap_err();
        assert!(
            err.contains("invalid vsock cid"),
            "cid > u32::MAX should fail: {err}"
        );
    }

    #[test]
    fn remote_parse_vsock_overflow_port() {
        let err = "vsock:2:4294967296"
            .parse::<RemoteSocketSpec>()
            .unwrap_err();
        assert!(
            err.contains("invalid vsock port"),
            "port > u32::MAX should fail: {err}"
        );
    }

    #[test]
    fn remote_parse_unsupported_scheme() {
        let err = "localabstract:foo".parse::<RemoteSocketSpec>().unwrap_err();
        assert!(
            err.contains("unsupported scheme"),
            "localabstract not yet supported: {err}"
        );
    }

    #[test]
    fn remote_display_tcp() {
        assert_eq!(
            RemoteSocketSpec::Tcp(5555).to_string(),
            "tcp:5555",
            "Display should produce wire format"
        );
    }

    #[test]
    fn remote_display_vsock() {
        assert_eq!(
            RemoteSocketSpec::Vsock {
                cid: 2,
                port: 46668
            }
            .to_string(),
            "vsock:2:46668",
            "Display should produce wire format"
        );
    }

    #[test]
    fn round_trip_local_tcp() {
        let spec = LocalSocketSpec::Tcp(8080);
        let parsed: LocalSocketSpec = spec.to_string().parse().expect("round-trip should work");
        assert_eq!(spec, parsed, "Display → FromStr round-trip");
    }

    #[test]
    fn round_trip_remote_vsock() {
        let spec = RemoteSocketSpec::Vsock {
            cid: 2,
            port: 46668,
        };
        let parsed: RemoteSocketSpec = spec.to_string().parse().expect("round-trip should work");
        assert_eq!(spec, parsed, "Display → FromStr round-trip");
    }
}
