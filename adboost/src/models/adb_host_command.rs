use std::{fmt::Display, net::SocketAddrV4};

use crate::models::DeviceSelector;
use crate::proxy::{WaitForDeviceState, WaitForDeviceTransport};

/// ADB commands that relates to the host and are handled by the ADB server.
pub enum ADBHostCommand {
    Version,
    Kill,
    Devices,
    DevicesLong,
    TrackDevices,
    HostFeatures,
    Connect(SocketAddrV4),
    Disconnect(SocketAddrV4),
    Pair(SocketAddrV4, String),
    TransportAny,
    TransportSerial(String),
    TransportId(u32),
    MDNSCheck,
    MDNSServices,
    ServerStatus,
    ReconnectOffline,
    WaitForDevice(WaitForDeviceState, WaitForDeviceTransport),
    /// Add a forward rule (`{selector}forward:<local>;<remote>`).
    ///
    /// `forward`/`killforward` are **device-pinned host services**: they are
    /// scoped to one device by the `host-serial:`/`host-transport-id:` prefix that
    /// [`DeviceSelector`] renders — NOT by a preceding `host:transport:` switch
    /// (which the server does not bind to a subsequent bare `host:forward`, so it
    /// auto-selects and fails with `more than one device/emulator` on ≥2 devices).
    ///
    /// Rebind (no `norebind:`) matches native `adb forward`'s default: a repeated
    /// `local` silently replaces the existing rule. (`--no-rebind` is a separate
    /// opt-in native adb does not send by default; not modeled here.)
    Forward {
        selector: DeviceSelector,
        local: String,
        remote: String,
    },
    /// Remove one forward rule by its local endpoint
    /// (`{selector}killforward:<local>`). Device-pinned like [`Self::Forward`].
    KillForward {
        selector: DeviceSelector,
        local: String,
    },
    /// Remove every forward rule (`host:killforward-all`).
    ///
    /// Deliberately NOT device-scoped: AOSP's `killforward-all` is process-global
    /// (one global listener registry, `remove_all_listeners()` takes no transport),
    /// and native `adb -s <serial> forward --remove-all` still sends the bare
    /// `host:killforward-all`. Scoping it would diverge from adb with no upside.
    KillForwardAll,
}

impl Display for ADBHostCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version => write!(f, "host:version"),
            Self::Kill => write!(f, "host:kill"),
            Self::Devices => write!(f, "host:devices"),
            Self::DevicesLong => write!(f, "host:devices-l"),
            Self::TrackDevices => write!(f, "host:track-devices"),
            Self::TransportAny => write!(f, "host:transport-any"),
            Self::TransportSerial(serial) => write!(f, "host:transport:{serial}"),
            Self::TransportId(id) => write!(f, "host:transport-id:{id}"),
            Self::Connect(addr) => write!(f, "host:connect:{addr}"),
            Self::Disconnect(addr) => write!(f, "host:disconnect:{addr}"),
            Self::Pair(addr, code) => {
                write!(f, "host:pair:{code}:{addr}")
            }
            Self::MDNSCheck => write!(f, "host:mdns:check"),
            Self::MDNSServices => write!(f, "host:mdns:services"),
            Self::ServerStatus => write!(f, "host:server-status"),
            Self::ReconnectOffline => write!(f, "host:reconnect-offline"),
            Self::WaitForDevice(wait_for_device_state, wait_for_device_transport) => {
                write!(
                    f,
                    "host:wait-for-{wait_for_device_transport}-{wait_for_device_state}"
                )
            }
            Self::HostFeatures => write!(f, "host:features"),
            Self::Forward {
                selector,
                local,
                remote,
            } => write!(f, "{}forward:{local};{remote}", selector.host_prefix()),
            Self::KillForward { selector, local } => {
                write!(f, "{}killforward:{local}", selector.host_prefix())
            }
            Self::KillForwardAll => write!(f, "host:killforward-all"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ADBHostCommand;
    use crate::models::DeviceSelector;

    // The multi-device bug: a serial-known forward MUST render as a device-pinned
    // `host-serial:<serial>:forward:…` so the server never auto-selects (which
    // fails `more than one device/emulator` with ≥2 devices). This is the exact
    // assertion from the field bug report.
    #[test]
    fn forward_with_serial_is_host_serial_scoped() {
        let cmd = ADBHostCommand::Forward {
            selector: DeviceSelector::Serial("ABC123".to_string()),
            local: "tcp:17023".to_string(),
            remote: "tcp:17023".to_string(),
        };
        assert_eq!(
            cmd.to_string(),
            "host-serial:ABC123:forward:tcp:17023;tcp:17023"
        );
    }

    #[test]
    fn forward_with_transport_id_is_host_transport_id_scoped() {
        let cmd = ADBHostCommand::Forward {
            selector: DeviceSelector::TransportId(4),
            local: "tcp:1000".to_string(),
            remote: "tcp:2000".to_string(),
        };
        assert_eq!(
            cmd.to_string(),
            "host-transport-id:4:forward:tcp:1000;tcp:2000"
        );
    }

    // Asymmetric ports lock the `<local>;<remote>` order (a swap would surface here).
    #[test]
    fn forward_renders_local_then_remote() {
        let cmd = ADBHostCommand::Forward {
            selector: DeviceSelector::Serial("S".to_string()),
            local: "tcp:1111".to_string(),
            remote: "tcp:2222".to_string(),
        };
        assert_eq!(cmd.to_string(), "host-serial:S:forward:tcp:1111;tcp:2222");
    }

    // Auto-select fallback: neither id nor serial known → bare `host:forward:`
    // (native adb's single-device behavior; server resolves the only device).
    #[test]
    fn forward_with_any_falls_back_to_bare_host() {
        let cmd = ADBHostCommand::Forward {
            selector: DeviceSelector::Any,
            local: "tcp:1".to_string(),
            remote: "tcp:2".to_string(),
        };
        assert_eq!(cmd.to_string(), "host:forward:tcp:1;tcp:2");
    }

    #[test]
    fn killforward_with_serial_is_host_serial_scoped() {
        let cmd = ADBHostCommand::KillForward {
            selector: DeviceSelector::Serial("ABC123".to_string()),
            local: "tcp:17023".to_string(),
        };
        assert_eq!(cmd.to_string(), "host-serial:ABC123:killforward:tcp:17023");
    }

    // Remove-all is process-global in AOSP — bare `host:killforward-all`, never
    // serial-scoped, even though a serial may be configured on the device.
    #[test]
    fn killforward_all_is_always_global() {
        assert_eq!(
            ADBHostCommand::KillForwardAll.to_string(),
            "host:killforward-all"
        );
    }
}
