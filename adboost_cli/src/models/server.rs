use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

/// Manage adboost's **own** USB-backed ADB server (distinct from `host kill`,
/// which tells an *external* adb daemon to quit).
#[derive(Parser, Debug)]
pub enum ServerCommand {
    /// Start the adboost ADB server (background daemon by default).
    Start {
        /// Address to listen on for adb/scrcpy clients.
        #[clap(short = 'a', long = "address", default_value = "127.0.0.1:5037")]
        address: SocketAddr,
        /// Run in the foreground (do not daemonize); blocks until interrupted.
        #[clap(long = "foreground")]
        foreground: bool,
        /// Path to the PID file (default: per-user runtime/home location).
        #[clap(long = "pid-file")]
        pid_file: Option<PathBuf>,
        /// Path to the daemon log file (default: next to the PID file).
        #[clap(long = "log-file")]
        log_file: Option<PathBuf>,
    },
    /// Stop a running adboost ADB server (via its PID file).
    Kill {
        /// Path to the PID file (default: per-user runtime/home location).
        #[clap(long = "pid-file")]
        pid_file: Option<PathBuf>,
    },
}
