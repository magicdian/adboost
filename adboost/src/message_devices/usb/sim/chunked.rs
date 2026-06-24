//! [`ChunkedTransport`]: a **byte-level** [`ADBMessageTransport`] over the same
//! [`SimState`] adbd model, for the sub-frame bug class a whole-frame mock cannot
//! reach.
//!
//! Where [`SimulatedDevice`](super::SimulatedDevice) hands the consumer whole
//! [`ADBTransportMessage`]s, `ChunkedTransport` serializes the device's frames to
//! wire bytes and reassembles them through the real, shared
//! [`FrameReadBuffer`] — the same cancel-safe framing layer the USB/TCP
//! transports use. This lets a test deliver a frame in pieces (and, in Phase B,
//! split it across an idle `ReadTimeout`, coalesce several frames into one read,
//! or fail a write part-way) and assert the persistent reader/writer loops stay
//! frame-aligned.
//!
//! Phase A scope: prove it can carry a normal handshake. The `with_read_chunk`
//! knob already exercises the trickle-a-frame-across-`ReadTimeout`s path; the
//! richer fault scenarios (over-delivery, mid-write failure) build on this in
//! Phase B.
//!
//! Each clone gets an **independent** reassembly buffer (only the reader clone
//! reads, and it has consumed nothing at clone time), while the adbd
//! [`SimState`] is shared — matching how the real transport splits into bulk-IN
//! and bulk-OUT halves over one device.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Result;
use crate::RustADBError;
use crate::adb_transport::ADBTransport;
use crate::message_devices::adb_message_transport::ADBMessageTransport;
use crate::message_devices::adb_transport_message::ADBTransportMessage;
use crate::message_devices::framed_read::FrameReadBuffer;

use super::profile::DeviceProfile;
use super::scenario::Scenario;
use super::state::SimState;

/// A byte-level simulated ADB device transport sharing the [`SimState`] model.
///
/// Construct via [`ChunkedTransport::new`] / [`ChunkedTransport::with_scenario`]
/// and tune byte delivery with [`ChunkedTransport::with_read_chunk`]. See the
/// [module docs](self) for the bug class it targets.
pub struct ChunkedTransport {
    /// The adbd state machine, shared across clones (as in `SimulatedDevice`).
    state: Arc<Mutex<SimState>>,
    /// Bytes serialized from the device's frames, not yet handed to the reader's
    /// reassembly buffer. Per-clone (only the reader clone consumes it).
    pending_bytes: VecDeque<u8>,
    /// Cancel-safe reassembly buffer (the shared production framing layer).
    /// Per-clone: independent across the reader/writer split.
    read_buffer: FrameReadBuffer,
    /// Max bytes moved from `pending_bytes` into the reassembly buffer per
    /// `read_message` call. `None` = deliver the whole pending frame at once
    /// (the Phase A default). `Some(n)` = trickle ≤ `n` bytes per read, returning
    /// `ReadTimeout` until a whole frame has arrived.
    read_chunk: Option<usize>,
}

impl ChunkedTransport {
    /// Build a healthy byte-level transport for `profile` (no injected faults,
    /// whole-frame delivery).
    #[must_use]
    pub fn new(profile: DeviceProfile) -> Self {
        Self::with_scenario(profile, Scenario::healthy())
    }

    /// Build a byte-level transport for `profile` with `scenario`'s faults.
    #[must_use]
    pub fn with_scenario(profile: DeviceProfile, scenario: Scenario) -> Self {
        Self {
            state: Arc::new(Mutex::new(SimState::new(profile, scenario))),
            pending_bytes: VecDeque::new(),
            read_buffer: FrameReadBuffer::new(),
            read_chunk: None,
        }
    }

    /// Deliver at most `n` bytes per `read_message` call, returning `ReadTimeout`
    /// until a complete frame has been reassembled. Models a transport whose read
    /// deadline can elapse part-way through a logical frame.
    #[must_use]
    pub fn with_read_chunk(mut self, n: usize) -> Self {
        self.read_chunk = Some(n.max(1));
        self
    }

    /// Lock the shared state, mapping a poisoned mutex to
    /// [`RustADBError::PoisonError`]. The guard never crosses an `.await`.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, SimState>> {
        self.state.lock().map_err(|_| RustADBError::PoisonError)
    }

    /// Serialize a frame to its on-the-wire bytes: 24-byte header then payload.
    fn frame_to_bytes(msg: &ADBTransportMessage) -> Vec<u8> {
        let mut bytes = msg.header().as_bytes();
        bytes.extend_from_slice(msg.payload());
        bytes
    }
}

