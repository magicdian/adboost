//! What this server tells *clients* it can do.
//!
//! [`ServerCapabilities`] configures the **client-facing** side of the host
//! protocol: the `host:features` set, the `host:version` number, and the
//! `host:kill` takeover policy. It is the mirror of [`DeviceFeatureSet`], which
//! is the **device-facing** CNXN banner — do not confuse the two:
//!
//! - [`DeviceFeatureSet`] → what adboost (as a *client*) advertises to a device.
//! - [`ServerCapabilities`] → what adboost (as a *server*) advertises to `adb` /
//!   `scrcpy` clients.
//!
//! # The honest-minimal default (and why over-claiming hangs clients)
//!
//! The default advertises only `cmd,stat_v2,fixed_push_mkdir,apex` — explicitly
//! **not** `shell_v2` or the `sync_v2` family. Advertising `shell_v2` makes a
//! client switch `adb shell` to v2 inner framing (a 1-byte stream-id + 4-byte LE
//! length per WRTE), which the bare `shell:` v1 bridge does not produce → the
//! client desyncs. Advertising `sync_v2` makes `push`/`pull` negotiate chunk
//! codecs the bridge does not implement. So the default forces clients onto the
//! always-safe v1 paths; widen it (e.g. [`Self::with_shell_v2`]) only in lockstep
//! with a backend that genuinely implements the richer protocol.
//!
//! [`DeviceFeatureSet`]: crate::DeviceFeatureSet

use super::backend::BackendCapabilities;

/// Honest-minimal client-facing features. Every one is safe to advertise with
/// the v1 `shell:`/`tcp:` bridge — none changes the client's wire framing in a
/// way the bridge cannot satisfy.
const DEFAULT_FEATURES: &[&str] = &["cmd", "stat_v2", "fixed_push_mkdir", "apex"];

/// The default `host:version` 4-hex value (AOSP server protocol version `0x29`).
const DEFAULT_VERSION_HEX: &str = "0029";

/// How this server answers a client's `host:kill` (request to shut the server
/// down). adboost holds `:5037` for its own lifetime by default, so the default
/// is [`Reject`][KillPolicy::Reject].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillPolicy {
    /// Reply `FAIL` and keep running — the server owns `:5037` for its lifetime.
    Reject,
    /// Accept: reply `OKAY` and shut the server down (native-adb-like takeover).
    Shutdown,
}

/// The capabilities this server advertises to clients, plus its `host:kill`
/// policy. Build with [`Self::default`] then opt into extras with the `with_*`
/// methods.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCapabilities {
    features: Vec<String>,
    version_hex: String,
    kill_policy: KillPolicy,
}

impl Default for ServerCapabilities {
    fn default() -> Self {
        Self {
            features: DEFAULT_FEATURES.iter().map(|s| (*s).to_string()).collect(),
            version_hex: DEFAULT_VERSION_HEX.to_string(),
            kill_policy: KillPolicy::Reject,
        }
    }
}

impl ServerCapabilities {
    /// The `host:features` reply value: the advertised features, comma-joined.
    #[must_use]
    pub fn features_csv(&self) -> String {
        self.features.join(",")
    }

    /// The `host:version` reply value (4-hex, e.g. `"0029"`).
    #[must_use]
    pub fn version_hex(&self) -> &str {
        &self.version_hex
    }

