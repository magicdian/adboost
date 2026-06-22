//! Configurable ADB feature set advertised in the CNXN banner.

/// ADB `shell_v2` feature string — shell protocol v2 (separate stdout/stderr,
/// exit code framing).
pub const FEATURE_SHELL_V2: &str = "shell_v2";
/// ADB `cmd` feature string — the `cmd` / `abb` command runner.
pub const FEATURE_CMD: &str = "cmd";
/// ADB `stat_v2` feature string — extended `stat` (`STA2`) over the sync service.
pub const FEATURE_STAT_V2: &str = "stat_v2";
/// ADB `ls_v2` feature string — extended `ls` (`LIS2`) over the sync service.
pub const FEATURE_LS_V2: &str = "ls_v2";
/// ADB `delayed_ack` feature string — windowed flow control (decoupled WRTE/OKAY).
pub const FEATURE_DELAYED_ACK: &str = "delayed_ack";
/// ADB `sendrecv_v2` feature string — sync send/recv protocol v2.
pub const FEATURE_SENDRECV_V2: &str = "sendrecv_v2";
/// ADB `sendrecv_v2_brotli` feature string — sync v2 with brotli compression.
pub const FEATURE_SENDRECV_V2_BROTLI: &str = "sendrecv_v2_brotli";
/// ADB `sendrecv_v2_lz4` feature string — sync v2 with lz4 compression.
pub const FEATURE_SENDRECV_V2_LZ4: &str = "sendrecv_v2_lz4";
/// ADB `sendrecv_v2_zstd` feature string — sync v2 with zstd compression.
pub const FEATURE_SENDRECV_V2_ZSTD: &str = "sendrecv_v2_zstd";
/// ADB `sendrecv_v2_dry_run_send` feature string — sync v2 dry-run send.
pub const FEATURE_SENDRECV_V2_DRY_RUN_SEND: &str = "sendrecv_v2_dry_run_send";

/// The set of ADB protocol features this end advertises in its CNXN banner.
///
/// Each boolean gates one feature string in the `host::features=<csv>` banner.
/// The banner is a **contract**: when this fork acts as an ADB *server*, any
/// client that connects will act on whatever is advertised here. Therefore the
/// [`Default`] impl advertises **only features the fork actually implements
/// today** — see the per-PR notes on each field.
// Each bool maps to one independent ADB feature string on the wire; this is a
// flat set of orthogonal flags, not a state machine, so the `excessive_bools`
// refactor suggestions (state machine / two-variant enums) do not apply.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeviceFeatureSet {
    /// `shell_v2`: separate stdout/stderr + exit code framing.
    ///
    /// Off by default — the persistent connection does not yet decode the
    /// shell-v2 inner frames (Ask #5, a later PR).
    pub shell_v2: bool,
    /// `cmd`: the `cmd` / `abb` command runner. Off by default — not implemented.
    pub cmd: bool,
    /// `stat_v2`: extended `stat` (`STA2`). Off by default — sync v2 not implemented.
    pub stat_v2: bool,
    /// `ls_v2`: extended `ls` (`LIS2`). Off by default — sync v2 not implemented.
    pub ls_v2: bool,
    /// `delayed_ack`: windowed flow control.
    ///
    /// On by default — the persistent connection implements real windowed flow
    /// control (Ask #1): a 32 MiB per-stream send window, signed i32-LE OKAY
    /// payload deltas, and eager receive-side acks. The window is only used when
    /// the device's banner also advertises `delayed_ack` (intersection);
    /// otherwise the session falls back to classic stop-and-wait.
    pub delayed_ack: bool,
    /// `sendrecv_v2`: sync send/recv protocol v2. Off by default — not implemented.
    pub sendrecv_v2: bool,
    /// `sendrecv_v2_brotli`: sync v2 + brotli. Off by default — no compression deps.
    pub sendrecv_v2_brotli: bool,
    /// `sendrecv_v2_lz4`: sync v2 + lz4. Off by default — no compression deps.
    pub sendrecv_v2_lz4: bool,
    /// `sendrecv_v2_zstd`: sync v2 + zstd. Off by default — no compression deps.
    pub sendrecv_v2_zstd: bool,
    /// `sendrecv_v2_dry_run_send`: sync v2 dry-run send. Off by default — not implemented.
    pub sendrecv_v2_dry_run_send: bool,
}