impl Clone for ChunkedTransport {
    /// Share the adbd state across the clone, but give the clone a **fresh**
    /// reassembly buffer and pending-byte queue: clones are taken at connection
    /// construction before any byte is read, and only the reader clone ever
    /// reads, so independent read state is the faithful (and correct) model.
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            pending_bytes: VecDeque::new(),
            read_buffer: FrameReadBuffer::new(),
            read_chunk: self.read_chunk,
        }
    }
}

impl ADBTransport for ChunkedTransport {
    async fn connect(&mut self) -> Result<()> {
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

impl ADBMessageTransport for ChunkedTransport {
    async fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        _write_timeout: Duration,
    ) -> Result<()> {
        let transient = {
            let mut state = self.lock()?;
            if state.take_transient_write() {
                Some(state.transient_err())
            } else {
                state.react_to(&message);
                None
            }
        };
        match transient {
            Some(err) => Err(RustADBError::UsbTransferError(err)),
            None => Ok(()),
        }
    }

    async fn read_message_with_timeout(
        &mut self,
        read_timeout: Duration,
    ) -> Result<ADBTransportMessage> {
        // The read deadline decides whether a partial frame may return early.
        //
        // A real transport, given a deadline, reads chunks off the wire until it
        // has a whole frame OR the deadline elapses. So a LARGE deadline (the
        // handshake's default `u64::MAX`-second read) assembles the whole frame
        // within the call and never returns `ReadTimeout` mid-frame; only a SHORT
        // deadline (the reader loop's 1 s, the stale-drain's 100 ms) can elapse
        // part-way through a trickled frame and surface the idle `ReadTimeout`.
        //
        // Modeling that with a threshold lets the SAME transport carry a clean
        // handshake AND exercise the reader loop's `ReadStep::ReadTimeout =>
        // continue` path on the same trickled byte stream — without a wall-clock
        // dependency. Chunking only engages when `with_read_chunk` is set.
        let assemble_whole = self.read_chunk.is_none() || read_timeout > Duration::from_secs(3600);

        loop {
            // 1. A whole frame may already be buffered (prior over-read / earlier
            //    chunks of this same call).
            if let Some(msg) = self.read_buffer.try_parse()? {
                return Ok(msg);
            }

            // 2. Refill `pending_bytes` from the next device frame when drained,
            //    consulting the same death/transient/idle accounting as the
            //    frame-level device.
            if self.pending_bytes.is_empty() {
                enum Refill {
                    Dead,
                    Transient(nusb::transfer::TransferError),
                    Bytes(Vec<u8>),
                    Idle,
                }
                let refill = {
                    let mut state = self.lock()?;
                    if state.reader_already_dead() {
                        Refill::Dead
                    } else if state.take_transient_read() {
                        Refill::Transient(state.transient_err())
                    } else if let Some(frame) = state.pop_outbound() {
                        Refill::Bytes(Self::frame_to_bytes(&frame))
                    } else if state.should_die_on_idle_read() {
                        Refill::Dead
                    } else {
                        Refill::Idle
                    }
                };
                match refill {
                    Refill::Dead => {
                        return Err(RustADBError::UsbTransferError(
                            nusb::transfer::TransferError::Disconnected,
                        ));
                    }
                    Refill::Transient(err) => return Err(RustADBError::UsbTransferError(err)),
                    Refill::Bytes(bytes) => self.pending_bytes.extend(bytes),
                    // No frame queued: a genuine idle read.
                    Refill::Idle => return Err(RustADBError::ReadTimeout),
                }
            }

            // 3. Move one chunk of pending bytes into the reassembly buffer.
            let take = match self.read_chunk {
                Some(n) => n.min(self.pending_bytes.len()),
                None => self.pending_bytes.len(),
            };
            let chunk: Vec<u8> = self.pending_bytes.drain(..take).collect();
            self.read_buffer.push(&chunk);

            // 4. Complete frame now → deliver it. Otherwise: under a large
            //    deadline keep pulling chunks (loop); under a short deadline the
            //    deadline elapsed mid-frame → idle `ReadTimeout`, buffered bytes
            //    retained for the next call (the cancel-safety property).
            if let Some(msg) = self.read_buffer.try_parse()? {
                return Ok(msg);
            }
            if !assemble_whole {
                return Err(RustADBError::ReadTimeout);
            }
        }
    }
}