    /// Whether `feature` is currently advertised. The frontend uses this to gate
    /// optional local services (e.g. only bridge `sync:` when `sync_v2` is
    /// advertised) so it never accepts a service it did not promise.
    #[must_use]
    pub fn has_feature(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    /// The configured `host:kill` policy.
    #[must_use]
    pub fn kill_policy(&self) -> KillPolicy {
        self.kill_policy
    }

    /// Advertise `shell_v2` in addition to the defaults.
    ///
    /// Only call this when the backend actually bridges shell-v2 inner framing —
    /// advertising it with the bare `shell:` v1 bridge desyncs clients. Adding a
    /// feature already present is a no-op (no duplicate token).
    #[must_use]
    pub fn with_shell_v2(self) -> Self {
        self.with_feature("shell_v2")
    }

    /// Advertise an arbitrary additional feature string. Idempotent: a feature
    /// already advertised is not duplicated.
    #[must_use]
    pub fn with_feature(mut self, feature: impl Into<String>) -> Self {
        let feature = feature.into();
        if !self.features.contains(&feature) {
            self.features.push(feature);
        }
        self
    }

    /// Set the `host:version` 4-hex value.
    #[must_use]
    pub fn with_version_hex(mut self, version_hex: impl Into<String>) -> Self {
        self.version_hex = version_hex.into();
        self
    }

    /// Set the `host:kill` takeover policy (default [`KillPolicy::Reject`]).
    #[must_use]
    pub fn with_kill_policy(mut self, policy: KillPolicy) -> Self {
        self.kill_policy = policy;
        self
    }

    /// Return a copy whose advertised features are widened to include the
    /// optional features the backend genuinely implements.
    ///
    /// This is the honest-banner negotiation: the frontend calls it once at
    /// serve time with the backend's [`BackendCapabilities`]. A feature is added
    /// only when the backend reports it (`sync` → `sync_v2`,
    /// `shell_v2` → `shell_v2`); features already present are not duplicated.
    /// Features are never *removed* — an operator who explicitly opted into a
    /// feature via the builder keeps it (their responsibility), but the default
    /// path never over-claims because the defaults exclude these.
    #[must_use]
    pub fn negotiated_with(mut self, backend: BackendCapabilities) -> Self {
        if backend.sync {
            self = self.with_feature("sync_v2");
        }
        if backend.shell_v2 {
            self = self.with_feature("shell_v2");
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_honest_minimal_and_excludes_shell_v2() {
        let caps = ServerCapabilities::default();
        assert_eq!(caps.features_csv(), "cmd,stat_v2,fixed_push_mkdir,apex");
        assert!(
            !caps.features_csv().contains("shell_v2"),
            "default must NOT advertise shell_v2 (would desync the v1 shell bridge)"
        );
        assert!(
            !caps.features_csv().contains("sync_v2"),
            "default must NOT advertise the sync_v2 family"
        );
        assert_eq!(caps.version_hex(), "0029");
        assert_eq!(caps.kill_policy(), KillPolicy::Reject);
    }

    #[test]
    fn with_shell_v2_appends_once() {
        let caps = ServerCapabilities::default()
            .with_shell_v2()
            .with_shell_v2();
        let csv = caps.features_csv();
        assert!(csv.ends_with(",shell_v2"));
        assert_eq!(
            csv.matches("shell_v2").count(),
            1,
            "with_shell_v2 must be idempotent (no duplicate token)"
        );
    }

    #[test]
    fn negotiated_with_adds_only_backend_supported_features() {
        // Backend supports both → both appended exactly once, on top of defaults.
        let caps = ServerCapabilities::default().negotiated_with(BackendCapabilities {
            sync: true,
            shell_v2: true,
            ..Default::default()
        });
        let csv = caps.features_csv();
        assert!(
            csv.contains("sync_v2"),
            "sync backend must advertise sync_v2"
        );
        assert!(
            csv.contains("shell_v2"),
            "shell_v2 backend must advertise shell_v2"
        );
        assert_eq!(csv.matches("sync_v2").count(), 1, "no duplicate sync_v2");
        assert_eq!(csv.matches("shell_v2").count(), 1, "no duplicate shell_v2");
    }

    #[test]
    fn negotiated_with_default_backend_stays_honest_minimal() {
        // A backend that implements nothing optional must NOT widen the banner.
        let caps = ServerCapabilities::default().negotiated_with(BackendCapabilities::default());
        assert_eq!(
            caps.features_csv(),
            "cmd,stat_v2,fixed_push_mkdir,apex",
            "all-false backend caps must leave the honest-minimal default untouched"
        );
    }

    #[test]
    fn negotiated_with_only_sync_does_not_add_shell_v2() {
        let caps = ServerCapabilities::default().negotiated_with(BackendCapabilities {
            sync: true,
            shell_v2: false,
            ..Default::default()
        });
        let csv = caps.features_csv();
        assert!(csv.contains("sync_v2"), "sync backend advertises sync_v2");
        assert!(
            !csv.contains("shell_v2"),
            "a sync-only backend must NOT advertise shell_v2"
        );
    }

    #[test]
    fn builders_set_version_and_kill_policy() {
        let caps = ServerCapabilities::default()
            .with_version_hex("0041")
            .with_kill_policy(KillPolicy::Shutdown);
        assert_eq!(caps.version_hex(), "0041");
        assert_eq!(caps.kill_policy(), KillPolicy::Shutdown);
    }
}
