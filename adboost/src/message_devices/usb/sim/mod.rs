//! Deterministic in-memory ADB-device simulator for protocol / timing tests.
//!
//! # What this is
//!
//! [`SimulatedDevice`] is a stateful model of `adbd` that implements
//! [`ADBMessageTransport`], so it plugs into the real
//! [`PersistentConnection<T>`] in place of [`USBTransport`] / [`TcpTransport`]
//! with **zero production change** (the persistent connection is already generic
//! over the transport). Where the existing `ScriptedTransport` test mock answers
//! every read with one canned CNXN banner, `SimulatedDevice` runs an actual
//! request/response state machine: it observes each frame the host *writes* and
//! enqueues the frames a real adbd would send back, so the live
//! `do_connect` / `do_auth` / reader / writer paths drive a faithful peer.
//!
//! [`ChunkedTransport`] is the complementary **byte-level** mock: it serializes
//! a `SimulatedDevice`'s frames to bytes and hands them to the consumer in
//! caller-controlled chunks (split mid-frame, coalesced across frames, or with
//! an injected mid-write failure), to drive the cancel-safe framing path
//! (`FrameReadBuffer`) end-to-end through the live reader/writer loops — the
//! sub-frame bug class a whole-frame mock structurally cannot reach. (The byte
//! fault scenarios land in Phase B; Phase A only proves it can carry a normal
//! handshake.)
//!
//! # How the reactive model maps onto a polled transport
//!
//! [`ADBMessageTransport`] is polled one frame at a time, but `adbd` is reactive:
//! except for connection death it speaks only when spoken to. So a
//! `SimulatedDevice` is a request/response state machine with an **outbound
//! queue**:
//!
//! - on each `write_message` the host sends, the device runs its reaction
//!   ([`SimState::react_to`]) synchronously and pushes any replies onto the
//!   queue;
//! - on each `read_message` the host issues, the device pops one frame from the
//!   queue, or — when the queue is empty — returns
//!   [`RustADBError::ReadTimeout`]. Honoring that idle-timeout contract (idle ≠
//!   failure) is what lets the reader loop's `ReadStep::ReadTimeout => continue`
//!   idle path run instead of wedging or tearing down.
//!
//! The transport is **cloned** into the connection's reader (bulk-IN) and writer
//! (bulk-OUT) halves, exactly as the real USB transport is. Both clones therefore
//! share one [`SimState`] behind an `Arc<Mutex<_>>`; the lock is never held
//! across an `.await` (mirroring `ScriptedTransport`), so a blocked read never
//! blocks a concurrent write.
//!
//! # Honest boundary — what a simulator can and cannot prove
//!
//! This harness tests protocol / state-machine logic **at and above** the
//! message-transport frame interface, plus byte-level cancel-safety via
//! [`ChunkedTransport`]. It deliberately does **not** prove:
//!
//! - **real OS error codes / latency distributions** — the mocks *emit* the
//!   `UsbTransferError` / `ReadTimeout` variants to drive the classifier; they do
//!   not prove the kernel, `nusb`, or `tokio` actually produces them in any given
//!   situation;
//! - **real device shell / filesystem** behavior;
//! - **real TLS / STLS upgrade** — the trait's `upgrade_connection` default is a
//!   no-op, so a simulator can answer STLS but not exercise a real handshake;
//! - **IOKit re-enumeration to a new registry id** (the back-to-back
//!   `adb root; adb unroot` OS artifact) — only the reopen-layer *reaction* to a
//!   dead handle is testable here, not the kernel event itself.
//!
//! Those remain hardware tests.
//!
//! [`ADBMessageTransport`]: crate::message_devices::adb_message_transport::ADBMessageTransport
//! [`PersistentConnection<T>`]: crate::message_devices::usb::persistent::PersistentConnection
//! [`USBTransport`]: crate::message_devices::usb::usb_transport::USBTransport
//! [`TcpTransport`]: crate::tcp::TcpTransport
//! [`RustADBError::ReadTimeout`]: crate::RustADBError::ReadTimeout

mod chunked;
mod device;
mod profile;
mod scenario;
mod state;

pub use chunked::ChunkedTransport;
pub use device::SimulatedDevice;
pub use profile::DeviceProfile;
pub use scenario::{OpenResponse, Scenario};

// Phase A handshake/CNXN/DRAIN/AUTH/DACK edge suite + B1/B2/B-feat regressions.
// Only compiled for adboost's own test runs (the `test-support` feature exposes
// the harness types to external crates, which bring their own tests).
#[cfg(test)]
mod tests;

// Phase B session/flow-control/teardown edge suite + ChunkedTransport byte-level
// fault scenarios (B3a/B8/B-recv/B4/B5/B7/B9).
#[cfg(test)]
mod tests_session;
