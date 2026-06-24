//! The "which Android / which adbd" axis of a [`SimulatedDevice`].
//!
//! A [`DeviceProfile`] carries exactly the three things the host's handshake
//! negotiates against: the CNXN reply **version** (`arg0`), the device
//! **banner** (its `features=` list, parsed by
//! [`DeviceFeatureSet::from_banner`]), and whether the device demands **AUTH**.
//! The presets below cover the real negotiation outcomes the protocol-state
//! research flagged as today untestable without specific hardware.
//!
//! [`SimulatedDevice`]: super::SimulatedDevice
//! [`DeviceFeatureSet::from_banner`]: crate::models::DeviceFeatureSet::from_banner

/// AOSP `A_VERSION_MIN` / legacy: classic stop-and-wait, no windowed flow
/// control. Mirrors the `A_VERSION_LEGACY` constant in `persistent.rs`.
pub(super) const A_VERSION_LEGACY: u32 = 0x0100_0000;

/// AOSP `A_VERSION_SKIP_CHECKSUM` (= `A_VERSION`): windowed `delayed_ack` flow
/// control AND `data_check` sent as `0`. Mirrors `A_VERSION_SKIP_CHECKSUM`.
pub(super) const A_VERSION_SKIP_CHECKSUM: u32 = 0x0100_0001;

/// The Android/adbd profile a [`SimulatedDevice`] presents in its CNXN handshake.
///
/// This is the device-version axis: it decides the version reported in the CNXN
/// reply (`arg0`), the banner string (hence the device's advertised
/// [`DeviceFeatureSet`](crate::models::DeviceFeatureSet)), and whether the
/// device requires AUTH before it will send its CNXN banner.
///
/// `#[non_exhaustive]` is intentional: tests build profiles through the named
/// constructors / presets, never by struct literal, so new axes (e.g. a custom
/// max-payload) can be added without churning call sites.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DeviceProfile {
    /// Protocol version the device reports in its CNXN reply header `arg0`. The
    /// host gates `delayed_ack` windowing on `>= A_VERSION_SKIP_CHECKSUM`.
    pub version: u32,
    /// The device's CNXN banner, e.g.
    /// `device::ro.product.name=sim;features=shell_v2,delayed_ack`. Parsed by the
    /// host into the peer feature set and scanned for `delayed_ack`.
    pub banner: String,
    /// Whether the device demands AUTH (replies `AUTH(TOKEN)` to the first CNXN)
    /// before sending its banner. `false` → an already-authorized device that
    /// answers CNXN immediately.
    pub requires_auth: bool,
    /// When `requires_auth`, whether the device accepts the host's first
    /// `AUTH(SIGNATURE)` (already-known key → CNXN) or rejects it once, forcing
    /// the host down the `AUTH(RSAPUBLICKEY)` path before accepting.
    pub accepts_first_signature: bool,
}

impl DeviceProfile {
    /// Build a banner of the form `device::<props>;features=<csv>`.
    fn banner_with_features(features: &str) -> String {
        format!("device::ro.product.name=sim;features={features}")
    }

    /// Android 11-era device: legacy version, banner WITHOUT `delayed_ack`, no
    /// AUTH. The host's negotiation must land on **classic** stop-and-wait flow
    /// control (windowing disabled) — the case that today needs a real old
    /// device to exercise.
    #[must_use]
    pub fn android_11() -> Self {
        Self {
            version: A_VERSION_LEGACY,
            banner: Self::banner_with_features("shell_v2"),
            requires_auth: false,
            accepts_first_signature: true,
        }
    }

    /// Android 16-era device: skip-checksum version, banner WITH `delayed_ack`,
    /// no AUTH. The host must negotiate **windowed** flow control. This is the
    /// device class the `delayed_ack` saga (bugs #1/#2/#3) escaped on.
    #[must_use]
    pub fn android_16() -> Self {
        Self {
            version: A_VERSION_SKIP_CHECKSUM,
            banner: Self::banner_with_features("shell_v2,cmd,delayed_ack"),
            requires_auth: false,
            accepts_first_signature: true,
        }
    }

    /// A device that demands AUTH and accepts the host's signature on the first
    /// try (the host's key is already known to the device) → the
    /// TOKEN → SIGNATURE → CNXN path.
    #[must_use]
    pub fn auth_known_key() -> Self {
        Self {
            version: A_VERSION_SKIP_CHECKSUM,
            banner: Self::banner_with_features("shell_v2,delayed_ack"),
            requires_auth: true,
            accepts_first_signature: true,
        }
    }

    /// A device that demands AUTH and rejects the first signature (unknown key),
    /// forcing the TOKEN → SIGNATURE → (reject) → RSAPUBLICKEY → CNXN path.
    #[must_use]
    pub fn auth_new_key() -> Self {
        Self {
            version: A_VERSION_SKIP_CHECKSUM,
            banner: Self::banner_with_features("shell_v2,delayed_ack"),
            requires_auth: true,
            accepts_first_signature: false,
        }
    }

    /// A feature-less device: empty `features=` segment. The host must parse the
    /// all-`false` peer feature set (so the server never over-advertises
    /// `shell_v2` to it — bug B-feat, fully exercised on the server path in
    /// Phase C). Skip-checksum version, no AUTH.
    #[must_use]
    pub fn featureless() -> Self {
        Self {
            version: A_VERSION_SKIP_CHECKSUM,
            banner: Self::banner_with_features(""),
            requires_auth: false,
            accepts_first_signature: true,
        }
    }
}
