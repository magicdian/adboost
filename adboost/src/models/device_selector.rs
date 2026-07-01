use std::borrow::Cow;

use crate::models::ADBHostCommand;

/// How a single device is addressed on the ADB **host** protocol.
///
/// This is the one source of truth for the selection precedence that every proxy
/// operation shares: prefer a `transport_id` (unique even when two devices report
/// the same serial), then fall back to the `serial`, and finally to "the only
/// device" when neither is known.
///
/// The ADB host protocol exposes a device in **two different ways**, and a
/// [`DeviceSelector`] renders both so the precedence is never duplicated:
///
/// 1. [`transport_switch_command`](Self::transport_switch_command) — a
///    `host:transport[-id]:…` / `host:transport-any` request that switches the
///    connection into device-transport mode. This is correct for genuine
///    *device* services (`shell:`, `sync:`, `reverse:forward:`, …) that are meant
///    to be issued *after* a transport switch.
/// 2. [`host_prefix`](Self::host_prefix) — the `host-serial:<serial>:` /
///    `host-transport-id:<id>:` / bare `host:` prefix for **device-pinned host
///    services** (`forward:` / `killforward:`). These are host services scoped to
///    one device by the prefix itself; they are NOT bound by a preceding
///    `host:transport:` switch, so a transport switch followed by a bare
///    `host:forward:` auto-selects a device and fails with
///    `more than one device/emulator` once ≥2 devices are attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSelector {
    /// Address by the server-assigned transport id (`host-transport-id:<id>:` /
    /// `host:transport-id:<id>`). Volatile — reassigned on reconnect / server
    /// restart — so it takes precedence but must be re-queried, never cached.
    TransportId(u32),
    /// Address by device serial (`host-serial:<serial>:` / `host:transport:<serial>`).
    Serial(String),
    /// No specific device: let the server auto-select the only one
    /// (`host:` / `host:transport-any`). Ambiguous with ≥2 devices — that
    /// ambiguity is the server's to report, matching native `adb`.
    Any,
}

impl DeviceSelector {
    /// The `host:transport*` request that switches the connection to this device,
    /// for issuing a subsequent *device* service (`shell:` etc.).
    ///
    /// Precedence: `transport_id` → `serial` → any.
    #[must_use]
    pub fn transport_switch_command(&self) -> ADBHostCommand {
        match self {
            Self::TransportId(id) => ADBHostCommand::TransportId(*id),
            Self::Serial(serial) => ADBHostCommand::TransportSerial(serial.clone()),
            Self::Any => ADBHostCommand::TransportAny,
        }
    }

    /// The device-pinning prefix for a **host** service (`forward:` /
    /// `killforward:`), rendering `host-serial:<serial>:` /
    /// `host-transport-id:<id>:` / bare `host:` (auto-select).
    ///
    /// Concatenate the sub-service after this: `"{prefix}forward:{local};{remote}"`.
    #[must_use]
    pub fn host_prefix(&self) -> Cow<'static, str> {
        match self {
            Self::TransportId(id) => Cow::Owned(format!("host-transport-id:{id}:")),
            Self::Serial(serial) => Cow::Owned(format!("host-serial:{serial}:")),
            Self::Any => Cow::Borrowed("host:"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceSelector;

    #[test]
    fn host_prefix_serial_is_host_serial_scoped() {
        assert_eq!(
            DeviceSelector::Serial("ABC123".to_string()).host_prefix(),
            "host-serial:ABC123:"
        );
    }

    #[test]
    fn host_prefix_transport_id_is_host_transport_id_scoped() {
        assert_eq!(
            DeviceSelector::TransportId(7).host_prefix(),
            "host-transport-id:7:"
        );
    }

    #[test]
    fn host_prefix_any_is_bare_host() {
        // Auto-select: neither serial nor id known. The server resolves the only
        // device (or reports ambiguity with ≥2) — same as native `adb forward`.
        assert_eq!(DeviceSelector::Any.host_prefix(), "host:");
    }

    #[test]
    fn transport_switch_command_maps_each_variant() {
        assert_eq!(
            DeviceSelector::TransportId(3)
                .transport_switch_command()
                .to_string(),
            "host:transport-id:3"
        );
        assert_eq!(
            DeviceSelector::Serial("S1".to_string())
                .transport_switch_command()
                .to_string(),
            "host:transport:S1"
        );
        assert_eq!(
            DeviceSelector::Any.transport_switch_command().to_string(),
            "host:transport-any"
        );
    }
}
