use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Default number of bytes between two [`InstallProgress`] samples (1 MiB).
pub const DEFAULT_INSTALL_PROGRESS_INTERVAL_BYTES: u64 = 1024 * 1024;

/// A snapshot of a streamed install's upload progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    /// APK bytes already written to the device connection.
    pub bytes_sent: u64,
    /// Total APK size, known before the transfer starts.
    pub total_bytes: u64,
}

/// Observation and cancellation control surface for an install.
///
/// Both members are optional; [`InstallControl::default`] transfers without
/// emitting progress and without a cancellation path.
///
/// # Progress is an observation channel
///
/// Samples are emitted roughly every
/// [`progress_interval_bytes`](Self::progress_interval_bytes) (and once more for
/// the final byte). Sending is non-blocking: when the channel is full a sample
/// is dropped rather than stalling the transfer. Never use backpressure on this
/// channel to pace an install.
///
/// # Cancellation is best-effort
///
/// Triggering the token stops the local transfer and closes the connection, so
/// the device-side `cmd package install` observes EOF and abandons the session.
/// The device may nevertheless have finished installing before the abort lands;
/// callers that need certainty should query the package state afterwards.
#[derive(Debug)]
pub struct InstallControl {
    /// Progress samples. Dropped when the channel is full.
    pub progress: Option<mpsc::Sender<InstallProgress>>,
    /// Cancellation token; see the type-level note on best-effort semantics.
    pub cancel: Option<CancellationToken>,
    /// Byte interval between progress samples. `0` is treated as every chunk.
    pub progress_interval_bytes: u64,
}

impl Default for InstallControl {
    fn default() -> Self {
        Self {
            progress: None,
            cancel: None,
            progress_interval_bytes: DEFAULT_INSTALL_PROGRESS_INTERVAL_BYTES,
        }
    }
}

impl InstallControl {
    /// Progress-only control with the default sample interval.
    #[must_use]
    pub fn with_progress(progress: mpsc::Sender<InstallProgress>) -> Self {
        Self {
            progress: Some(progress),
            ..Self::default()
        }
    }

    /// Cancellation-only control with the default sample interval.
    #[must_use]
    pub fn with_cancel(cancel: CancellationToken) -> Self {
        Self {
            cancel: Some(cancel),
            ..Self::default()
        }
    }
}
