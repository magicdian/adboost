use crate::proxy::host_service::HostServiceError;

/// Errors returned by the adb device tracker.
///
/// The tracker keeps its own error taxonomy instead of adding variants to the
/// crate-wide [`crate::RustADBError`]: a tracker failure is a self-contained
/// protocol/serialization concern, and downstream callers can match it without
/// depending on the rest of the ADB client's error surface.
#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    /// The adb server rejected the tracker service with `FAIL`.
    #[error("unsupported adb service '{service}': {detail}")]
    Unsupported {
        /// The host service that failed.
        service: &'static str,
        /// The server's `FAIL` reason.
        detail: String,
    },
    /// The host protocol or tracker framing is malformed.
    #[error("tracker protocol error: {detail}")]
    Protocol {
        /// Human-readable description.
        detail: String,
    },
    /// A tracker frame could not be decoded.
    #[error("tracker decode error at row {row}: {detail}")]
    Decode {
        /// The device row index (0 for a whole-frame failure).
        row: usize,
        /// Human-readable description.
        detail: String,
    },
    /// A serial failed the tracker's identity invariant.
    #[error("invalid device serial {serial:?}: {reason}")]
    InvalidSerial {
        /// The rejected serial.
        serial: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// The underlying socket or TCP connection failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<HostServiceError> for TrackerError {
    fn from(error: HostServiceError) -> Self {
        match error {
            HostServiceError::Io(error) => Self::Io(error),
            HostServiceError::Protocol(detail) => Self::Protocol { detail },
        }
    }
}
