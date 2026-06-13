//! Security policy for device-initiated reverse connections.
//!
//! Lives in `usb::` (not `server::`) because it parameterises the reusable
//! [`ReverseEngine`][crate::usb::ReverseEngine] data path, which any "acts-as-a-server"
//! backend can compose — independently of the `server` frontend. The `server`
//! module re-exports it (`server::ReversePolicy`) for backward compatibility.

/// Policy deciding which device-initiated reverse connections are accepted.
///
/// `reverse:` makes the *device* open connections back to a host target. A
/// compromised or buggy device could try to reach an arbitrary host port, so the
/// server validates each inbound `A_OPEN` against this policy before dialing.
/// The library does not hard-code a choice — the caller picks the security
/// posture; the bundled CLI uses [`ReversePolicy::RejectUnconfigured`].
#[derive(Clone, Default)]
pub enum ReversePolicy {
    /// Accept only inbound opens whose target matches a reverse rule the client
    /// explicitly configured (`reverse:forward:`). Anything else is closed. This
    /// is the safe default and mirrors AOSP's allow-list hardening (minus its
    /// process-abort).
    #[default]
    RejectUnconfigured,
    /// Accept any device-initiated open (relay / advanced use). **Unsafe**: the
    /// device may ask the server to connect to any local target. Opt in only
    /// when the device is fully trusted.
    AllowAll,
    /// Caller-supplied predicate over the target endpoint string (e.g. `tcp:5201`).
    /// Returning `true` accepts; `false` closes the stream.
    Custom(std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

impl std::fmt::Debug for ReversePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RejectUnconfigured => f.write_str("RejectUnconfigured"),
            Self::AllowAll => f.write_str("AllowAll"),
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_reject_unconfigured() {
        assert!(
            matches!(ReversePolicy::default(), ReversePolicy::RejectUnconfigured),
            "default reverse policy must be the safe RejectUnconfigured"
        );
    }
}
