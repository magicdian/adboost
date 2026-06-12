#![crate_type = "lib"]
#![forbid(unsafe_code)]
#![allow(missing_debug_implementations)]
#![allow(missing_docs)]
#![allow(clippy::missing_errors_doc)]
#![doc = include_str!("../README.md")]
// Feature `doc_cfg` is currently only available on nightly builds.
// It is activated when cfg `docsrs` is enabled.
// Documentation can be build locally using:
// `RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --no-deps --all-features`
#![cfg_attr(docsrs, feature(doc_cfg))]

mod adb_device_ext;
mod adb_transport;
/// Emulator-related definitions
pub mod emulator;
mod error;
mod message_devices;
mod models;

/// Proxy client: connects to and proxies commands through an **external** ADB
/// server daemon (the classic `adb` server on `:5037`). This is a client, not a
/// server — for adboost's own ADB server frontend, see a future `server` module.
pub mod proxy;
mod utils;

/// MDNS-related definitions
#[cfg(feature = "mdns")]
#[cfg_attr(docsrs, doc(cfg(feature = "mdns")))]
pub mod mdns;

/// Install a stderr `tracing` subscriber configured from `RUST_LOG`
/// (via [`tracing_subscriber::EnvFilter`]).
///
/// This is a **convenience for binaries** (e.g. `adboost_cli`) and quick
/// downstream bring-up — it is feature-gated (`tracing-init`) and OFF by
/// default. It uses `try_init`, so it is a no-op when a subscriber is already
/// installed: the library itself never installs one, staying a pure emitter.
///
/// # Activating logs (no rebuild)
///
/// With a subscriber installed (this helper, or any `tracing-subscriber` set up
/// by the consumer), `RUST_LOG` selects output at runtime:
///
/// - `RUST_LOG=adb_client=trace` — the whole crate.
/// - `RUST_LOG=adb_client::message_devices::usb::persistent=trace` — just the
///   USB multiplexer.
/// - `RUST_LOG=[reader]=trace` / `[writer]=trace` — just the reader / writer task.
/// - `RUST_LOG=[session{local_id=...}]=trace` — only events for one session
///   (per-`local_id` attribution).
/// - `RUST_LOG=adb_client=info,[session]=debug` — combine.
///
/// Because `tracing` is built with its `log` feature here, every event is also
/// emitted as a `log` record, so consumers wired only with `env_logger`
/// (e.g. the current CLI) still see output.
#[cfg(feature = "tracing-init")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing-init")))]
pub fn init_tracing_from_env() {
    use tracing_subscriber::{EnvFilter, fmt};
    let _ = fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
}

pub use adb_device_ext::ADBDeviceExt;
use adb_transport::ADBTransport;
pub use error::{Result, RustADBError};
pub use message_devices::*;
pub use models::{
    ADBListItem, ADBListItemType, ADBLocalCommand, ADBStatExtendedResponse, ADBStatMapping,
    AdbStatResponse, DeviceFeatureSet, HostFeatures, RebootType, RemountInfo,
};
