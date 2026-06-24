//! [`SimulatedDevice`]: the frame-level [`ADBMessageTransport`] front end over
//! the shared [`SimState`] adbd state machine.
//!
//! This type owns all the `async` / locking / timeout plumbing so that
//! [`SimState`] stays pure. The state lives behind an `Arc<Mutex<_>>` shared by
//! every [`Clone`] (the persistent connection's reader and writer halves each get
//! one), and the lock is **never held across an `.await`** — every method takes
//! the lock, computes synchronously, drops the guard, and only then awaits.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Result;
use crate::RustADBError;
use crate::adb_transport::ADBTransport;
use crate::message_devices::adb_message_transport::ADBMessageTransport;
use crate::message_devices::adb_transport_message::ADBTransportMessage;

use super::profile::DeviceProfile;
use super::scenario::Scenario;
use super::state::SimState;

/// A deterministic, in-memory simulated ADB device that implements
/// [`ADBMessageTransport`], so it can be handed to the real
/// [`PersistentConnection`](crate::message_devices::usb::persistent::PersistentConnection)
/// in place of a USB/TCP transport.
///
/// Construct from a [`DeviceProfile`] (which Android/adbd) and optionally a
/// [`Scenario`] (what faults to inject), then pass to `PersistentConnection::new`
/// / `new_with_features`. See the [module docs](super) for the reactive model and
/// the honest boundary of what it proves.
#[derive(Clone)]
pub struct SimulatedDevice {
    /// The adbd state machine, shared across the reader/writer clones.
    state: Arc<Mutex<SimState>>,
}

impl SimulatedDevice {
    /// Build a healthy simulated device for `profile` (no injected faults).
    #[must_use]
    pub fn new(profile: DeviceProfile) -> Self {
        Self::with_scenario(profile, Scenario::healthy())
    }

    /// Build a simulated device for `profile` with `scenario`'s injected faults.
    #[must_use]
    pub fn with_scenario(profile: DeviceProfile, scenario: Scenario) -> Self {
        Self {
            state: Arc::new(Mutex::new(SimState::new(profile, scenario))),
        }
    }

    /// Lock the shared state, mapping a poisoned mutex to
    /// [`RustADBError::PoisonError`] (per the project's no-`unwrap`-on-`Mutex`
    /// rule). The guard is always dropped before any `.await`.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, SimState>> {
        self.state.lock().map_err(|_| RustADBError::PoisonError)
    }

    /// Whether the simulated device has completed its handshake. Test helper for
    /// assertions that don't go through the persistent connection.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.lock().is_ok_and(|s| s.is_connected())
    }
}

impl ADBTransport for SimulatedDevice {
    async fn connect(&mut self) -> Result<()> {
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

impl ADBMessageTransport for SimulatedDevice {
    async fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        _write_timeout: Duration,
    ) -> Result<()> {
        // Take the lock, run the synchronous reaction, decide the outcome, then
        // drop the guard before returning (no `.await` while held).
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
        _read_timeout: Duration,
    ) -> Result<ADBTransportMessage> {
        // Resolve the read outcome under the lock, then drop the guard. Outcomes,
        // in priority order:
        //  1. reader already latched dead → fatal error (drives DeathSignal)
        //  2. transient delivery blip     → transient transfer error
        //  3. a queued frame              → deliver it (never eaten by death)
        //  4. idle + death scheduled      → fatal death this read
        //  5. idle, healthy               → ReadTimeout (idle ≠ failure contract)
        enum Outcome {
            Dead,
            Transient(nusb::transfer::TransferError),
            Frame(ADBTransportMessage),
            Idle,
        }
        let outcome = {
            let mut state = self.lock()?;
            if state.reader_already_dead() {
                Outcome::Dead
            } else if state.take_transient_read() {
                Outcome::Transient(state.transient_err())
            } else if let Some(frame) = state.pop_outbound() {
                Outcome::Frame(frame)
            } else if state.should_die_on_idle_read() {
                Outcome::Dead
            } else {
                Outcome::Idle
            }
        };
        match outcome {
            // A generic disconnect: the persistent reader treats any non-
            // `ReadTimeout`, non-`InvalidIntegrity` error as fatal and fires the
            // DeathSignal — exactly the "adbd closed the connection" edge.
            Outcome::Dead => Err(RustADBError::UsbTransferError(
                nusb::transfer::TransferError::Disconnected,
            )),
            Outcome::Transient(err) => Err(RustADBError::UsbTransferError(err)),
            Outcome::Frame(frame) => Ok(frame),
            // The single transport-neutral idle signal mandated by the trait's
            // read-timeout contract. Drives `ReadStep::ReadTimeout => continue`.
            Outcome::Idle => Err(RustADBError::ReadTimeout),
        }
    }
}
