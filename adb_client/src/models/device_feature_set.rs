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

    /// Build the CNXN banner string `host::features=<csv>\0`.
    ///
    /// The trailing NUL terminator matches what a real `adb` server sends and is
    /// what the existing persistent connection wrote. With no features enabled
    /// (the honest default) this is `"host::features=\0"`.
    #[must_use]
    pub fn to_banner_string(&self) -> String {
        format!("host::features={}\0", self.feature_names().join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_banner_advertises_shell_v2_and_delayed_ack() {
        let banner = DeviceFeatureSet::default().to_banner_string();
        assert_eq!(
            banner, "host::features=shell_v2,delayed_ack\0",
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
            banner, "host::features=shell_v2,cmd,delayed_ack\0",
            "enabled features must be CSV-joined in declaration order with trailing NUL"
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
            "host::features=shell_v2\0",
            "single enabled feature must not produce a trailing comma"
        );
    }
}
