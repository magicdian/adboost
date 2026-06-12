use std::num::ParseIntError;
use std::path::PathBuf;

use clap::Parser;

const fn parse_hex_id(id: &str) -> Result<u16, ParseIntError> {
    u16::from_str_radix(id, 16)
}

/// Persistent-USB exerciser: a one-command, in-tree reproducer for the async
/// USB / windowed-`delayed_ack` path (where bugs #1/#2/#3 lived).
///
/// Builds a [`adb_client::usb::PersistentUsbConnection`], prints a negotiation
/// self-check (advertised feature set, banner, first inbound frame after OPEN —
/// OKAY vs CLSE), then runs a shell command and prints its output. Read-only /
/// non-invasive against the device.
#[derive(Parser, Debug)]
pub struct PersistentCommand {
    /// Hexadecimal vendor id of this USB device (omit both vid/pid to autodetect)
    #[clap(short = 'v', long = "vendor-id", value_parser = parse_hex_id, value_name = "VID")]
    pub vendor_id: Option<u16>,
    /// Hexadecimal product id of this USB device (omit both vid/pid to autodetect)
    #[clap(short = 'p', long = "product-id", value_parser = parse_hex_id, value_name = "PID")]
    pub product_id: Option<u16>,
    /// Path to a custom private key to use for authentication
    #[clap(short = 'k', long = "private-key")]
    pub path_to_private_key: Option<PathBuf>,
    /// Advertise the classic (non-windowed) path: `DeviceFeatureSet { delayed_ack: false, .. }`.
    /// This is the exact bug-#3 control experiment in one flag.
    #[clap(long = "no-delayed-ack")]
    pub no_delayed_ack: bool,
    /// Shell command to run over the persistent connection.
    #[clap(subcommand)]
    pub command: Option<PersistentSubcommand>,
}

#[derive(Parser, Debug)]
pub enum PersistentSubcommand {
    /// Run a shell command over the persistent connection and print its output.
    Shell {
        /// The command to run (defaults to `getprop` if omitted).
        #[arg(trailing_var_arg = true)]
        commands: Vec<String>,
    },
}
