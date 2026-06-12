use std::net::SocketAddrV4;

use clap::{Parser, Subcommand};

use crate::utils;

use super::{
    EmulatorCommand, HostCommand, LocalCommand, PersistentCommand, SelftestCommand, ServerCommand,
    TcpCommand, UsbCommand,
};

#[derive(Debug, Parser)]
#[clap(about, long_version = utils::long_version(), author)]
pub struct Opts {
    #[clap(long = "debug")]
    pub debug: bool,
    #[clap(subcommand)]
    pub command: MainCommand,
}

#[derive(Debug, Parser)]
pub enum MainCommand {
    /// Proxy commands sent to an external adb server daemon
    Host(ProxyCommand<HostCommand>),
    /// Device commands routed through an external adb server daemon
    Local(ProxyCommand<LocalCommand>),
    /// Run adboost's own USB-backed ADB server (start / kill)
    Server {
        #[clap(subcommand)]
        command: ServerCommand,
    },
    /// Emulator related commands
    Emu(EmulatorCommand),
    /// USB device related commands
    Usb(UsbCommand),
    /// TCP device related commands
    Tcp(TcpCommand),
    /// Persistent-USB exerciser: one-command reproducer for the async USB /
    /// windowed `delayed_ack` path (prints a negotiation self-check + runs a shell command)
    Persistent(PersistentCommand),
    /// MDNS discovery related commands
    Mdns,
    /// Run the interactive, device-backed self-test suite
    Selftest(SelftestCommand),
    /// Display various version information
    Version,
}

/// Address + device-selector wrapper for commands that proxy through an
/// **external** adb server daemon (`host` / `local`).
#[derive(Debug, Parser)]
pub struct ProxyCommand<T: Subcommand> {
    #[clap(short = 'a', long = "address", default_value = "127.0.0.1:5037")]
    pub address: SocketAddrV4,
    /// Serial id of a specific device. Every request will be sent to this device.
    #[clap(short = 's', long = "serial")]
    pub serial: Option<String>,
    /// Transport id of a specific device (as shown by `adb devices -l`). Use this to
    /// disambiguate devices that share the same serial number. Transport ids are
    /// reassigned on device reconnect or adb-server restart and should not be cached.
    #[clap(short = 't', long = "transport-id", conflicts_with = "serial")]
    pub transport_id: Option<u32>,
    #[clap(subcommand)]
    pub command: T,
}
