//! Fault / lifecycle injection for a [`SimulatedDevice`].
//!
//! A [`Scenario`] is the orthogonal "what goes wrong, and when" axis layered on
//! top of a [`DeviceProfile`](super::DeviceProfile)'s "which device" axis. Phase
//! A uses the handshake-relevant knobs (transient write/read errors, stale-CLSE
//! bursts, reader death); Phase B layers session-level faults on the same type.
//!
//! Every injected error is a *value* emitted above the transport interface to
//! drive the host's classifier — see the module-level honest-boundary note: the
//! simulator does not prove the OS produces these, only that the consumer reacts
//! correctly when it does.

use nusb::transfer::TransferError;

/// A programmable fault/lifecycle script for a [`SimulatedDevice`].
///
/// Built fluently from [`Scenario::healthy`] (no faults) via the `with_*`
/// setters. Defaults are all "no fault", so a test names only the knob it cares
/// about.
#[derive(Debug, Clone, Default)]
pub struct Scenario {
    /// Number of leading `write_message` calls that fail with
    /// [`Self::transient_error`] before writes start succeeding. Models adbd
    /// briefly not answering on a still-valid handle right after re-enumeration.
    pub(super) transient_writes: u32,
    /// Number of leading `read_message` calls that fail with
    /// [`Self::transient_error`] before reads behave normally.
    pub(super) transient_reads: u32,
    /// The transfer error emitted for the transient write/read faults above.
    /// Defaults to `NotResponding` (`kIOReturnNotResponding`).
    pub(super) transient_error: Option<TransferError>,
    /// Number of stale `CLSE` frames the device emits (ahead of its real CNXN
    /// reply) on connect, modeling a previous connection's orphaned streams.
    pub(super) stale_clse: u32,
    /// If set, the device's reader half dies after this many `read_message`
    /// calls: the Nth read returns a fatal (non-`ReadTimeout`) error, which the
    /// persistent reader treats as fatal → fires the `DeathSignal`. Models adbd
    /// closing the connection (an `adb root`/`unroot` restart).
    pub(super) die_after_reads: Option<u32>,
}

impl Scenario {
    /// The default: a perfectly healthy device, no injected faults.
    #[must_use]
    pub fn healthy() -> Self {
        Self::default()
    }

    /// The transfer error this scenario emits for its transient faults, defaulting
    /// to `NotResponding` (the textbook post-re-enumeration "endpoint enumerated
    /// but not answering yet" blip) when none was set explicitly.
    pub(super) fn transient_err(&self) -> TransferError {
        self.transient_error
            .unwrap_or(TransferError::Unknown(0xe000_02ed))
    }

    /// Fail the first `n` writes with `err` before writes succeed.
    #[must_use]
    pub fn with_transient_writes(mut self, n: u32, err: TransferError) -> Self {
        self.transient_writes = n;
        self.transient_error = Some(err);
        self
    }

    /// Fail the first `n` reads with `err` before reads behave normally.
    #[must_use]
    pub fn with_transient_reads(mut self, n: u32, err: TransferError) -> Self {
        self.transient_reads = n;
        self.transient_error = Some(err);
        self
    }

    /// Emit `n` stale `CLSE` frames before the real CNXN reply on connect.
    #[must_use]
    pub fn with_stale_clse(mut self, n: u32) -> Self {
        self.stale_clse = n;
        self
    }

    /// Make the reader half die (fatal error) on the `n`-th `read_message` call,
    /// modeling adbd closing the connection.
    #[must_use]
    pub fn with_death_after_reads(mut self, n: u32) -> Self {
        self.die_after_reads = Some(n);
        self
    }
}
