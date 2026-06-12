use clap::Parser;

/// Run the interactive, device-backed self-test suite.
///
/// Exercises shell / push / pull / forward over two channels — USB-direct and
/// through adboost's own in-process ADB server (on an ephemeral port, so it
/// never disturbs a real `:5037`). Requires at least one connected, authorized
/// device. Results are printed in a gtest-style format and the exit code
/// reflects success.
#[derive(Parser, Debug)]
pub struct SelftestCommand {
    /// Run only the automated phase; skip the interactive (re-plug / reboot)
    /// cases. Useful for unattended / CI runs.
    #[clap(long = "no-interactive")]
    pub no_interactive: bool,
}