impl Default for DeviceFeatureSet {
    /// The honest default for the fork.
    ///
    /// Advertises only features the persistent connection actually honors today:
    /// `shell_v2` (Ask #5, inner-frame decoding + exit code — landed) and
    /// `delayed_ack` (Ask #1, windowed flow control — landed). Still-pending
    /// features stay off and are flipped on as they land (sync v2 family).
    fn default() -> Self {
        Self {
            shell_v2: true,
            cmd: false,
            stat_v2: false,
            ls_v2: false,
            delayed_ack: true,
            sendrecv_v2: false,
            sendrecv_v2_brotli: false,
            sendrecv_v2_lz4: false,
            sendrecv_v2_zstd: false,
            sendrecv_v2_dry_run_send: false,
        }
    }
}

impl DeviceFeatureSet {
    /// Collect the advertised feature strings, in stable order.
    fn feature_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.shell_v2 {
            names.push(FEATURE_SHELL_V2);
        }
        if self.cmd {
            names.push(FEATURE_CMD);
        }
        if self.stat_v2 {
            names.push(FEATURE_STAT_V2);
        }
        if self.ls_v2 {
            names.push(FEATURE_LS_V2);
        }
        if self.delayed_ack {
            names.push(FEATURE_DELAYED_ACK);
        }
        if self.sendrecv_v2 {
            names.push(FEATURE_SENDRECV_V2);
        }
        if self.sendrecv_v2_brotli {
            names.push(FEATURE_SENDRECV_V2_BROTLI);
        }
        if self.sendrecv_v2_lz4 {
            names.push(FEATURE_SENDRECV_V2_LZ4);
        }
        if self.sendrecv_v2_zstd {
            names.push(FEATURE_SENDRECV_V2_ZSTD);
        }
        if self.sendrecv_v2_dry_run_send {
            names.push(FEATURE_SENDRECV_V2_DRY_RUN_SEND);
        }
        names
    }

    /// Build the CNXN banner string `host::features=<csv>`.
    ///
    /// NO trailing NUL: the real AOSP `adb` host (`send_connect`) puts no NUL in
    /// the CNXN payload, and adbd's feature parser (`StringToFeatureSet` →
    /// `Split(",")`) does not trim, so a trailing NUL would corrupt the LAST CSV
    /// feature token (e.g. `delayed_ack\0` != `delayed_ack`) and make adbd's
    /// `SupportsDelayedAck()` false — which then makes it reject our windowed
    /// `OPEN(arg1=32MiB)` with `A_CLSE`. With no features enabled this is
    /// `"host::features="`.
    #[must_use]
    pub fn to_banner_string(&self) -> String {
        format!("host::features={}", self.feature_names().join(","))
    }

    /// Parse the **peer's** advertised feature set out of a CNXN banner.
    ///
    /// This is the inverse of [`to_banner_string`](Self::to_banner_string): given a
    /// device banner such as
    /// `device::ro.product.name=...;features=shell_v2,cmd,delayed_ack`, it returns
    /// the [`DeviceFeatureSet`] the *device* advertised. It is the truth source for
    /// per-device capability negotiation (so the server never offers `shell_v2` /
    /// `sync_v2` to a device whose banner lacks it).
    ///
    /// Parsing mirrors adbd's own `StringToFeatureSet` (`Split(",")`, no trim) and
    /// the existing `delayed_ack` scan: the banner is split on `;`/`\0` segment
    /// separators, the `features=` segment's value is split on commas, and each
    /// known token flips its flag. Unknown tokens are ignored (forward-compatible).
    /// An absent or empty `features=` segment yields the all-`false` set — exactly
    /// the conservative result wanted for a stripped adbd (or a TLS-upgraded link
    /// whose post-STLS banner we report as empty).
    ///
    /// Note this is the device's *raw advertised* set, NOT what is negotiated: a
    /// feature is only usable when BOTH ends agree (see the `delayed_ack`
    /// intersection in the persistent connection).
    #[must_use]
    pub fn from_banner(banner: &str) -> Self {
        let mut set = Self {
            shell_v2: false,
            cmd: false,
            stat_v2: false,
            ls_v2: false,
            delayed_ack: false,
            sendrecv_v2: false,
            sendrecv_v2_brotli: false,
            sendrecv_v2_lz4: false,
            sendrecv_v2_zstd: false,
            sendrecv_v2_dry_run_send: false,
        };
        // The banner may be NUL-terminated and is `<type>::` followed by a list of
        // `;`-separated key=value props. The `features=` prop can be the first one
        // (so it appears as `<type>::features=...`, e.g. our own
        // `host::features=...`) or a later one (`...;features=...`). Drop any
        // `<type>::` prefix on each segment (`rsplit("::").next()`) before matching
        // `features=`, so both placements parse. Fold the CSV tokens into the set.
        for token in banner
            .split([';', '\0'])
            .filter_map(|seg| {
                seg.rsplit("::")
                    .next()
                    .and_then(|prop| prop.strip_prefix("features="))
            })
            .flat_map(|features| features.split(','))
        {
            match token {
                FEATURE_SHELL_V2 => set.shell_v2 = true,
                FEATURE_CMD => set.cmd = true,
                FEATURE_STAT_V2 => set.stat_v2 = true,
                FEATURE_LS_V2 => set.ls_v2 = true,
                FEATURE_DELAYED_ACK => set.delayed_ack = true,
                FEATURE_SENDRECV_V2 => set.sendrecv_v2 = true,
                FEATURE_SENDRECV_V2_BROTLI => set.sendrecv_v2_brotli = true,
                FEATURE_SENDRECV_V2_LZ4 => set.sendrecv_v2_lz4 = true,
                FEATURE_SENDRECV_V2_ZSTD => set.sendrecv_v2_zstd = true,
                FEATURE_SENDRECV_V2_DRY_RUN_SEND => set.sendrecv_v2_dry_run_send = true,
                _ => {} // unknown / empty token — ignore (forward-compatible)
            }
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_banner_advertises_shell_v2_and_delayed_ack() {
        let banner = DeviceFeatureSet::default().to_banner_string();
        assert_eq!(
            banner, "host::features=shell_v2,delayed_ack",
            "the honest default advertises shell_v2 (Ask #5) and delayed_ack (Ask #1), in declaration order"
        );
    }

    #[test]
    fn custom_banner_lists_enabled_features_in_order() {
        let features = DeviceFeatureSet {
            shell_v2: true,
            cmd: true,
            delayed_ack: true,
            ..DeviceFeatureSet::default()
        };
        let banner = features.to_banner_string();
        assert_eq!(
            banner, "host::features=shell_v2,cmd,delayed_ack",
            "enabled features must be CSV-joined in declaration order with no trailing NUL"
        );
    }

    #[test]
    fn single_feature_banner_has_no_trailing_comma() {
        let features = DeviceFeatureSet {
            shell_v2: true,
            // Explicitly disable the (now default-on) delayed_ack so this case is
            // genuinely a single advertised feature.
            delayed_ack: false,
            ..DeviceFeatureSet::default()
        };
        assert_eq!(
            features.to_banner_string(),
            "host::features=shell_v2",
            "single enabled feature must not produce a trailing comma"
        );
    }

    #[test]
    fn banner_has_no_trailing_nul_so_last_feature_is_clean() {
        // Bug #3 root-cause lock: adbd's `StringToFeatureSet` does
        // `Split(value, ",")` with NO trimming, so a trailing NUL in the CNXN
        // banner payload corrupts the LAST CSV feature token (it becomes
        // `delayed_ack\0` != `delayed_ack`), making adbd's `SupportsDelayedAck()`
        // false and causing it to reject our windowed `OPEN(arg1=32MiB)` with
        // `A_CLSE`. The banner must therefore not end in NUL and the last feature
        // token must be exactly `delayed_ack` (no embedded NUL).
        let banner = DeviceFeatureSet::default().to_banner_string();
        assert!(
            !banner.ends_with('\0'),
            "CNXN banner must not end with a NUL (would corrupt the last CSV feature token in adbd's no-trim Split)"
        );
        let value = banner
            .strip_prefix("host::features=")
            .expect("banner must start with host::features=");
        let last_token = value
            .split(',')
            .next_back()
            .expect("default banner has at least one feature");
        assert_eq!(
            last_token, FEATURE_DELAYED_ACK,
            "the last comma-separated feature token must be exactly delayed_ack with no embedded NUL"
        );
    }

    #[test]
    fn from_banner_parses_a_full_device_banner() {
        // A realistic full-feature device banner (the `device::` prefix +
        // product metadata segments before `features=`).
        let banner = "device::ro.product.name=sdk;ro.product.model=X;\
                      features=shell_v2,cmd,stat_v2,ls_v2,delayed_ack";
        let set = DeviceFeatureSet::from_banner(banner);
        assert!(set.shell_v2 && set.cmd && set.stat_v2 && set.ls_v2 && set.delayed_ack);
        // Tokens not present must stay false.
        assert!(!set.sendrecv_v2 && !set.sendrecv_v2_zstd);
    }

    #[test]
    fn from_banner_empty_features_segment_is_all_false() {
        // The stripped-adbd case from the bug report: a banner whose features
        // segment is empty must parse to no optional capabilities, so the server
        // never offers shell_v2 / sync_v2 to it.
        let banner = "device::ro.product.name=;ro.product.model=;features=";
        let set = DeviceFeatureSet::from_banner(banner);
        assert_eq!(
            set,
            DeviceFeatureSet {
                shell_v2: false,
                cmd: false,
                stat_v2: false,
                ls_v2: false,
                delayed_ack: false,
                sendrecv_v2: false,
                sendrecv_v2_brotli: false,
                sendrecv_v2_lz4: false,
                sendrecv_v2_zstd: false,
                sendrecv_v2_dry_run_send: false,
            },
            "an empty features= segment must yield the all-false set"
        );
    }

    #[test]
    fn from_banner_with_no_features_segment_is_all_false() {
        // A banner that carries no `features=` segment at all (older adbd) is
        // treated as advertising nothing optional.
        let set = DeviceFeatureSet::from_banner("device::ro.product.name=x");
        assert!(!set.shell_v2 && !set.delayed_ack && !set.cmd);
    }

    #[test]
    fn from_banner_ignores_nul_termination_and_unknown_tokens() {
        // NUL-terminated payload (some transports include it) + an unknown future
        // token must not break parsing: known tokens still register, the unknown
        // is ignored, and the trailing NUL does not corrupt the last token.
        let banner = "device::features=shell_v2,future_feature,delayed_ack\0";
        let set = DeviceFeatureSet::from_banner(banner);
        assert!(set.shell_v2, "shell_v2 before the unknown token registers");
        assert!(
            set.delayed_ack,
            "delayed_ack must register even though a NUL follows it (split on \\0 first)"
        );
        assert!(!set.cmd, "an unknown token must not flip any known flag");
    }

    #[test]
    fn from_banner_round_trips_with_to_banner_string() {
        // Parsing our own emitted banner must recover the same set.
        let original = DeviceFeatureSet {
            shell_v2: true,
            cmd: true,
            stat_v2: true,
            delayed_ack: true,
            ..DeviceFeatureSet::default()
        };
        let reparsed = DeviceFeatureSet::from_banner(&original.to_banner_string());
        assert_eq!(
            reparsed, original,
            "from_banner ∘ to_banner_string == identity"
        );
    }
}
