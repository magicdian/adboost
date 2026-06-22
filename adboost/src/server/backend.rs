//! The device-backend seam: where protocol ends and "how devices are reached"
//! begins.
//!
//! [`DeviceBackend`] is the single injection point of the server frontend. The
//! frontend owns the host protocol, transport-id computation, and session
//! bridging; the backend owns *where devices come from* and *how a local
//! service is opened*. adboost ships [`DefaultDeviceBackend`] (a thin wrapper over
//! [`PersistentUsbConnection`]); downstreams (e.g. xdb) can inject their own to
//! weave in custom discovery / relay / auth without reimplementing any protocol.
//!
//! [`PersistentUsbConnection`]: crate::usb::PersistentUsbConnection
//! [`DefaultDeviceBackend`]: crate::server::DefaultDeviceBackend

use tokio::sync::mpsc;

use crate::models::{ADBLocalCommand, DeviceFeatureSet};
use crate::usb::{MultiplexedSession, ShellV2Session, SyncSession};
use crate::{Result, RustADBError};

// `ReversePolicy` now lives in `usb::` (it parameterises the reusable
// `ReverseEngine` data path). Re-export it from this module's public surface so
// existing `server::ReversePolicy` users keep compiling.
pub use crate::usb::ReversePolicy;

/// One device visible to clients. `state` is usually [`DeviceState::Device`]
/// (the wire equivalent of AOSP's `ConnectionState`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    /// The device serial — the authoritative identifier, also what
    /// transport-ids are computed over (see [`super::protocol::transport_id_for`]).
    pub serial: String,
    /// Connection state reported to clients.
    pub state: DeviceState,
    /// Optional `devices-l` extras.
    pub product: Option<String>,
    /// Optional `devices-l` model.
    pub model: Option<String>,
    /// Optional `devices-l` device name.
    pub device: Option<String>,
    /// The device's own advertised capabilities, parsed from its CNXN banner —
    /// the per-device truth source the frontend negotiates `shell_v2` / `sync_v2`
    /// against (so a feature-less device is never offered a framing it rejects).
    ///
    /// `None` means **not yet known**: the device is listed but has not completed
    /// a CNXN handshake (e.g. a USB device enumerated from descriptors but never
    /// opened). The frontend treats `None` conservatively — it does not offer the
    /// wire-framing-changing optional features. A handshaked device (every
    /// `host:connect`ed TCP device, and any USB device after first open) carries
    /// `Some`. See [`DeviceBackend::device_capabilities`] for the authoritative,
    /// on-demand query that can drive a handshake within a timeout.
    pub capabilities: Option<DeviceFeatureSet>,
}

impl DeviceEntry {
    /// Construct a minimal entry: just a serial in the [`DeviceState::Device`]
    /// state, with no `devices-l` extras and unknown capabilities (`None`).
    #[must_use]
    pub fn new(serial: impl Into<String>) -> Self {
        Self {
            serial: serial.into(),
            state: DeviceState::Device,
            product: None,
            model: None,
            device: None,
            capabilities: None,
        }
    }

    /// Set the device's advertised capabilities (parsed from its CNXN banner).
    /// Builder for the enumeration paths that already hold a live connection.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: DeviceFeatureSet) -> Self {
        self.capabilities = Some(capabilities);
        self
    }
}

/// A device's connection state, in its host-protocol wire spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceState {
    /// Ready for commands (`device`).
    Device,
    /// Known but not currently reachable (`offline`).
    Offline,
    /// Connected but the host key is not yet authorized (`unauthorized`).
    Unauthorized,
}

impl DeviceState {
    /// The wire string used in `host:devices` / `host:devices-l` /
    /// `host-serial:*:get-state` replies.
    #[must_use]
    pub fn as_wire(&self) -> &'static str {
        match self {
            DeviceState::Device => "device",
            DeviceState::Offline => "offline",
            DeviceState::Unauthorized => "unauthorized",
        }
    }
}

/// What a [`DeviceBackend`] is actually able to bridge beyond the always-present
/// `shell:` v1 / `tcp:` local services.
///
/// This is the **honest-banner source of truth**: the frontend asks the backend
/// for this before deciding which optional `host:features` to advertise
/// (`sync_v2` / `shell_v2`). A backend that does not override the corresponding
/// trait method must leave the flag `false` here — advertising a capability the
/// bridge cannot satisfy desyncs clients (see [`super::capabilities`]).
///
/// All flags default to `false` so the conservative behavior is the default; a
/// backend opts in only for what it genuinely implements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackendCapabilities {
    /// The backend implements [`DeviceBackend::open_sync_session`] (`adb push`/`pull`).
    pub sync: bool,
    /// The backend implements [`DeviceBackend::open_shell_v2`] (separated
    /// stdout/stderr + exit code).
    pub shell_v2: bool,
    /// The backend implements the reverse family
    /// ([`DeviceBackend::open_reverse`] etc. — device-initiated port forwarding).
    pub reverse: bool,
}

