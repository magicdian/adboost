use tracing_subscriber::{EnvFilter, fmt};

/// Installs a stderr `tracing` subscriber.
///
/// The CLI is the binary, so it owns subscriber installation (the `adboost`
/// library stays a pure emitter). Filtering follows the standard convention:
/// - if `RUST_LOG` is set, use it (full `EnvFilter` directive syntax, including
///   per-span / per-`local_id` filters such as `[session{local_id=42}]=trace`);
/// - else fall back to the `--debug` CLI flag (`debug` vs `info`).
///
/// Uses `try_init`, so it never panics if a subscriber is already installed.
pub fn setup_logger(debug: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if debug { "debug" } else { "info" }));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

pub const fn long_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
