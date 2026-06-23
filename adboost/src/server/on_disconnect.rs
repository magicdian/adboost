//! Policy deciding what happens to a device's `forward` / `reverse` rules when
//! its transport disconnects (USB unplug, TCP drop, or the persistent
//! connection's reader task dying).
//!
//! Standard `adb` releases a device's forwards the moment the device goes away;
//! adboost historically kept them (the host-side listener lingered and
//! `forward --list` still showed the rule). This policy restores the standard
//! behavior **by default** while letting a caller opt out — adboost's
//! "standard defaults + opt-in customization" stance.
//!
//! It mirrors the shape of [`ReversePolicy`](crate::usb::ReversePolicy): a small
//! enum whose escape hatch is a caller-supplied closure. One policy governs
//! **both** forward and reverse rules for the disconnected serial (unified
//! semantics): when the device is gone, so is everything it was forwarding.

use std::sync::Arc;

/// What to do with a serial's `forward` + `reverse` rules when its transport
/// disconnects.
///
/// The same policy applies to both rule kinds — a disconnected device loses its
/// host-side forward listeners *and* its reverse rules together. The notify
/// variant is purely informational: adboost releases nothing on the caller's
/// behalf, leaving them to decide via the active-cleanup API
/// ([`ForwardHandle`](crate::server::ForwardHandle)).
#[derive(Clone, Default)]
pub enum OnDisconnect {
    /// Release the disconnected serial's forward listeners and reverse rules
    /// automatically. This is the default and mirrors standard `adb`: a device
    /// that goes away takes its forwards with it.
    #[default]
    ReleaseAll,
    /// Keep every rule in place; the caller manages release itself (e.g. it
    /// intends to reconnect the same serial and reuse the rules). Rules persist
    /// in the registry and keep showing in `forward --list` until the caller
    /// releases them via [`ForwardHandle`](crate::server::ForwardHandle).
    Retain,
    /// Pure notification: invoke the callback with the disconnected serial and
    /// release nothing. The callback decides what (if anything) to release,
    /// typically by calling [`ForwardHandle`](crate::server::ForwardHandle)
    /// methods it captured. The callback must be cheap and non-blocking; it runs
    /// on the frontend's disconnect-handling task.
    Notify(Arc<dyn Fn(&str) + Send + Sync>),
}

impl std::fmt::Debug for OnDisconnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReleaseAll => f.write_str("ReleaseAll"),
            Self::Retain => f.write_str("Retain"),
            Self::Notify(_) => f.write_str("Notify(<fn>)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_release_all() {
        assert!(
            matches!(OnDisconnect::default(), OnDisconnect::ReleaseAll),
            "default disconnect policy must be ReleaseAll (align with standard adb)"
        );
    }

    #[test]
    fn debug_does_not_leak_closure() {
        // The closure variant must render a stable, non-revealing label.
        let p = OnDisconnect::Notify(Arc::new(|_serial: &str| {}));
        assert_eq!(format!("{p:?}"), "Notify(<fn>)");
        assert_eq!(format!("{:?}", OnDisconnect::ReleaseAll), "ReleaseAll");
        assert_eq!(format!("{:?}", OnDisconnect::Retain), "Retain");
    }

    #[test]
    fn notify_callback_receives_serial() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_cl = Arc::clone(&seen);
        let policy = OnDisconnect::Notify(Arc::new(move |serial: &str| {
            seen_cl.lock().expect("test lock").push(serial.to_owned());
        }));
        if let OnDisconnect::Notify(cb) = &policy {
            cb("YTGUSCNFMFAIK7ZP");
        }
        assert_eq!(
            seen.lock().expect("test lock").as_slice(),
            ["YTGUSCNFMFAIK7ZP"],
            "Notify must invoke the callback with the disconnected serial"
        );
    }
}