/// The device backend: the frontend gets its device list, change stream, and
/// local-service sessions through this trait.
///
/// adboost provides [`DefaultDeviceBackend`] implementing it over the existing
/// [`PersistentUsbConnection`]; callers may inject a custom implementation to
/// weave in bespoke discovery / relay / auth in `list_devices` / `open_local_service`
/// without rewriting any protocol.
///
/// All methods are `async`; [`trait_variant::make`] generates the `Send`
/// variant so the backend can be driven from a multi-threaded tokio runtime
/// (matching [`crate::ADBDeviceExt`] / the crate-internal `ADBTransport`).
///
/// [`PersistentUsbConnection`]: crate::usb::PersistentUsbConnection
/// [`DefaultDeviceBackend`]: crate::server::DefaultDeviceBackend
#[trait_variant::make(Send)]
pub trait DeviceBackend: Send + Sync + 'static {
    /// The authoritative device set for `host:devices` / `host:devices-l` and
    /// transport-id computation.
    async fn list_devices(&self) -> Vec<DeviceEntry>;

    /// The `host:track-devices` change stream. The backend pushes a full
    /// snapshot whenever the device set changes; the receiver closes when the
    /// backend stops watching.
    async fn subscribe_changes(&self) -> mpsc::Receiver<Vec<DeviceEntry>>;

    /// Open a local service (`shell:` / `tcp:`) on a device, returning a
    /// bidirectionally-bridgeable session. Implementations reuse adboost's
    /// existing [`PersistentUsbConnection::open_session`].
    ///
    /// [`PersistentUsbConnection::open_session`]: crate::usb::PersistentUsbConnection::open_session
    async fn open_local_service(
        &self,
        serial: &str,
        cmd: &ADBLocalCommand,
    ) -> Result<MultiplexedSession>;

    /// What optional services this backend can bridge, beyond the always-present
    /// `shell:` v1 / `tcp:`. The frontend consults this to decide which
    /// `host:features` to advertise honestly (see [`BackendCapabilities`]).
    ///
    /// Defaults to all-`false`: a backend that does not override the matching
    /// `open_*` method below must not claim the capability here.
    ///
    // NOTE: `#[trait_variant::make(Send)]` rewrites this `async fn` into
    // `fn ... -> impl Future + Send { <body> }`, so the default body must itself
    // be a future — hence the explicit `async move` block (a bare expression
    // would not type-check post-rewrite). Same for the two methods below.
    async fn capabilities(&self) -> BackendCapabilities {
        async move { BackendCapabilities::default() }
    }

    /// The advertised capabilities of **one specific device**, parsed from its
    /// CNXN banner — the authoritative, on-demand per-device query.
    ///
    /// Unlike [`Self::list_devices`] (which stays lightweight and may report
    /// `DeviceEntry::capabilities == None` for a device it has not handshaked),
    /// this is allowed to *establish* the device connection to learn its banner,
    /// bounded by `timeout`. The frontend calls it when it actually needs a
    /// device's capabilities — answering a post-transport `host:features` or
    /// gating `shell,v2` / `sync:` — so the cost of a handshake is paid only when
    /// the capability is consumed, never for a bare `host:devices` listing.
    ///
    /// Returns `None` when the capability cannot be determined within `timeout`
    /// (device unreachable, handshake slow, or serial unknown); the frontend
    /// treats `None` conservatively (does not offer the framing-changing optional
    /// features). The default implementation returns `None` so existing backends
    /// keep compiling and simply never advertise per-device optional features.
    async fn device_capabilities(
        &self,
        _serial: &str,
        _timeout: std::time::Duration,
    ) -> Option<DeviceFeatureSet> {
        async move { None }
    }

    /// Open a SYNC v1 file-transfer session (`sync:`) for `adb push`/`pull`.
    ///
    /// The default returns an `unsupported` error so existing backends keep
    /// compiling unchanged; override it (and set [`BackendCapabilities::sync`])
    /// to enable `sync:` bridging. adboost's [`DefaultDeviceBackend`] forwards to
    /// [`PersistentUsbConnection::open_sync_session`].
    ///
    /// [`DefaultDeviceBackend`]: crate::server::DefaultDeviceBackend
    /// [`PersistentUsbConnection::open_sync_session`]: crate::usb::PersistentUsbConnection::open_sync_session
    async fn open_sync_session(&self, _serial: &str) -> Result<SyncSession> {
        async move {
            Err(RustADBError::ADBRequestFailed(
                "sync not supported by this backend".into(),
            ))
        }
    }

    /// Open a `shell,v2` session, returning a decoder for the inner
    /// `[id][len][payload]` framing (separated stdout/stderr + exit code).
    ///
    /// The default returns an `unsupported` error so existing backends keep
    /// compiling unchanged; override it (and set
    /// [`BackendCapabilities::shell_v2`]) to enable `shell,v2` bridging.
    /// adboost's [`DefaultDeviceBackend`] forwards to
    /// [`PersistentUsbConnection::open_shell_v2`].
    ///
    /// [`DefaultDeviceBackend`]: crate::server::DefaultDeviceBackend
    /// [`PersistentUsbConnection::open_shell_v2`]: crate::usb::PersistentUsbConnection::open_shell_v2
    async fn open_shell_v2(&self, _serial: &str, _cmd: &str) -> Result<ShellV2Session> {
        async move {
            Err(RustADBError::ADBRequestFailed(
                "shell_v2 not supported by this backend".into(),
            ))
        }
    }

    /// Establish a reverse rule: ask the device to listen on `remote` (e.g.
    /// `tcp:5201`) and tunnel inbound connections back to the host target
    /// `local`. The backend owns the whole reverse data path — it sets up the
    /// device listener, records the allow-list rule, and (lazily) runs the pump
    /// that accepts device-initiated opens and bridges them to `local`.
    ///
    /// The default returns `unsupported`; set [`BackendCapabilities::reverse`]
    /// and override to enable. Note the AOSP arg order: `remote` is the
    /// device-listen endpoint, `local` is the host-connect target (opposite of
    /// forward).
    async fn open_reverse(&self, _serial: &str, _remote: &str, _local: &str) -> Result<()> {
        async move {
            Err(RustADBError::ADBRequestFailed(
                "reverse not supported by this backend".into(),
            ))
        }
    }

    /// Remove the reverse rule whose device-listen endpoint is `remote`.
    async fn reverse_remove(&self, _serial: &str, _remote: &str) -> Result<()> {
        async move {
            Err(RustADBError::ADBRequestFailed(
                "reverse not supported by this backend".into(),
            ))
        }
    }

    /// Remove every reverse rule for `serial`.
    async fn reverse_remove_all(&self, _serial: &str) -> Result<()> {
        async move {
            Err(RustADBError::ADBRequestFailed(
                "reverse not supported by this backend".into(),
            ))
        }
    }

    /// List active reverse rules for `serial` as the `host:list-forward` body
    /// would render them (`(reverse) <remote> <local>\n` per rule).
    async fn list_reverse(&self, _serial: &str) -> Result<String> {
        async move {
            Err(RustADBError::ADBRequestFailed(
                "reverse not supported by this backend".into(),
            ))
        }
    }

    /// Connect to a device over TCP/IP (`adb connect <addr>` → `host:connect`).
    ///
    /// `addr` is the client-supplied target (`<host>` or `<host>:<port>`; a
    /// missing port defaults to 5555, the adbd-over-TCP default). On success the
    /// device joins [`Self::list_devices`] and the returned string is the
    /// AOSP-style status the client prints (e.g. `connected to 127.0.0.1:5555`
    /// or `already connected to …`).
    ///
    /// The default returns `unsupported`; set up a TCP registry and override to
    /// enable. adboost's [`DefaultDeviceBackend`] connects via
    /// [`crate::message_devices::tcp::ADBTcpDevice`].
    ///
    /// [`DefaultDeviceBackend`]: crate::server::DefaultDeviceBackend
    async fn connect(&self, _addr: &str) -> Result<String> {
        async move {
            Err(RustADBError::ADBRequestFailed(
                "connect not supported by this backend".into(),
            ))
        }
    }

    /// Disconnect a previously [`Self::connect`]ed TCP device (`adb disconnect`
    /// → `host:disconnect`). An empty `addr` disconnects every TCP device. The
    /// returned string is the AOSP-style status (e.g. `disconnected 127.0.0.1:5555`).
    async fn disconnect(&self, _addr: &str) -> Result<String> {
        async move {
            Err(RustADBError::ADBRequestFailed(
                "disconnect not supported by this backend".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_state_wire_strings_match_aosp() {
        assert_eq!(DeviceState::Device.as_wire(), "device");
        assert_eq!(DeviceState::Offline.as_wire(), "offline");
        assert_eq!(DeviceState::Unauthorized.as_wire(), "unauthorized");
    }

    #[test]
    fn device_entry_new_defaults_to_device_state_no_extras() {
        let e = DeviceEntry::new("ABC123");
        assert_eq!(e.serial, "ABC123");
        assert_eq!(e.state, DeviceState::Device);
        assert!(e.product.is_none() && e.model.is_none() && e.device.is_none());
        assert!(
            e.capabilities.is_none(),
            "a freshly-listed device has unknown (None) capabilities until handshaked"
        );
    }

    #[test]
    fn device_entry_with_capabilities_sets_some() {
        let caps = DeviceFeatureSet::default();
        let e = DeviceEntry::new("ABC123").with_capabilities(caps.clone());
        assert_eq!(e.capabilities, Some(caps));
    }

    #[test]
    fn backend_capabilities_default_is_all_false() {
        // The honest-banner default: a backend advertises nothing optional until
        // it explicitly opts in. This is what keeps `host:features` from
        // over-claiming (sync_v2 / shell_v2) when the bridge can't satisfy it.
        let caps = BackendCapabilities::default();
        assert!(!caps.sync, "sync must default to false (honest banner)");
        assert!(
            !caps.shell_v2,
            "shell_v2 must default to false (honest banner)"
        );
        assert!(
            !caps.reverse,
            "reverse must default to false (honest banner)"
        );
    }
}
