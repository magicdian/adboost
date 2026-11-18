/// Classification of an `adb root` / `adb unroot` device receipt.
///
/// adbd answers these services with a single human-readable line; the wording is
/// stable in AOSP but vendor builds vary, so an unrecognized receipt is reported
/// as [`RootState::Unknown`] with the raw text preserved on
/// [`RootOutcome::raw`] rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootState {
    /// adbd is restarting into the requested mode (`restarting adbd as root`,
    /// `restarting adbd as non root`). Callers should wait (bounded) for the
    /// device to reconnect — adboost deliberately does not wait itself.
    Restarting,
    /// adbd was already in the requested mode: already root for `root`, already
    /// non-root for `unroot`.
    AlreadyRoot,
    /// The device refused the privilege change, e.g.
    /// `adbd cannot run as root in production builds`. Treat this as a failure.
    Refused,
    /// Unrecognized or empty receipt; [`RootOutcome::raw`] still holds the text.
    Unknown,
}

/// Result of a `root` / `unroot` request: the classified state plus the receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootOutcome {
    /// Classified device state.
    pub state: RootState,
    /// Raw device receipt, preserved for diagnostics and vendor-specific wording.
    pub raw: String,
}
