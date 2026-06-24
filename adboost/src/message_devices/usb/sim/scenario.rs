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

/// How the simulated device reacts to a host `OPEN` (session establishment).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpenResponse {
    /// Accept the stream: reply one `OKAY` (the default healthy behavior).
    #[default]
    Accept,
    /// Accept, but reply `OKAY` **twice** — the double-OKAY framing a real adbd
    /// can emit (an extra ready/credit OKAY). The host's open must tolerate it.
    AcceptDoubleOkay,
    /// Reject the stream: reply `CLSE(arg0 = 0, arg1 = host_local_id)` on the
    /// data channel instead of an `OKAY`. The host must fast-fail, NOT hang for
    /// the 10 s OPEN timeout (bug #3a).
    RejectWithClse,
    /// Send nothing at all — model an OPEN the device silently ignores, so the
    /// host hits its OPEN-response timeout.
    Ignore,
}

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
    /// How the device reacts to a host `OPEN` (accept / double-OKAY / reject /
    /// ignore). Defaults to [`OpenResponse::Accept`].
    pub(super) open_response: OpenResponse,
    /// If set, on each host `WRTE` the device echoes back a `WRTE` carrying this
    /// many bytes (clamped to the payload), modeling a device that produces
    /// output. `None` = the device only acknowledges writes (`OKAY`), no echo.
    pub(super) echo_bytes: Option<usize>,
    /// If true, the device sends a `CLSE` immediately after accepting a session's
    /// first WRTE — modeling a stream the device tears down early.
    pub(super) close_after_first_write: bool,
    /// If set, on the session's first host `WRTE` the device replies with a
    /// `WRTE` carrying exactly these bytes (instead of echoing). Lets a test feed
    /// a session a crafted/truncated sub-protocol frame (e.g. a too-short SYNC
    /// frame for the B-recv panic guard).
    pub(super) first_write_reply: Option<Vec<u8>>,
    /// `ChunkedTransport` only: coalesce up to this many device frames into a
    /// single read delivery (bulk-IN over-delivery, B5). `None`/`<=1` = one frame
    /// per refill. The reassembly buffer must still split them cleanly.
    pub(super) coalesce_frames: Option<usize>,
    /// `ChunkedTransport` only: if set, the writer fails the `n`-th
    /// `write_message` *after* committing `WriteFault::after_bytes` of it,
    /// modeling a mid-frame truncation (B7). The persistent writer must treat a
    /// partial-frame write as fatal (poison), not warn-and-continue.
    pub(super) write_fault: Option<WriteFault>,
}

/// A mid-frame write truncation injected by [`ChunkedTransport`] (B7).
#[derive(Debug, Clone, Copy)]
pub(super) struct WriteFault {
    /// 1-based index of the `write_message` call that fails.
    pub(super) on_write: u32,
    /// Bytes of that frame "committed" before the failure (`> 0` = a mid-frame
    /// truncation → fatal; `0` = nothing reached the wire → recoverable
    /// `WriteTimeout`, the backpressure case B9).
    pub(super) after_bytes: usize,
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

    /// Set how the device reacts to a host `OPEN`.
    #[must_use]
    pub fn with_open_response(mut self, response: OpenResponse) -> Self {
        self.open_response = response;
        self
    }

    /// Echo back `n` bytes (clamped to the written payload) on each host `WRTE`,
    /// modeling a device that produces output.
    #[must_use]
    pub fn with_echo_bytes(mut self, n: usize) -> Self {
        self.echo_bytes = Some(n);
        self
    }

    /// Send a `CLSE` right after accepting the session's first `WRTE`.
    #[must_use]
    pub fn with_close_after_first_write(mut self) -> Self {
        self.close_after_first_write = true;
        self
    }

    /// Reply to the session's first host `WRTE` with a device `WRTE` carrying
    /// exactly `bytes` (a crafted sub-protocol frame), instead of echoing.
    #[must_use]
    pub fn with_first_write_reply(mut self, bytes: Vec<u8>) -> Self {
        self.first_write_reply = Some(bytes);
        self
    }

    /// (`ChunkedTransport`) Coalesce up to `n` device frames into one read
    /// delivery, exercising bulk-IN over-delivery reassembly (B5).
    #[must_use]
    pub fn with_coalesced_frames(mut self, n: usize) -> Self {
        self.coalesce_frames = Some(n);
        self
    }

    /// (`ChunkedTransport`) Fail the `on_write`-th `write_message` after
    /// committing `after_bytes` of the frame. `after_bytes > 0` = a fatal
    /// mid-frame truncation (B7); `after_bytes == 0` = a recoverable backpressure
    /// `WriteTimeout` with nothing on the wire (B9).
    #[must_use]
    pub fn with_write_fault(mut self, on_write: u32, after_bytes: usize) -> Self {
        self.write_fault = Some(WriteFault {
            on_write,
            after_bytes,
        });
        self
    }
}
