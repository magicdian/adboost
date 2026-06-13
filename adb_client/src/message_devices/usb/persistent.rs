//! Persistent USB connection with session multiplexing (async / tokio).
//!
//! Holds a single CNXN+AUTH'd USB connection and allows multiple concurrent
//! ADB sessions (shell, tcp, sync) to be opened without re-authenticating.
//!
//! # Architecture
//!
//! Two long-lived tokio tasks own the two halves of the USB pipe:
//!
//! - **reader task**: the single owner of the bulk-IN endpoint. It reads frames
//!   in a loop and demultiplexes them by `local_id` into per-session channels,
//!   the device-OPEN queue, and raw subscribers. It also owns the session
//!   registry *privately* (no shared mutex): registration / unregistration flow
//!   in over a control channel and are applied between reads. The routing
//!   decision is the I/O-free [`classify_message`] (unit-tested without
//!   hardware). The reader NEVER blocks — all sends use `try_send`; on overflow
//!   it drops and `tracing::warn!`s so the loss is observable.
//! - **writer task**: the single owner of the bulk-OUT endpoint. Every outbound
//!   frame (OPEN / OKAY / WRTE / CLSE / raw) is delivered to it over one mpsc
//!   channel as an [`OutboundFrame`]. It serializes the writes:
//!   `OutboundFrame::FireForget` (OKAY / CLSE / OPEN / raw) is enqueued and
//!   forgotten; `OutboundFrame::WithAck` (WRTE) carries a `oneshot::Sender` so
//!   the caller can `.await` the write's `Result` before debiting the
//!   flow-control window. Because the reader task never writes OUT, the OUT
//!   endpoint has a single physical writer enforced structurally — no shared
//!   writer mutex anywhere.
//!
//! Teardown is explicit: [`PersistentUsbConnection::shutdown`] (`&self`, for
//! `Arc`-held connections such as the server backend's cache),
//! [`PersistentUsbConnection::close`] (`self`), and [`MultiplexedSession::close`]
//! send their CLSE and await confirmation. A connection-level graceful close
//! flushes ONE connection CLSE and sets a shared `conn_closed` flag; each live
//! session's `Drop` then skips its now-redundant per-stream CLSE (the device
//! already knows the whole connection is gone). `Drop` is the best-effort
//! fallback: if no graceful close ran, it enqueues a connection CLSE
//! fire-and-forget onto the writer channel; either way it `abort()`s the
//! reader/writer tasks (Rust stable has no async `Drop`). Always prefer the
//! explicit graceful path — at process teardown the writer task is often already
//! gone, so a fire-and-forget CLSE fails to enqueue and the device is left with
//! orphaned streams that reject the next CNXN with a stale CLSE.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use rand::RngExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::adb_transport::ADBTransport;
use crate::message_devices::adb_message_transport::ADBMessageTransport;
use crate::message_devices::adb_transport_message::{
    ADBTransportMessage, AUTH_RSAPUBLICKEY, AUTH_SIGNATURE, AUTH_TOKEN,
};
use crate::message_devices::message_commands::MessageCommand;
use crate::message_devices::models::{ADBRsaKey, read_adb_private_key};
use crate::message_devices::usb::flow_control::{
    FlowControl, INITIAL_DELAYED_ACK_BYTES, MAX_PAYLOAD, encode_okay_payload, parse_okay_delta,
};
use crate::message_devices::usb::shell_v2_session::ShellV2Session;
use crate::message_devices::usb::sync_session::SyncSession;
use crate::message_devices::usb::usb_transport::USBTransport;
use crate::models::{ADBLocalCommand, DeviceFeatureSet, FEATURE_DELAYED_ACK};
use crate::utils::get_default_adb_key_path;
use crate::{Result, RustADBError};

/// Channel buffer size for per-session message queues.
const SESSION_CHANNEL_SIZE: usize = 64;

/// Channel buffer size for the device-originated OPEN (`incoming_opens`) queue.
const PENDING_OPENS_CHANNEL_SIZE: usize = 64;

/// Channel buffer size for each raw subscriber (`subscribe_raw`) queue.
const RAW_SUBSCRIBER_CHANNEL_SIZE: usize = 64;

/// Channel buffer size for the writer task's outbound-frame queue.
const WRITER_CHANNEL_SIZE: usize = 256;

/// Channel buffer size for the reader task's session-registry control queue.
const CONTROL_CHANNEL_SIZE: usize = 64;

/// Max CNXN handshake attempts before giving up. adbd can emit one stale CLSE
/// per orphaned stream left by a previous connection's unclean teardown; the
/// multi-session server path can leave several, so this is well above the old
/// fixed 3. Each stale CLSE also triggers a re-drain (see `do_connect`).
const CNXN_MAX_ATTEMPTS: u32 = 8;

/// Upper bound on frames drained per [`PersistentUsbConnection::drain_stale`]
/// pass, so a device that keeps emitting frames cannot wedge the drain forever.
const STALE_DRAIN_MAX_FRAMES: usize = 64;

/// Legacy ADB wire version; predates `delayed_ack` windowed flow control.
const A_VERSION_LEGACY: u32 = 0x0100_0000;
/// First ADB wire version that permits `delayed_ack` windowing
/// (AOSP `A_VERSION_SKIP_CHECKSUM`). Windowing MUST NOT be enabled below this.
const A_VERSION_SKIP_CHECKSUM: u32 = 0x0100_0001;

/// Boxed predicate used to filter raw-tee'd messages for a subscriber.
type RawFilter = Box<dyn Fn(&ADBTransportMessage) -> bool + Send>;

/// A registered raw subscriber: its filter predicate plus the sender side of
/// its bounded queue. Lives in the reader task's private subscriber list.
struct RawSubscriber {
    filter: RawFilter,
    tx: mpsc::Sender<ADBTransportMessage>,
}

/// An outbound frame queued for the single writer task.
///
/// `FireForget` is used for OKAY / CLSE / OPEN / raw sends where the caller does
/// not need the write `Result` (it is enqueued and forgotten — Drop also uses
/// this to push a best-effort CLSE). `WithAck` is used for WRTE so the caller
/// can `.await` the write's `Result` before debiting the flow-control window,
/// preserving send-side accounting correctness (P1-①, write-completion option 1).
enum OutboundFrame {
    FireForget(ADBTransportMessage),
    WithAck(ADBTransportMessage, oneshot::Sender<io::Result<()>>),
}

/// A control message to the reader task to mutate its private session registry
/// or subscriber list. The reader applies these between reads via `select!` so
/// the registry never needs a shared lock.
enum ReaderControl {
    Register(u32, SessionChannels),
    Unregister(u32),
    Subscribe(RawSubscriber),
}

/// Handle used to talk to the single writer task: enqueue outbound frames.
/// Cloned into every session half so each can write/close independently.
#[derive(Clone)]
struct WriterHandle {
    tx: mpsc::Sender<OutboundFrame>,
}

impl WriterHandle {
    /// Enqueue a fire-and-forget frame (OKAY / CLSE / OPEN / raw). Returns an
    /// error only if the writer task is gone (channel closed) or its queue is
    /// full — both surface as a broken pipe to the caller.
    fn try_send_fire_forget(&self, msg: ADBTransportMessage) -> io::Result<()> {
        self.tx
            .try_send(OutboundFrame::FireForget(msg))
            .map_err(|e| match e {
                TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "writer queue full")
                }
                TrySendError::Closed(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "writer task gone")
                }
            })
    }

    /// Enqueue a WRTE with an ack channel and await the writer task's write
    /// result. Used by the send path so the window is debited only after the
    /// frame is actually on the wire.
    async fn send_with_ack(&self, msg: ADBTransportMessage) -> io::Result<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(OutboundFrame::WithAck(msg, ack_tx))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer task gone"))?;
        ack_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer dropped ack"))?
    }
}

/// Flush exactly one connection-level CLSE through `writer` (awaiting the write
/// confirmation) and set `conn_closed`. Idempotent: a `compare_exchange` makes
/// the first caller win, so repeated `shutdown`/`close` calls — and a later
/// `Drop` — send nothing more. Factored out of [`PersistentUsbConnection`] so it
/// is the single source of truth for the graceful connection CLSE and is
/// unit-testable against a bare writer channel.
async fn flush_connection_clse_impl(writer: &WriterHandle, conn_closed: &Arc<AtomicBool>) {
    if conn_closed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return; // already flushed by a prior shutdown()/close()
    }
    if let Ok(clse) = ADBTransportMessage::try_new(MessageCommand::Clse, 0, 0, &[]) {
        // Best-effort: a closed/full writer at graceful teardown is expected and
        // not warned (unlike Drop's fire-and-forget path).
        let _ = writer.send_with_ack(clse).await;
    }
}

/// Routing decision computed by [`classify_message`] for a single inbound
/// message. This is the I/O-free heart of the reader-loop demux: it depends
/// only on the message header and the set of known session ids, so it can be
/// unit-tested without any USB hardware (see the `tests` module). The reader
/// loop turns a [`RouteDecision`] into the actual channel `try_send`s.
#[derive(Debug, PartialEq, Eq)]
enum RouteDecision {
    /// An `OKAY` for a registered session → its ack channel.
    SessionAck(u32),
    /// A `WRTE`/`CLSE`/other for a registered session → its data channel.
    SessionData(u32),
    /// A device-originated `OPEN` (target local id not registered) → the
    /// `pending_opens` queue exposed via `incoming_opens()`.
    DeviceOpen,
    /// A message for an unknown session id that is not an OPEN → dropped.
    Unknown,
}

/// Classify an inbound message into a [`RouteDecision`], given the set of
/// registered session local ids. Pure / I/O-free for unit testing (D1).
///
/// Note: the raw-subscriber tee is orthogonal to this decision — every message
/// is offered to matching raw subscribers *in addition* to its primary route,
/// so the tee is handled separately in the reader loop and not represented
/// here.
fn classify_message(
    msg: &ADBTransportMessage,
    known_sessions: &HashMap<u32, SessionChannels>,
) -> RouteDecision {
    let target_id = msg.header().arg1();
    let command = msg.header().command();
    if known_sessions.contains_key(&target_id) {
        if command == MessageCommand::Okay {
            RouteDecision::SessionAck(target_id)
        } else {
            RouteDecision::SessionData(target_id)
        }
    } else if command == MessageCommand::Open {
        // Device-originated OPEN: `A_OPEN(device_local_id, 0, "<dest>")`.
        // Its target local id (arg1) is 0 / never registered, so it falls here
        // rather than into a session. The crate does not implement reverse
        // policy: it just surfaces the OPEN for the caller (xdb) to accept
        // (reply OKAY + register a session) or reject (reply CLSE).
        RouteDecision::DeviceOpen
    } else {
        RouteDecision::Unknown
    }
}

/// Timeout for the OPEN → first-response handshake.
const OPEN_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// The acceptor's initial send-window controller for a device-initiated OPEN.
///
/// Pure / I/O-free so the windowing decision can be unit tested (D1).
/// `windowed` is the CONNECTION's `delayed_ack` mode (adbd gates its OKAY payloads
/// on the connection level, so the session must match it to credit the send
/// window from adbd's OKAYs). `initial_grant` is the device's OPEN `arg1` (often
/// 0), the starting send credit; further credit arrives via adbd's OKAYs.
fn acceptor_send_flow(windowed: bool, initial_grant: i64) -> FlowControl {
    if windowed {
        FlowControl::new_windowed(initial_grant)
    } else {
        FlowControl::new_classic()
    }
}

/// Await the device's first response to an OPEN, racing the session's ACK and
/// DATA channels (within [`OPEN_RESPONSE_TIMEOUT`]).
///
/// Returns the OKAY message (success) on `ack_rx`, or a fast, actionable error
/// when the device instead routes a frame to `data_rx` — which, before the
/// first OKAY, can only be a CLSE rejecting the OPEN. On rejection AOSP adbd
/// sends `A_CLSE(arg0=0, arg1=local_id)` (`send_close(0, p->msg.arg0, t)`); the
/// reader routes it to `data_rx` (its command is not OKAY), so waiting only on
/// `ack_rx` would never observe it and the open would burn the full timeout
/// (bug #3). `biased` prefers a genuine OKAY when both are simultaneously ready.
///
/// I/O-free over the channels for unit testing (D1): drive a message into the
/// respective sender and assert the decision.
async fn await_open_response(
    ack_rx: &mut mpsc::Receiver<ADBTransportMessage>,
    data_rx: &mut mpsc::Receiver<ADBTransportMessage>,
) -> Result<ADBTransportMessage> {
    let raced = tokio::time::timeout(OPEN_RESPONSE_TIMEOUT, async {
        tokio::select! {
            biased;
            ack = ack_rx.recv() => match ack {
                Some(m) => Ok(m),
                None => Err(RustADBError::SendError),
            },
            _data = data_rx.recv() => {
                // arg1 == our local_id is already guaranteed by the reader's
                // routing; arg0 is 0 per AOSP and must NOT be required.
                Err(RustADBError::ADBRequestFailed(
                    "open_session: OPEN rejected by device (CLSE)".into(),
                ))
            }
        }
    })
    .await;

    match raced {
        Ok(inner) => inner,
        Err(_) => Err(RustADBError::ADBRequestFailed(
            "open_session: timeout waiting for OKAY".into(),
        )),
    }
}

/// Whether a device CNXN banner advertises the `delayed_ack` feature.
///
/// An ADB banner looks like `device::ro.product.name=...;features=shell_v2,cmd,delayed_ack,...`.
/// We scan for a `features=` segment and check its comma-separated list for the
/// `delayed_ack` token. Pure / I/O-free for unit testing (D1). `delayed_ack`
/// windowed flow control is only enabled when BOTH ends advertise it.
fn banner_advertises_delayed_ack(banner: &str) -> bool {
    // The banner may be NUL-terminated and contains `;`-separated key=value
    // segments. Find the `features=` segment and split its value on commas.
    banner
        .split([';', '\0'])
        .filter_map(|seg| seg.strip_prefix("features="))
        .any(|features| features.split(',').any(|f| f.trim() == FEATURE_DELAYED_ACK))
}

/// Decide whether `delayed_ack` windowed flow control may be enabled.
///
/// Windowing requires agreement on BOTH the feature and the wire version:
/// our end must advertise `delayed_ack` (`local_delayed_ack`), the device's
/// CNXN banner must advertise it, and the negotiated wire version must be
/// `>= A_VERSION_SKIP_CHECKSUM`. We connect at that version iff we advertise
/// the feature, so `device_version` here is the effective min of both ends.
/// Enabling windowing without the version makes adbd ignore the windowed OPEN
/// (no OKAY → `open_session` times out) — the defect this gate prevents.
/// Pure / I/O-free for unit testing (D1).
fn negotiate_delayed_ack(
    local_delayed_ack: bool,
    device_banner: &str,
    device_version: u32,
) -> bool {
    local_delayed_ack
        && banner_advertises_delayed_ack(device_banner)
        && device_version >= A_VERSION_SKIP_CHECKSUM
}

/// A persistent USB connection to an ADB device that supports concurrent sessions.
///
/// Unlike the per-operation model in `ADBUSBDevice`, this holds the USB handle
/// permanently and multiplexes multiple sessions over a single authenticated connection.
pub struct PersistentUsbConnection {
    /// Handle to enqueue outbound frames onto the single writer task.
    writer: WriterHandle,
    /// Control channel to mutate the reader task's private session registry.
    control_tx: mpsc::Sender<ReaderControl>,
    /// Reader task handle (aborted on Drop).
    reader_handle: Option<JoinHandle<()>>,
    /// Writer task handle (aborted on Drop).
    writer_handle: Option<JoinHandle<()>>,
    /// Features advertised to the device in the CNXN banner.
    features: DeviceFeatureSet,
    /// Whether `delayed_ack` windowed flow control is negotiated for this
    /// connection: `true` iff BOTH this end advertised it (`features.delayed_ack`)
    /// AND the device's CNXN banner advertised it. When `false`, sessions use
    /// classic strict stop-and-wait. See [`FlowControl`].
    delayed_ack_negotiated: bool,
    /// The single consumer side of the device-originated OPEN queue. `Mutex<Option>`
    /// so it can be taken by [`Self::incoming_opens`] through a shared `&self`
    /// (the connection is typically held as `Arc`); subsequent calls return an
    /// error. The matching sender lives in (and is kept alive by) the reader
    /// task, so the receiver reports disconnect once the reader stops.
    pending_opens_rx: std::sync::Mutex<Option<mpsc::Receiver<ADBTransportMessage>>>,
    /// Set once a connection-level CLSE has been flushed by [`Self::shutdown`] /
    /// [`Self::close`] (or the connection is otherwise being torn down). Cloned
    /// into every [`SessionInner`] so a session's `Drop` skips its now-redundant
    /// per-stream CLSE: the single connection-level CLSE already told the device
    /// every stream on this connection is gone, and the writer task is being
    /// intentionally retired — re-enqueueing per-session CLSEs would only race
    /// the writer's teardown and emit spurious "writer task gone" warnings.
    conn_closed: Arc<AtomicBool>,
}

impl PersistentUsbConnection {
    /// Create a new persistent connection from a USB transport.
    ///
    /// Performs CNXN+AUTH handshake, then spawns reader + writer tasks for
    /// message demuxing and serialized writes.
    ///
    /// Advertises the honest [`DeviceFeatureSet::default`] banner. To advertise a
    /// custom feature set, use [`Self::new_with_features`].
    pub async fn new(transport: USBTransport, private_key_path: Option<PathBuf>) -> Result<Self> {
        Self::new_with_features(transport, private_key_path, DeviceFeatureSet::default()).await
    }

    /// Create a new persistent connection advertising an explicit feature set.
    ///
    /// The `features` set determines the `host::features=` list sent in the CNXN
    /// banner. Only advertise features this end actually implements — see
    /// [`DeviceFeatureSet`].
    pub async fn new_with_features(
        transport: USBTransport,
        private_key_path: Option<PathBuf>,
        features: DeviceFeatureSet,
    ) -> Result<Self> {
        let key_path = match private_key_path {
            Some(p) => p,
            None => get_default_adb_key_path()?,
        };

        let private_key = if let Some(k) = read_adb_private_key(&key_path)? {
            k
        } else {
            tracing::warn!(
                "No private key found at {}. Generating random.",
                key_path.display()
            );
            ADBRsaKey::new_random()?
        };

        // Connect the transport (claim interface, find endpoints)
        let mut transport = transport;
        transport.connect().await?;

        // Perform CNXN handshake; the device's banner tells us which features it
        // supports so we can negotiate `delayed_ack` (intersection of both ends).
        let (device_version, device_banner) =
            Self::do_connect(&mut transport, &private_key, &features).await?;
        // Windowing is only valid once BOTH ends advertise `delayed_ack` AND the
        // negotiated wire version is `>= A_VERSION_SKIP_CHECKSUM`. We advertise
        // that version iff `features.delayed_ack`, so gating on `device_version`
        // here is the effective min of the two ends.
        let delayed_ack_negotiated =
            negotiate_delayed_ack(features.delayed_ack, &device_banner, device_version);
        tracing::debug!(
            "PersistentUsb: delayed_ack negotiated = {delayed_ack_negotiated} (local={}, device_banner_has_it={}, device_version={device_version:#010x})",
            features.delayed_ack,
            banner_advertises_delayed_ack(&device_banner)
        );

        let (pending_opens_tx, pending_opens_rx) = mpsc::channel(PENDING_OPENS_CHANNEL_SIZE);
        let (control_tx, control_rx) = mpsc::channel(CONTROL_CHANNEL_SIZE);
        let (writer_tx, writer_rx) = mpsc::channel(WRITER_CHANNEL_SIZE);

        // The reader task owns one clone of the transport (drives bulk-IN);
        // the writer task owns the original (drives bulk-OUT). Both share the
        // same underlying `Arc<DeviceHandle>` but use the separate endpoint
        // locks, so reads never block writes (see `USBTransport`).
        let reader_transport = transport.clone();
        let writer_transport = transport;

        let reader_handle = tokio::spawn(async move {
            Self::reader_loop(reader_transport, control_rx, pending_opens_tx).await;
        });

        let writer_handle = tokio::spawn(async move {
            Self::writer_loop(writer_transport, writer_rx).await;
        });

        Ok(Self {
            writer: WriterHandle { tx: writer_tx },
            control_tx,
            reader_handle: Some(reader_handle),
            writer_handle: Some(writer_handle),
            features,
            delayed_ack_negotiated,
            pending_opens_rx: std::sync::Mutex::new(Some(pending_opens_rx)),
            conn_closed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Create from `vendor_id/product_id`.
    ///
    /// Advertises the honest [`DeviceFeatureSet::default`] banner. To advertise a
    /// custom feature set, use [`Self::new_from_ids_with_features`].
    pub async fn new_from_ids(
        vendor_id: u16,
        product_id: u16,
        private_key_path: Option<PathBuf>,
    ) -> Result<Self> {
        Self::new_from_ids_with_features(
            vendor_id,
            product_id,
            private_key_path,
            DeviceFeatureSet::default(),
        )
        .await
    }

    /// Create from `vendor_id/product_id`, advertising an explicit feature set.
    pub async fn new_from_ids_with_features(
        vendor_id: u16,
        product_id: u16,
        private_key_path: Option<PathBuf>,
        features: DeviceFeatureSet,
    ) -> Result<Self> {
        let transport = USBTransport::new(vendor_id, product_id).await?;
        Self::new_with_features(transport, private_key_path, features).await
    }

    /// Create a persistent connection to the device with the given USB serial.
    ///
    /// Unlike [`Self::new_from_ids`], the serial unambiguously selects one
    /// device even when several share the same `vendor_id`/`product_id` — it is
    /// the identifier `adb devices` shows (see
    /// [`USBTransport::new_by_serial`]). Advertises the honest
    /// [`DeviceFeatureSet::default`] banner.
    pub async fn new_from_serial(serial: &str, private_key_path: Option<PathBuf>) -> Result<Self> {
        let transport = USBTransport::new_by_serial(serial).await?;
        Self::new(transport, private_key_path).await
    }

    /// The feature set advertised to the device in the CNXN banner.
    #[must_use]
    pub fn device_features(&self) -> &DeviceFeatureSet {
        &self.features
    }

    /// Subscribe to device-originated `OPEN` messages (pull model).
    ///
    /// Returns the consumer side of a bounded queue. The reader task routes
    /// every inbound `OPEN` whose target local id is not a known session
    /// (i.e. a device-initiated stream such as a `reverse:`/scrcpy connection,
    /// `A_OPEN(device_local_id, 0, "<dest>")`) into this queue. The caller
    /// decides whether to accept the stream — turn the OPEN into a session via
    /// [`Self::accept_device_open`] — or reject it (reply `CLSE(0, device_local_id)`
    /// via [`Self::send_raw`]). This crate intentionally implements no reverse
    /// *policy* (which targets are allowed); that belongs to the caller.
    ///
    /// Takes `&self` (via interior mutability) so it works on an `Arc`-shared
    /// connection. Single-consumer: the receiver can be taken once.
    ///
    /// The queue is bounded; on overflow the reader drops the *incoming* OPEN
    /// and logs a warning rather than blocking (a blocked reader would stall
    /// every session). Drain it promptly.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::ADBRequestFailed`] if the receiver has already
    /// been taken by a previous call (there is a single consumer), or
    /// [`RustADBError::PoisonError`] if the guard mutex was poisoned.
    pub fn incoming_opens(&self) -> Result<mpsc::Receiver<ADBTransportMessage>> {
        self.pending_opens_rx
            .lock()
            .map_err(|_| RustADBError::PoisonError)?
            .take()
            .ok_or_else(|| {
                RustADBError::ADBRequestFailed(
                    "incoming_opens: receiver already taken (single consumer only)".into(),
                )
            })
    }

    /// Subscribe to a raw, filtered copy of every inbound message (low-level
    /// primitive, committed stable public API).
    ///
    /// The reader task tees every received [`ADBTransportMessage`] for which
    /// `filter` returns `true` to the returned bounded queue — *in addition* to
    /// the message's normal session/OPEN routing. This bypasses the session
    /// registry, giving callers a raw view of the wire for relay/reverse use
    /// cases (id translation and flow control are the caller's responsibility).
    /// Multiple independent subscribers may coexist; the filter should usually
    /// be narrow to avoid teeing the whole stream.
    ///
    /// The queue is bounded; on overflow the reader drops the matched message
    /// with a warning rather than blocking. When the returned `Receiver` is
    /// dropped, the subscriber is pruned on the reader's next message.
    ///
    /// Because there is exactly one bulk-IN reader, this is implemented as a tee
    /// inside the existing reader loop — it does NOT spawn a second reader.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::SendError`] if the reader task is gone.
    pub async fn subscribe_raw(
        &self,
        filter: impl Fn(&ADBTransportMessage) -> bool + Send + 'static,
    ) -> Result<mpsc::Receiver<ADBTransportMessage>> {
        let (tx, rx) = mpsc::channel(RAW_SUBSCRIBER_CHANNEL_SIZE);
        self.control_tx
            .send(ReaderControl::Subscribe(RawSubscriber {
                filter: Box::new(filter),
                tx,
            }))
            .await
            .map_err(|_| RustADBError::SendError)?;
        Ok(rx)
    }

    /// Send a raw [`ADBTransportMessage`] over the connection (low-level
    /// primitive, committed stable public API).
    ///
    /// Enqueues the frame onto the single writer task, exactly like
    /// `open_session`. The caller is responsible for all protocol semantics (id
    /// allocation, flow control). Pairs with [`Self::subscribe_raw`] for
    /// relay/reverse use.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::SendError`] if the writer task is gone, or any
    /// error from the underlying transport write.
    pub async fn send_raw(&self, msg: ADBTransportMessage) -> Result<()> {
        self.writer
            .send_with_ack(msg)
            .await
            .map_err(RustADBError::IOError)
    }

    /// Perform CNXN+AUTH handshake on a connected transport.
    ///
    /// Returns `(device_protocol_version, device_banner)`. The banner is used to
    /// negotiate features such as `delayed_ack`; the version is taken from the
    /// CNXN response header (`arg0`) so the caller can gate `delayed_ack`
    /// windowing on the negotiated version (windowing is only valid at
    /// `>= A_VERSION_SKIP_CHECKSUM`).
    #[tracing::instrument(name = "connect", skip(transport, private_key, features))]
    async fn do_connect(
        transport: &mut USBTransport,
        private_key: &ADBRsaKey,
        features: &DeviceFeatureSet,
    ) -> Result<(u32, String)> {
        // Drain any stale messages from previous sessions on this USB pipe before
        // the handshake. An unclean prior teardown can leave several orphaned
        // streams' CLSE/WRTE frames buffered; a fresh CNXN handshake must not
        // consume them as its response.
        Self::drain_stale(transport).await;

        // Honest banner: advertise only features this end actually implements
        // (see `DeviceFeatureSet`). No trailing NUL — the real AOSP adb host
        // sends none, and a trailing NUL would corrupt the last CSV feature token
        // in adbd's no-trim parser (see `to_banner_string`).
        let banner = features.to_banner_string();

        // The CNXN wire version must agree with the `delayed_ack` feature: AOSP
        // only permits windowed flow control at `>= A_VERSION_SKIP_CHECKSUM`.
        // Advertising `delayed_ack` at the legacy version makes adbd ignore the
        // windowed OPEN (no OKAY → open_session times out), so connect at the
        // version that allows windowing iff we intend to use it.
        let cnxn_version = if features.delayed_ack {
            A_VERSION_SKIP_CHECKSUM
        } else {
            A_VERSION_LEGACY
        };

        // Try CNXN up to `CNXN_MAX_ATTEMPTS` times. After an unclean disconnect
        // adbd can have MULTIPLE orphaned streams queued, each emitting a stale
        // CLSE; the count scales with how many sessions the previous connection
        // had open, so a fixed-3 bound was too low under the multi-session server
        // path. Each stale CLSE we hit, we also re-drain before retrying so a burst
        // of buffered CLSEs is cleared in one pass rather than one-per-attempt.
        for attempt in 1..=CNXN_MAX_ATTEMPTS {
            let cnxn_msg = ADBTransportMessage::try_new(
                MessageCommand::Cnxn,
                cnxn_version,
                1_048_576,
                banner.as_bytes(),
            )?;
            transport.write_message(cnxn_msg).await?;

            let response = transport.read_message().await?;

            match response.header().command() {
                MessageCommand::Cnxn => {
                    let dev_banner = String::from_utf8_lossy(response.payload()).into_owned();
                    tracing::debug!(
                        "PersistentUsb: unencrypted connection established, device banner: {dev_banner:?}"
                    );
                    return Ok((response.header().arg0(), dev_banner));
                }
                MessageCommand::Auth => {
                    tracing::debug!("PersistentUsb: authentication required");
                    return Self::do_auth(transport, response, private_key).await;
                }
                MessageCommand::Stls => {
                    return Err(RustADBError::ADBRequestFailed(
                        "STLS not supported in persistent USB connection".into(),
                    ));
                }
                MessageCommand::Clse => {
                    // Stale CLSE from a previous session — drain any sibling stale
                    // frames buffered behind it, then retry the handshake.
                    tracing::debug!(
                        "PersistentUsb: got stale CLSE on attempt {attempt}/{CNXN_MAX_ATTEMPTS}, draining + retrying"
                    );
                    Self::drain_stale(transport).await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                _ => {
                    return Err(RustADBError::WrongResponseReceived(
                        "Expected CNXN or AUTH".into(),
                        response.header().command().to_string(),
                    ));
                }
            }
        }
        Err(RustADBError::ADBRequestFailed(format!(
            "CNXN failed after {CNXN_MAX_ATTEMPTS} attempts (stale CLSE)"
        )))
    }

    /// Drain any buffered stale frames on the USB pipe (left by a previous
    /// connection's unclean teardown) so a fresh CNXN handshake does not consume
    /// them as its response. Reads with a short per-frame timeout until the pipe
    /// goes quiet. Best-effort and bounded by `STALE_DRAIN_MAX_FRAMES` so a device
    /// that keeps chattering cannot wedge the drain forever.
    async fn drain_stale(transport: &mut USBTransport) {
        for _ in 0..STALE_DRAIN_MAX_FRAMES {
            match transport
                .read_message_with_timeout(Duration::from_millis(100))
                .await
            {
                Ok(msg) => tracing::trace!(
                    "PersistentUsb: drained stale message: cmd={}",
                    msg.header().command()
                ),
                Err(_) => return, // timeout (pipe quiet) or transient read error
            }
        }
        tracing::debug!(
            "PersistentUsb: stale-drain hit the {STALE_DRAIN_MAX_FRAMES}-frame cap; proceeding"
        );
    }

    /// Returns `(device_protocol_version, device_banner)` from the accepted CNXN
    /// response — see [`Self::do_connect`].
    #[tracing::instrument(name = "auth", skip(transport, message, private_key))]
    async fn do_auth(
        transport: &mut USBTransport,
        message: ADBTransportMessage,
        private_key: &ADBRsaKey,
    ) -> Result<(u32, String)> {
        if message.header().arg0() != AUTH_TOKEN {
            return Err(RustADBError::ADBRequestFailed(format!(
                "AUTH message with type != TOKEN ({})",
                message.header().arg0()
            )));
        }

        let sign = private_key.sign(message.into_payload())?;
        let sig_msg = ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_SIGNATURE, 0, &sign)?;
        transport.write_message(sig_msg).await?;

        let received = transport.read_message().await?;
        if received.header().command() == MessageCommand::Cnxn {
            let banner = String::from_utf8_lossy(received.payload()).into_owned();
            tracing::info!(
                "PersistentUsb: auth OK (signature accepted), device banner: {banner:?}"
            );
            return Ok((received.header().arg0(), banner));
        }

        // Send public key
        let mut pubkey = private_key.android_pubkey_encode()?.into_bytes();
        pubkey.push(b'\0');
        let pk_msg =
            ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_RSAPUBLICKEY, 0, &pubkey)?;
        transport.write_message(pk_msg).await?;

        let final_resp = transport
            .read_message_with_timeout(Duration::from_secs(10))
            .await?;
        final_resp.assert_command(MessageCommand::Cnxn)?;
        let banner = String::from_utf8_lossy(final_resp.payload()).into_owned();
        tracing::info!("PersistentUsb: auth OK (public key accepted), device banner: {banner:?}");
        Ok((final_resp.header().arg0(), banner))
    }

    /// Reader task: the single owner of the USB bulk-IN endpoint.
    ///
    /// There is exactly ONE reader task per connection — a second reader would
    /// deadlock on the IN-endpoint mutex (see `usb_transport`). So all inbound
    /// routing (session-data, session-ack, device-OPEN, raw tee) happens in this
    /// one loop. The routing decision is factored into the I/O-free
    /// [`classify_message`] so it can be unit-tested without hardware (D1).
    ///
    /// The reader OWNS the session registry and raw-subscriber list privately —
    /// no shared lock. Registry mutations arrive over `control_rx` and are
    /// applied via `select!` between reads, so the reader never holds a lock
    /// across a USB `.await`.
    ///
    /// The reader must NEVER block: blocking on a full queue would stall every
    /// session. All sends use `try_send`; on overflow we drop and `tracing::warn!`
    /// so the loss is observable (never a silent drop).
    // Not per-session: this task demuxes ALL sessions, so the span is just a
    // task label (no single `local_id`). Every routed-frame event emitted inside
    // inherits the `reader` label so interleaved lines are attributable to the task.
    #[tracing::instrument(name = "reader", skip(transport, control_rx, pending_opens_tx))]
    async fn reader_loop(
        mut transport: USBTransport,
        mut control_rx: mpsc::Receiver<ReaderControl>,
        pending_opens_tx: mpsc::Sender<ADBTransportMessage>,
    ) {
        let mut sessions: HashMap<u32, SessionChannels> = HashMap::new();
        let mut raw_subscribers: Vec<RawSubscriber> = Vec::new();

        loop {
            // Apply any pending control-channel mutations FIRST, without
            // cancelling an in-flight read. A multi-byte ADB frame read
            // (header + `data_length` payload, possibly spanning many bulk
            // transfers, plus the residual carry-over in `usb_transport`) is
            // NOT cancel-safe: dropping `read_message_with_timeout` mid-frame
            // discards the partial payload and desyncs the stream. Previously
            // the read was `select!`ed against `control_rx`, so a `Register`/
            // `Unregister` arriving mid-read (e.g. while accepting a second
            // concurrent reverse session) cancelled and corrupted a large
            // in-flight WRTE — one of two concurrent device→host bulk streams
            // would silently stall at 0 bytes. Draining control between frames
            // (the read's own 1s timeout bounds the wait) keeps each frame read
            // atomic. Registration latency is at most one frame, which is short.
            let outcome = Self::read_or_control(
                &mut transport,
                &mut control_rx,
                &mut sessions,
                &mut raw_subscribers,
            )
            .await;

            let msg = match outcome {
                ReadStep::Message(msg) => msg,
                // `ReadTimeout`: normal read timeout — the transport hit its
                // per-read deadline. `nusb` surfaces this as
                // `TransferError::Cancelled`, which the transport maps to
                // `RustADBError::UsbTimeout`. `Control`: a control message was
                // applied (registry already mutated). Both just keep looping.
                ReadStep::ReadTimeout => continue,
                ReadStep::Closed => {
                    tracing::debug!("PersistentUsb reader: control channel closed, exiting");
                    break;
                }
                ReadStep::ReadError(e) => {
                    // Distinguish a recoverable, frame-classifiable error from a fatal
                    // transport error. ONLY `InvalidIntegrity` (bad magic) is proven
                    // frame-aligned: the read path reads the fixed 24-byte header AND
                    // exactly `data_length` payload bytes BEFORE the magic check
                    // (`read_message_with_timeout`: header decode → bound check →
                    // payload read → integrity check), so when the integrity check
                    // fails the entire frame has already been consumed off the wire and
                    // the next header read is still aligned. Drop just that frame and
                    // keep serving the other multiplexed sessions.
                    //
                    // `ConversionError` is deliberately NOT recoverable: it is raised by
                    // the header decode (`TryFrom<[u8; 24]>`) — e.g. an unknown command
                    // — which runs BEFORE `data_length` is known and BEFORE the payload
                    // is read. The frame's payload bytes are therefore still pending on
                    // the wire, so skipping the frame would desync the next header read.
                    // Likewise the oversize-`data_length` bound error (an
                    // `ADBRequestFailed` returned before the payload read) leaves the
                    // refused payload on the wire. Anything not proven frame-aligned
                    // (header-decode errors, the bound error, all IO / disconnect
                    // errors) stays fatal.
                    if matches!(e, RustADBError::InvalidIntegrity(..)) {
                        tracing::warn!("PersistentUsb reader: skipping malformed frame: {e}");
                        continue;
                    }
                    tracing::warn!("PersistentUsb reader error (fatal): {e}");
                    break;
                }
            };

            tracing::trace!(
                "PersistentUsb reader: cmd={} arg0={} arg1={} payload_len={}",
                msg.header().command(),
                msg.header().arg0(),
                msg.header().arg1(),
                msg.payload().len()
            );

            // Apply any control mutations that arrived DURING the (uninterruptible)
            // frame read BEFORE classifying it. This preserves the
            // register-before-route guarantee that `open_session` /
            // `accept_device_open` rely on: they send `Register(local_id, ...)` and
            // then the device replies; that reply frame may complete this read
            // before the loop top drains control, so a `Register` could still be
            // queued here. Draining now ensures the just-registered session is in
            // `sessions` when its first frame is classified (otherwise the reply
            // would misroute to the device-OPEN queue). A `Closed` result is
            // handled on the next loop iteration after this frame is routed.
            let _ = Self::drain_control(&mut control_rx, &mut sessions, &mut raw_subscribers);

            // Tee to raw subscribers first (orthogonal to the primary route).
            Self::tee_raw(&mut raw_subscribers, &msg);

            // Primary routing decision (I/O-free, unit-testable).
            match classify_message(&msg, &sessions) {
                RouteDecision::SessionAck(id) => {
                    // OKAY → the session's ack channel. The flow-control credit
                    // (the OKAY's signed window delta) is the load-bearing part and
                    // MUST NOT be lost on a full queue, so it is accumulated into
                    // the shared `recv_credit` atomic here (lossless); the queued
                    // OKAY message then serves only as a wakeup poke for a write
                    // half that is parked waiting for credit. A dropped poke is
                    // harmless: it only happens when the queue is full, i.e. the
                    // write half is NOT parked on it (so no wakeup is owed) and
                    // will re-read the atomic on its next poll.
                    if let Some(channels) = sessions.get(&id) {
                        if let Some(delta) = parse_okay_delta(msg.payload()) {
                            if delta != 0 {
                                channels.recv_credit.fetch_add(delta, Ordering::AcqRel);
                            }
                        } else {
                            // Malformed OKAY payload (len ∉ {0,4}); AOSP drops it.
                            tracing::warn!(
                                "PersistentUsb: session {id} ignoring OKAY with invalid payload len {}",
                                msg.payload().len()
                            );
                        }
                        // Wakeup poke (best-effort); credit already banked above.
                        let _ = channels.ack_tx.try_send(msg);
                    }
                }
                RouteDecision::SessionData(id) => {
                    // WRTE / CLSE / other → the session's data channel. A CLSE is a
                    // control signal that MUST NOT be lost: set the shared `closed`
                    // flag directly (lossless) so the read half reports EOF even if
                    // the queued CLSE is dropped on a full queue. The queued CLSE is
                    // then best-effort for a timely, ordered EOF after any buffered
                    // WRTEs. A dropped WRTE is the acknowledged never-block/bounded-
                    // memory tradeoff and stays warned.
                    if let Some(channels) = sessions.get(&id) {
                        let cmd = msg.header().command();
                        let is_clse = cmd == MessageCommand::Clse;
                        if is_clse {
                            channels.closed.store(true, Ordering::Release);
                        }
                        if channels.data_tx.try_send(msg).is_err() && !is_clse {
                            // Only a dropped DATA frame is a real loss worth warning;
                            // a dropped CLSE already took effect via `closed`.
                            tracing::warn!(
                                "PersistentUsb: session {id} data queue full, dropped {cmd} message"
                            );
                        }
                    }
                }
                RouteDecision::DeviceOpen => {
                    tracing::debug!(
                        "PersistentUsb: device-originated OPEN arg0={} payload_len={}",
                        msg.header().arg0(),
                        msg.payload().len()
                    );
                    // Bounded queue, overflow policy = drop the incoming OPEN
                    // (the reader can never block on a full queue).
                    if pending_opens_tx.try_send(msg).is_err() {
                        tracing::warn!(
                            "PersistentUsb: incoming-OPEN queue full, dropped device-originated OPEN"
                        );
                    }
                }
                RouteDecision::Unknown => {
                    tracing::trace!(
                        "PersistentUsb: message for unknown session {} (cmd={}, dropping)",
                        msg.header().arg1(),
                        msg.header().command()
                    );
                }
            }
        }
        tracing::debug!("PersistentUsb reader task exiting");
    }

    /// Drain pending control-channel mutations, then read the next USB frame to
    /// completion.
    ///
    /// Control messages are applied to the registry first and non-blockingly
    /// (`try_recv` loop), so they never cancel an in-flight frame read — a frame
    /// read is NOT cancel-safe (see [`Self::reader_loop`]). The read then runs to
    /// completion under its own 1s timeout, which both bounds how long a freshly
    /// queued control message waits and lets a closed control channel be noticed
    /// between frames.
    async fn read_or_control(
        transport: &mut USBTransport,
        control_rx: &mut mpsc::Receiver<ReaderControl>,
        sessions: &mut HashMap<u32, SessionChannels>,
        raw_subscribers: &mut Vec<RawSubscriber>,
    ) -> ReadStep {
        // 1. Apply all currently-queued control mutations without awaiting.
        if matches!(
            Self::drain_control(control_rx, sessions, raw_subscribers),
            ControlDrain::Closed
        ) {
            return ReadStep::Closed;
        }

        // 2. Read one full frame to completion (atomic, never cancelled by a
        //    control message mid-frame). The 1s timeout returns control to the
        //    loop so newly-queued control mutations are applied between frames.
        match transport
            .read_message_with_timeout(Duration::from_secs(1))
            .await
        {
            Ok(msg) => ReadStep::Message(msg),
            Err(RustADBError::UsbTimeout) => ReadStep::ReadTimeout,
            Err(e) => ReadStep::ReadError(e),
        }
    }

    /// Apply every currently-queued [`ReaderControl`] mutation to the registry
    /// without awaiting (non-cancelling). Returns [`ControlDrain::Closed`] if the
    /// control channel has been disconnected (all senders dropped → the
    /// connection is shutting down).
    fn drain_control(
        control_rx: &mut mpsc::Receiver<ReaderControl>,
        sessions: &mut HashMap<u32, SessionChannels>,
        raw_subscribers: &mut Vec<RawSubscriber>,
    ) -> ControlDrain {
        loop {
            match control_rx.try_recv() {
                Ok(ReaderControl::Register(id, channels)) => {
                    sessions.insert(id, channels);
                }
                Ok(ReaderControl::Unregister(id)) => {
                    sessions.remove(&id);
                }
                Ok(ReaderControl::Subscribe(sub)) => {
                    raw_subscribers.push(sub);
                }
                Err(mpsc::error::TryRecvError::Empty) => return ControlDrain::Drained,
                Err(mpsc::error::TryRecvError::Disconnected) => return ControlDrain::Closed,
            }
        }
    }

    /// Writer task: the single owner of the USB bulk-OUT endpoint.
    ///
    /// Drains the outbound-frame queue and serializes every write. WRTE frames
    /// (`WithAck`) report their write `Result` back over a `oneshot`; OKAY /
    /// CLSE / OPEN / raw (`FireForget`) are written best-effort and logged on
    /// failure. The task exits when every sender (the connection + all session
    /// halves) has been dropped, draining the channel first.
    // Task-label span (single bulk-OUT pump for all sessions; no per-session id).
    #[tracing::instrument(name = "writer", skip(transport, writer_rx))]
    async fn writer_loop(
        mut transport: USBTransport,
        mut writer_rx: mpsc::Receiver<OutboundFrame>,
    ) {
        while let Some(frame) = writer_rx.recv().await {
            match frame {
                OutboundFrame::FireForget(msg) => {
                    if let Err(e) = transport.write_message(msg).await {
                        tracing::warn!("PersistentUsb writer: fire-and-forget write failed: {e}");
                    }
                }
                OutboundFrame::WithAck(msg, ack) => {
                    let result = transport
                        .write_message(msg)
                        .await
                        .map_err(|e| io::Error::other(e.to_string()));
                    // The receiver may have been cancelled (dropped); ignore.
                    let _ = ack.send(result);
                }
            }
        }
        tracing::debug!("PersistentUsb writer task exiting");
    }

    /// Tee a received message to every raw subscriber whose filter matches.
    /// Never blocks: on a full subscriber queue the clone is dropped with a
    /// warning. Dead (disconnected) subscribers are pruned lazily.
    fn tee_raw(raw_subscribers: &mut Vec<RawSubscriber>, msg: &ADBTransportMessage) {
        if raw_subscribers.is_empty() {
            return;
        }
        raw_subscribers.retain(|sub| {
            if (sub.filter)(msg) {
                match sub.tx.try_send(msg.clone()) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) => {
                        tracing::warn!(
                            "PersistentUsb: raw subscriber queue full, dropped {} message",
                            msg.header().command()
                        );
                        true
                    }
                    // Receiver dropped → prune this subscriber.
                    Err(TrySendError::Closed(_)) => false,
                }
            } else {
                true
            }
        });
    }

    /// Open a new multiplexed session with the given ADB command.
    ///
    /// When `delayed_ack` is negotiated for this connection, the session uses
    /// windowed flow control (32 MiB initial window, granted per AOSP semantics);
    /// otherwise it uses classic strict stop-and-wait. The async
    /// `AsyncRead`/`AsyncWrite` contract is identical in both modes (see
    /// [`MultiplexedSession`]).
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::SendError`] if a background task is gone,
    /// [`RustADBError::ADBRequestFailed`] on a missing/late OKAY, or any
    /// transport error.
    /// Best-effort unregister of a session id from the reader's session map.
    ///
    /// Used to undo a partial registration when opening a session fails; the
    /// send is fire-and-forget since the only caller is already returning an
    /// error.
    async fn unregister_session(&self, local_id: u32) {
        let _ = self
            .control_tx
            .send(ReaderControl::Unregister(local_id))
            .await;
    }

    // Per-session span: every OPEN/OKAY/CLSE/negotiation event emitted during this
    // handshake inherits `local_id`, so a `[session{local_id=...}]` `RUST_LOG`
    // filter narrows to one session. `#[instrument]` instruments the returned
    // FUTURE (async-correct: the span is entered/exited around every `.await`
    // resume, never held as a sync guard across a yield). `local_id` is generated
    // in the body, so it is declared as an empty span field here and recorded once
    // it exists. The returned `MultiplexedSession` outlives this fn; its own
    // `local_id` is carried explicitly on per-frame events in
    // `MultiplexedSession`/`SessionInner`.
    #[tracing::instrument(name = "session", skip(self, cmd), fields(local_id))]
    pub async fn open_session(&self, cmd: &ADBLocalCommand) -> Result<MultiplexedSession> {
        let local_id: u32 = {
            let mut rng = rand::rng();
            rng.random()
        };
        tracing::Span::current().record("local_id", local_id);

        // Create separate channels for data (WRTE/CLSE) and acks (OKAY).
        // `data_rx` is `mut` because we borrow it during the open handshake to
        // observe an early CLSE (OPEN rejection) before moving it into the
        // returned `MultiplexedSession`.
        let (data_tx, mut data_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);
        let (ack_tx, mut ack_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);

        // Shared lossless control signals (same Arcs the reader holds in
        // `SessionChannels` and the session keeps in `SessionInner`): the close
        // flag and the windowed-ack credit accumulator. Created BEFORE `Register`
        // so the reader posts CLSE/OKAY-credit into the very same atomics this
        // call (and the returned session) reads.
        let closed = Arc::new(AtomicBool::new(false));
        let recv_credit = Arc::new(AtomicI64::new(0));

        // Register in the reader's session map BEFORE sending OPEN (the reader
        // may respond fast). The control message is applied before any frame
        // for this id can be routed because the reader applies control messages
        // and reads from the same `select!` loop.
        self.control_tx
            .send(ReaderControl::Register(
                local_id,
                SessionChannels {
                    data_tx,
                    ack_tx,
                    closed: Arc::clone(&closed),
                    recv_credit: Arc::clone(&recv_credit),
                },
            ))
            .await
            .map_err(|_| RustADBError::SendError)?;

        // Send OPEN message (ADB protocol requires null-terminated service string)
        let mut service_bytes = cmd.to_string().into_bytes();
        if !service_bytes.ends_with(&[0]) {
            service_bytes.push(0);
        }
        // OPEN arg1 carries the opener's receive-window grant when delayed_ack is
        // negotiated; otherwise it MUST be 0. AOSP rejects a mismatch as fatal.
        let open_arg1 = if self.delayed_ack_negotiated {
            u32::try_from(INITIAL_DELAYED_ACK_BYTES).map_err(|_| RustADBError::ConversionError)?
        } else {
            0
        };
        tracing::debug!(
            "PersistentUsb: OPEN local_id={} service={:?} delayed_ack={} window_grant={}",
            local_id,
            String::from_utf8_lossy(&service_bytes),
            self.delayed_ack_negotiated,
            open_arg1
        );
        let open_msg = ADBTransportMessage::try_new(
            MessageCommand::Open,
            local_id,
            open_arg1,
            &service_bytes,
        )?;
        if self.writer.send_with_ack(open_msg).await.is_err() {
            self.unregister_session(local_id).await;
            return Err(RustADBError::SendError);
        }

        // Wait for the device's first response, racing the ACK channel (OKAY →
        // proceed) against the DATA channel (an early CLSE → OPEN rejected, fail
        // fast). Waiting only on `ack_rx` would never observe the rejection CLSE
        // (routed to `data_rx`) and the call would burn the full timeout (bug #3).
        let response = match await_open_response(&mut ack_rx, &mut data_rx).await {
            Ok(m) => m,
            Err(e) => {
                self.unregister_session(local_id).await;
                return Err(e);
            }
        };

        // Defensive: only the ack channel yields `response` here, and the reader
        // only routes OKAY frames to it — but keep the check in case routing
        // changes.
        if response.header().command() != MessageCommand::Okay {
            self.unregister_session(local_id).await;
            return Err(RustADBError::ADBRequestFailed(format!(
                "open_session: expected OKAY, got {}",
                response.header().command()
            )));
        }

        let remote_id = response.header().arg0();

        // Send-side window. As the opener (AOSP `connect_to_remote`) our own send
        // window starts at 0 when delayed_ack is on; in classic mode it stays
        // stop-and-wait (no window).
        let mut send_flow = if self.delayed_ack_negotiated {
            FlowControl::new_windowed(0)
        } else {
            FlowControl::new_classic()
        };

        // With delayed_ack, send an initial OKAY granting our own receive window
        // (32 MiB as i32 LE). Classic mode sends an empty-payload readiness OKAY.
        // adbd won't send WRTE until it gets this initial OKAY.
        let ready_payload = encode_okay_payload(
            self.delayed_ack_negotiated,
            usize::try_from(INITIAL_DELAYED_ACK_BYTES).unwrap_or(usize::MAX),
        );
        let ready_msg =
            ADBTransportMessage::try_new(MessageCommand::Okay, local_id, remote_id, &ready_payload)?;
        self.writer
            .try_send_fire_forget(ready_msg)
            .map_err(|_| RustADBError::SendError)?;

        // Seed the send window from the lossless `recv_credit` atomic. The reader
        // banks EVERY OKAY's window delta there (the handshake grant and anything
        // credited since), so a single drain here captures the device's grant —
        // NOT `on_okay_payload(response.payload())`, which would double-count what
        // the reader already added. Then clear any poke messages off the channel
        // (cosmetic; `poll_write` would otherwise consume them).
        send_flow.apply_delta(recv_credit.swap(0, Ordering::AcqRel));
        while ack_rx.try_recv().is_ok() {}

        let inner = SessionInner {
            local_id,
            remote_id,
            writer: self.writer.clone(),
            control_tx: self.control_tx.clone(),
            closed,
            recv_credit,
            conn_closed: Arc::clone(&self.conn_closed),
            windowed: self.delayed_ack_negotiated,
        };

        Ok(MultiplexedSession {
            shared: Arc::new(inner),
            data_rx,
            ack_rx,
            read_buf: Vec::new(),
            read_pos: 0,
            send_flow,
            write_state: WriteState::Idle,
        })
    }

    /// Accept a **device-initiated** `OPEN` (the acceptor role — the mirror of
    /// [`Self::open_session`]'s opener role), returning a bridgeable
    /// [`MultiplexedSession`].
    ///
    /// `open_msg` is an `A_OPEN(arg0 = device_local_id, arg1 = window|0,
    /// payload = "<dest>\0")` taken from [`Self::incoming_opens`] (e.g. a
    /// `reverse:` connection the device originated). This method does NOT send an
    /// `OPEN`; instead it:
    ///
    /// 1. takes `remote_id` from the OPEN's `arg0` (the device's local id),
    /// 2. allocates our own `local_id` and registers the session BEFORE replying
    ///    (so the device's subsequent `WRTE`/`OKAY`, which target our `local_id`
    ///    in their `arg1`, route to this session rather than back to the
    ///    incoming-OPEN queue),
    /// 3. replies `A_OKAY(arg0 = our local_id, arg1 = remote_id, payload =
    ///    32 MiB window grant when delayed_ack)`,
    /// 4. seeds the send window from the OPEN's `arg1` (the device's grant to us)
    ///    under `delayed_ack` — so we may immediately write toward the device.
    ///
    /// Arg ordering follows [`Self::open_session`]'s ready-OKAY (`arg0 = ours`,
    /// `arg1 = device's`); the reader routes by `arg1`. To *reject* an OPEN
    /// instead, the caller sends `A_CLSE(0, device_local_id)` via
    /// [`Self::send_raw`] and does not call this.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::SendError`] if a background task is gone, or a
    /// transport/encoding error while building the reply.
    #[tracing::instrument(name = "accept", skip(self, open_msg), fields(local_id))]
    pub async fn accept_device_open(
        &self,
        open_msg: &ADBTransportMessage,
    ) -> Result<MultiplexedSession> {
        // The device's local id (its socket) becomes our remote id.
        let remote_id = open_msg.header().arg0();
        // Windowing is a CONNECTION-level property: AOSP gates the OKAY payload on
        // `t->SupportsDelayedAck()` (adb.cpp send_ready), and adbd's own OKAYs to
        // us carry a 4-byte window delta whenever the connection negotiated
        // delayed_ack — regardless of this OPEN's arg1. So the session must use
        // the connection's mode (otherwise a 4-byte OKAY from adbd is rejected by
        // `on_okay_payload` and our send window is never credited → we can never
        // write toward the device). The OPEN's arg1 is only the *initial* send
        // grant (0 here = we wait for adbd's first OKAY to gain send credit, same
        // as the opener seeding from the device's first OKAY).
        let windowed = self.delayed_ack_negotiated;
        let initial_send_grant = if windowed {
            i64::from(open_msg.header().arg1())
        } else {
            0
        };

        let local_id: u32 = {
            let mut rng = rand::rng();
            rng.random()
        };
        tracing::Span::current().record("local_id", local_id);

        let (data_tx, data_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);
        let (ack_tx, ack_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);

        // Shared lossless control signals (see `SessionChannels` docs). Same Arcs
        // the reader and the returned session hold. The send window's INITIAL
        // grant comes from the OPEN's arg1 (not an OKAY), so `recv_credit` starts
        // at 0 and only accrues adbd's subsequent OKAY deltas — no double count.
        let closed = Arc::new(AtomicBool::new(false));
        let recv_credit = Arc::new(AtomicI64::new(0));

        // Register BEFORE replying so the device's follow-up frames (targeting
        // our local_id) route here, not back into the incoming-OPEN queue.
        self.control_tx
            .send(ReaderControl::Register(
                local_id,
                SessionChannels {
                    data_tx,
                    ack_tx,
                    closed: Arc::clone(&closed),
                    recv_credit: Arc::clone(&recv_credit),
                },
            ))
            .await
            .map_err(|_| RustADBError::SendError)?;

        // Our send window: in windowed mode it starts from the device's initial
        // grant (OPEN arg1, often 0) and is credited by adbd's subsequent OKAYs;
        // in classic mode it is stop-and-wait.
        let send_flow = acceptor_send_flow(windowed, initial_send_grant);

        // Reply OKAY granting the device our receive window (32 MiB under
        // delayed_ack; empty payload in classic mode). Match the device's
        // per-stream windowing (see above) so adbd does not reject the OKAY.
        let ready_payload = encode_okay_payload(
            windowed,
            usize::try_from(INITIAL_DELAYED_ACK_BYTES).unwrap_or(usize::MAX),
        );
        let ready_msg = ADBTransportMessage::try_new(
            MessageCommand::Okay,
            local_id,
            remote_id,
            &ready_payload,
        )?;
        if let Err(e) = self.writer.try_send_fire_forget(ready_msg) {
            self.unregister_session(local_id).await;
            return Err(RustADBError::IOError(e));
        }

        let inner = SessionInner {
            local_id,
            remote_id,
            writer: self.writer.clone(),
            control_tx: self.control_tx.clone(),
            closed,
            recv_credit,
            conn_closed: Arc::clone(&self.conn_closed),
            // Per-stream windowing (matches the OKAY we just sent), not the
            // connection-level flag.
            windowed,
        };

        Ok(MultiplexedSession {
            shared: Arc::new(inner),
            data_rx,
            ack_rx,
            read_buf: Vec::new(),
            read_pos: 0,
            send_flow,
            write_state: WriteState::Idle,
        })
    }

    /// Open a SYNC v1 file-transfer session multiplexed on this connection.
    ///
    /// Returns a [`SyncSession`] for `adb push`/`adb pull`. It opens a normal
    /// `sync:` session ([`Self::open_session`]) and rides the shared reader-loop
    /// demux like any other stream — so file transfer runs on the SAME
    /// authenticated USB connection as concurrent shell/tcp sessions.
    ///
    /// SYNC v2 + compression is out of scope; this is v1 only.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::open_session`].
    // No `local_id` yet at this point (it's generated inside `open_session`); a
    // labeled span here lets `open_session`'s per-session span nest under it.
    #[tracing::instrument(name = "open_sync_session", skip(self))]
    pub async fn open_sync_session(&self) -> Result<SyncSession> {
        let session = self.open_session(&ADBLocalCommand::Sync).await?;
        Ok(SyncSession::new(session))
    }

    /// Open a `shell,v2` session on this connection and decode the inner-frame
    /// protocol (separate stdout/stderr + exit code).
    ///
    /// Requests the shell-v2 service (`shell,v2,raw:<cmd>`) and returns a
    /// [`ShellV2Session`] that decodes the `[id][len][payload]` frames.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::open_session`].
    // Labeled span (the per-session `local_id` span is entered inside `open_session`).
    #[tracing::instrument(name = "open_shell_v2", skip(self))]
    pub async fn open_shell_v2(&self, cmd: &str) -> Result<ShellV2Session> {
        // Non-empty args ⇒ `ADBLocalCommand` formats the service as
        // `shell,v2,raw:<cmd>` (shell-v2), vs the empty-args `shell:<cmd>` (v1).
        let command = ADBLocalCommand::ShellCommand(cmd.to_string(), vec!["v2".to_string()]);
        let session = self.open_session(&command).await?;
        Ok(ShellV2Session::new(session))
    }

    /// Convenience: execute a shell command and return stdout + exit code.
    ///
    /// This is the v1 (raw, no inner framing) path: it cannot report an exit
    /// code (always `None`). For separated stdout/stderr and a real exit code,
    /// use [`Self::open_shell_v2`].
    pub async fn shell_exec(&self, cmd: &str) -> Result<(String, Option<u8>)> {
        use tokio::io::AsyncReadExt;

        let command = ADBLocalCommand::ShellCommand(cmd.to_string(), vec![]);
        let mut session = self.open_session(&command).await?;

        let mut output = Vec::new();
        loop {
            let mut buf = [0u8; 4096];
            match session.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(RustADBError::IOError(e)),
            }
        }

        let text = String::from_utf8_lossy(&output).to_string();
        Ok((text, None))
    }

    /// Check if the connection is still alive (reader task running).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        match &self.reader_handle {
            Some(h) => !h.is_finished(),
            None => false,
        }
    }

    /// Flush a single connection-level CLSE while the writer task is still alive,
    /// awaiting its write confirmation, and mark the connection closed.
    ///
    /// Idempotent: only the first call sends the CLSE; subsequent calls (and a
    /// later `Drop`) observe the `conn_closed` flag and do nothing. Setting the
    /// flag also suppresses every live [`SessionInner`]'s per-stream CLSE on its
    /// own `Drop` — the connection-level CLSE already told the device the whole
    /// connection (and thus every stream on it) is gone, so per-session CLSEs
    /// would only race the writer's retirement and emit spurious warnings.
    ///
    /// Shared by [`Self::shutdown`] (`&self`, for `Arc`-held connections) and
    /// [`Self::close`] (`self`, the consuming variant). This is the single source
    /// of truth for the graceful CLSE; do not duplicate it.
    async fn flush_connection_clse(&self) {
        flush_connection_clse_impl(&self.writer, &self.conn_closed).await;
    }

    /// Gracefully close the connection through a shared reference.
    ///
    /// Flushes a single connection-level CLSE (awaiting the writer's confirmation)
    /// so the device tears down every multiplexed stream cleanly, then leaves the
    /// reader/writer tasks to wind down on `Drop`. Safe to call on an
    /// `Arc<PersistentUsbConnection>` (the server backend holds connections that
    /// way) and idempotent.
    ///
    /// Prefer this over relying on `Drop`: `Drop` is best-effort and, at process
    /// teardown, the writer task is often already gone — so its fire-and-forget
    /// CLSE fails to enqueue and the device is left with orphaned streams that
    /// reject the next connection's CNXN with a stale CLSE.
    pub async fn shutdown(&self) {
        self.flush_connection_clse().await;
    }

    /// Gracefully close the connection: flushes a connection-level CLSE and
    /// aborts the background tasks after the writer drains.
    ///
    /// `Drop` does this best-effort automatically; call `close` explicitly when
    /// you want to ensure the CLSE is flushed before the connection is dropped.
    /// For an `Arc`-shared connection use [`Self::shutdown`] instead.
    pub async fn close(mut self) {
        self.flush_connection_clse().await;
        // Dropping the writer handle + control tx lets both tasks drain and exit;
        // then abort to guarantee they stop even if a read is in flight.
        if let Some(h) = self.reader_handle.take() {
            h.abort();
        }
        if let Some(h) = self.writer_handle.take() {
            h.abort();
        }
    }
}

/// Outcome of a single reader step (drain control, then read one frame).
enum ReadStep {
    Message(ADBTransportMessage),
    ReadTimeout,
    Closed,
    ReadError(RustADBError),
}

/// Result of draining the reader's control channel (non-cancelling).
#[derive(Debug, PartialEq, Eq)]
enum ControlDrain {
    /// All queued control messages applied; channel still open.
    Drained,
    /// The control channel is disconnected (connection shutting down).
    Closed,
}

impl Drop for PersistentUsbConnection {
    fn drop(&mut self) {
        // If `shutdown`/`close` already flushed a connection-level CLSE (awaiting
        // the writer), the device has been told cleanly — skip the fire-and-forget
        // CLSE entirely so we never race the retiring writer and warn spuriously.
        // Otherwise fall back to best-effort: fire-and-forget onto the writer queue
        // (we cannot `.await` in Drop). If the queue is full or the writer is gone,
        // the abort below still tears the connection down.
        if !self.conn_closed.load(Ordering::Relaxed)
            && let Ok(clse) = ADBTransportMessage::try_new(MessageCommand::Clse, 0, 0, &[])
            && let Err(e) = self.writer.try_send_fire_forget(clse)
        {
            // Best-effort contract: a full/closed writer queue means the
            // connection-level CLSE could not be delivered. The `abort` below
            // still tears the connection down, but the device may keep the
            // stream open until it times out — log it so the leak is observable.
            // (Reachable only when the connection was dropped WITHOUT a prior
            // graceful `shutdown`/`close`.)
            tracing::warn!("PersistentUsb: could not enqueue connection CLSE on drop: {e}");
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.writer_handle.take() {
            handle.abort();
        }
    }
}

/// Shared per-session context cloned into both halves: the writer handle, the
/// reader control channel (for unregistration on close), socket ids, and the
/// shared close flag. Reference-counted so the session map entry is removed and
/// the CLSE is sent exactly once when the last reference is dropped.
struct SessionInner {
    local_id: u32,
    remote_id: u32,
    writer: WriterHandle,
    control_tx: mpsc::Sender<ReaderControl>,
    /// Shared close flag. `true` once a CLSE has been observed (inbound) or sent.
    closed: Arc<AtomicBool>,
    /// Shared windowed-ack credit accumulator (same `Arc` the reader holds in
    /// [`SessionChannels::recv_credit`]). The reader `fetch_add`s each OKAY's
    /// signed window delta here so a full `ack_tx` cannot lose flow-control
    /// credit; the write half drains it (`swap(0)`) as its single credit source.
    recv_credit: Arc<AtomicI64>,
    /// Connection-level close flag (shared with [`PersistentUsbConnection`]).
    /// `true` once a connection-level CLSE has been flushed by `shutdown`/`close`.
    /// When set, this session's `Drop` skips its per-stream CLSE: the device has
    /// already been told the whole connection is closing and the writer task is
    /// being retired, so a per-session CLSE would only race teardown and warn.
    conn_closed: Arc<AtomicBool>,
    /// Whether `delayed_ack` windowing is active for this session.
    windowed: bool,
}

impl SessionInner {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn mark_closed(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// Atomically take (and zero) the accumulated windowed-ack credit the reader
    /// has posted for this session. Returns the net signed delta to apply to the
    /// send window. Single-consumer (the write half) by construction.
    fn take_recv_credit(&self) -> i64 {
        self.recv_credit.swap(0, Ordering::AcqRel)
    }
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        // If the whole connection was gracefully closed (a connection-level CLSE
        // was flushed by `shutdown`/`close`), skip the per-stream CLSE: the device
        // already knows every stream on this connection is gone and the writer
        // task is being intentionally retired. Still unregister from the reader.
        let conn_closed = self.conn_closed.load(Ordering::Relaxed);
        // Best-effort CLSE + unregister. Fire-and-forget; cannot `.await` here.
        if !conn_closed
            && !self.is_closed()
            && let Ok(clse) = ADBTransportMessage::try_new(
                MessageCommand::Clse,
                self.local_id,
                self.remote_id,
                &[],
            )
            && let Err(e) = self.writer.try_send_fire_forget(clse)
        {
            // Best-effort contract: a full/closed writer queue means this
            // session's CLSE was dropped, so the remote may leak the stream
            // until it times out. Surface it rather than swallowing silently.
            tracing::warn!(
                "PersistentUsb: could not enqueue CLSE for session {} on drop: {e}",
                self.local_id
            );
        }
        // Ask the reader to drop this session id from its registry.
        let _ = self
            .control_tx
            .try_send(ReaderControl::Unregister(self.local_id));
    }
}

/// A multiplexed session over a persistent USB connection.
///
/// Represents a single ADB stream (e.g., one shell command or one TCP
/// connection). Implements [`AsyncRead`] + [`AsyncWrite`] for use as a byte
/// stream. Writes go through the single writer task; reads come from a dedicated
/// per-session channel fed by the reader task.
pub struct MultiplexedSession {
    shared: Arc<SessionInner>,
    /// Channel for data messages (WRTE, CLSE).
    data_rx: mpsc::Receiver<ADBTransportMessage>,
    /// Channel for flow control (OKAY).
    ack_rx: mpsc::Receiver<ADBTransportMessage>,
    read_buf: Vec<u8>,
    read_pos: usize,
    /// Send-side flow control window (windowed) / stop-and-wait marker (classic).
    send_flow: FlowControl,
    /// In-flight write state (drives one WRTE through the writer task).
    write_state: WriteState,
}

/// Channels + lossless control signals for a single multiplexed session, held by
/// the reader task.
///
/// `data_tx` / `ack_tx` are bounded and may drop on overflow (the reader never
/// blocks). To make the two control signals that MUST NOT be lost survive a full
/// queue, they are carried out-of-band in shared atomics instead of (only) as
/// messages on those queues:
///
/// - `closed`: set directly by the reader the instant a CLSE is classified for
///   this session, regardless of whether the CLSE message also fits on `data_tx`.
///   The read half observes it and reports EOF even if the queued CLSE was
///   dropped. Same `Arc` as the session's [`SessionInner::closed`].
/// - `recv_credit`: the reader `fetch_add`s each OKAY's signed window delta here;
///   the write half drains it as the single source of send-window credit. A
///   full `ack_tx` therefore never loses flow-control credit — the OKAY message
///   is now only a wakeup poke. Same `Arc` as [`SessionInner::recv_credit`].
pub struct SessionChannels {
    pub data_tx: mpsc::Sender<ADBTransportMessage>,
    pub ack_tx: mpsc::Sender<ADBTransportMessage>,
    /// Shared close flag (same `Arc` as the session's `SessionInner::closed`).
    closed: Arc<AtomicBool>,
    /// Shared windowed-ack credit accumulator (same `Arc` as
    /// `SessionInner::recv_credit`). Signed; the reader adds OKAY deltas, the
    /// write half swaps it to zero and applies it to its `FlowControl`.
    recv_credit: Arc<AtomicI64>,
}

#[cfg(test)]
impl MultiplexedSession {
    /// Test-only constructor that wires a session directly to caller-supplied
    /// channels (bypassing the real USB handshake). The returned `writer_rx` is
    /// the writer-task side of the outbound-frame channel; tests drive it to
    /// emulate the writer task and observe what the session enqueues.
    fn new_for_test(
        local_id: u32,
        remote_id: u32,
        windowed: bool,
        send_flow: FlowControl,
    ) -> (
        Self,
        mpsc::Sender<ADBTransportMessage>, // data_tx (feed WRTE/CLSE to the session)
        mpsc::Sender<ADBTransportMessage>, // ack_tx  (feed OKAY/CLSE acks)
        mpsc::Receiver<OutboundFrame>,     // writer_rx (what the session sends out)
        mpsc::Receiver<ReaderControl>,     // control_rx (unregister on drop/close)
    ) {
        let (data_tx, data_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);
        let (ack_tx, ack_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);
        let (writer_tx, writer_rx) = mpsc::channel(WRITER_CHANNEL_SIZE);
        let (control_tx, control_rx) = mpsc::channel(CONTROL_CHANNEL_SIZE);
        let shared = Arc::new(SessionInner {
            local_id,
            remote_id,
            writer: WriterHandle { tx: writer_tx },
            control_tx,
            closed: Arc::new(AtomicBool::new(false)),
            recv_credit: Arc::new(AtomicI64::new(0)),
            conn_closed: Arc::new(AtomicBool::new(false)),
            windowed,
        });
        let session = Self {
            shared,
            data_rx,
            ack_rx,
            read_buf: Vec::new(),
            read_pos: 0,
            send_flow,
            write_state: WriteState::Idle,
        };
        (session, data_tx, ack_tx, writer_rx, control_rx)
    }

    /// Test-only handle to the session's connection-level `conn_closed` flag, so a
    /// test can emulate a prior graceful connection-level shutdown and assert that
    /// the session's `Drop` then suppresses its per-stream CLSE.
    fn conn_closed_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.conn_closed)
    }

    /// Test-only handle to the session's shared `recv_credit` atomic, so a test can
    /// emulate what the reader does on an inbound OKAY: bank the window delta here
    /// (lossless) *before* poking the ack channel. The write half drains this as
    /// its single credit source.
    fn recv_credit_handle(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.shared.recv_credit)
    }
}

impl MultiplexedSession {
    /// Get the local session ID.
    #[must_use]
    pub fn local_id(&self) -> u32 {
        self.shared.local_id
    }

    /// Get the remote session ID.
    #[must_use]
    pub fn remote_id(&self) -> u32 {
        self.shared.remote_id
    }

    /// Gracefully close the session: send a CLSE and mark the session closed so
    /// `Drop` does not send a duplicate. Best-effort; ignores write errors.
    pub async fn close(self) {
        if !self.shared.is_closed()
            && let Ok(clse) = ADBTransportMessage::try_new(
                MessageCommand::Clse,
                self.shared.local_id,
                self.shared.remote_id,
                &[],
            )
        {
            let _ = self.shared.writer.send_with_ack(clse).await;
            self.shared.mark_closed();
        }
        // Drop runs here: `SessionInner` sees `closed == true`, so it only
        // unregisters from the reader (no duplicate CLSE).
    }

    /// Split into independent read and write halves for concurrent use.
    /// The session map entry is cleaned up and a CLSE sent when BOTH halves are
    /// dropped (the shared [`SessionInner`] is reference-counted).
    #[must_use]
    pub fn into_split(self) -> (SessionReadHalf, SessionWriteHalf) {
        let shared = self.shared;
        let read_half = SessionReadHalf {
            shared: shared.clone(),
            data_rx: self.data_rx,
            read_buf: self.read_buf,
            read_pos: self.read_pos,
        };
        let write_half = SessionWriteHalf {
            shared,
            ack_rx: self.ack_rx,
            send_flow: self.send_flow,
            write_state: WriteState::Idle,
        };
        (read_half, write_half)
    }
}

/// Read half of a split [`MultiplexedSession`].
pub struct SessionReadHalf {
    shared: Arc<SessionInner>,
    data_rx: mpsc::Receiver<ADBTransportMessage>,
    read_buf: Vec<u8>,
    read_pos: usize,
}

/// Write half of a split [`MultiplexedSession`].
///
/// # Window accounting on drop while a WRTE is in flight
///
/// If this half is dropped while `write_state == Sending` (the WRTE bytes are
/// on the writer queue but the writer task's ack `oneshot` has not yet
/// resolved), the pending `record_sent` never runs, so this session's local
/// send-window view is left un-debited. This is intentionally accepted, not a
/// leak: the window lives entirely inside this half and is discarded with it,
/// and dropping the write half tears the whole session down (the shared
/// [`SessionInner`] still fires a best-effort CLSE on its own drop). There is no
/// cross-session impact and nothing to reconcile, so eagerly debiting here would
/// only add complexity for an accounting value that is about to be thrown away.
pub struct SessionWriteHalf {
    shared: Arc<SessionInner>,
    ack_rx: mpsc::Receiver<ADBTransportMessage>,
    send_flow: FlowControl,
    write_state: WriteState,
}

/// State of the write side's single in-flight WRTE.
///
/// `Idle` means no write is outstanding. `Sending` holds the `oneshot::Receiver`
/// for the writer task's write result plus the byte count to debit once the
/// write confirms — we poll the receiver directly (no boxed `'static` future,
/// no borrow of the session's channels), keeping the accounting on a single
/// non-await segment after the ack (P1-①, P1-③).
enum WriteState {
    Idle,
    Sending {
        ack: oneshot::Receiver<io::Result<()>>,
        chunk_size: usize,
    },
}

/// React to an ack-channel message (a wakeup *poke*) by folding in the window
/// credit the reader has banked in the shared `recv_credit` atomic.
///
/// The OKAY's window delta is NOT read from `msg` here: the reader already added
/// it to `recv_credit` losslessly (so a full ack queue cannot lose credit), and
/// the message on the channel is only a wakeup signal. We drain the atomic
/// (`take_recv_credit`) and apply the net delta — idempotent across repeated
/// calls (a second drain returns 0). A CLSE that somehow lands on the ack channel
/// still closes defensively; the reader's primary path sets `closed` directly.
fn apply_ack(
    msg: &ADBTransportMessage,
    send_flow: &mut FlowControl,
    shared: &SessionInner,
) -> io::Result<()> {
    match msg.header().command() {
        MessageCommand::Okay => {
            send_flow.apply_delta(shared.take_recv_credit());
            Ok(())
        }
        MessageCommand::Clse => {
            shared.mark_closed();
            Ok(())
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected OKAY/CLSE on ack channel, got {other}"),
        )),
    }
}

/// Drive the shared `poll_read` state machine: drain any re-buffered tail first,
/// otherwise `poll_recv` the per-session data channel, emit the crediting OKAY
/// (synchronously, no await between recv and OKAY — cancellation safety, P1-③),
/// and copy the WRTE payload into `buf`. Shared by [`MultiplexedSession`] and
/// [`SessionReadHalf`].
///
/// `poll_recv` is cancel-safe: a cancelled `poll_read` future does NOT take a
/// message off the channel, so a window credit cannot be lost in the gap between
/// receiving a WRTE and enqueueing its OKAY — those happen in one synchronous
/// step here.
fn poll_read_impl(
    cx: &mut Context<'_>,
    shared: &Arc<SessionInner>,
    data_rx: &mut mpsc::Receiver<ADBTransportMessage>,
    read_buf: &mut Vec<u8>,
    read_pos: &mut usize,
    buf: &mut ReadBuf<'_>,
) -> Poll<io::Result<()>> {
    // Return buffered data first (no new OKAY needed; it was acked on arrival).
    // This precedes the close check so a session closed mid-stream still drains
    // its already-received bytes before signalling EOF.
    if *read_pos < read_buf.len() {
        let available = &read_buf[*read_pos..];
        let to_copy = available.len().min(buf.remaining());
        buf.put_slice(&available[..to_copy]);
        *read_pos += to_copy;
        if *read_pos >= read_buf.len() {
            read_buf.clear();
            *read_pos = 0;
        }
        return Poll::Ready(Ok(()));
    }

    // Closed sessions: the reader sets the shared `closed` flag DIRECTLY on an
    // inbound CLSE (lossless — independent of whether the CLSE message also fit on
    // the bounded data queue). But a CLSE arrives AFTER any in-flight WRTEs, and
    // those WRTEs may still be sitting on the data channel, so we must NOT short-
    // circuit to EOF while data is immediately available. Deliver any ready WRTE
    // first (`try_recv`); only report EOF once the channel has no ready data.
    if shared.is_closed() {
        match data_rx.try_recv() {
            Ok(msg) => return deliver_data_msg(shared, read_buf, read_pos, buf, msg),
            // No buffered data left → genuine EOF (even if the CLSE message itself
            // was dropped on a full queue; the flag carried the close).
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                return Poll::Ready(Ok(()));
            }
        }
    }

    match data_rx.poll_recv(cx) {
        Poll::Ready(Some(msg)) => deliver_data_msg(shared, read_buf, read_pos, buf, msg),
        Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "session channel closed",
        ))),
        Poll::Pending => Poll::Pending,
    }
}

/// Handle one inbound data-channel message for the read path: a WRTE is acked
/// (crediting OKAY enqueued synchronously, P1-③) and its payload copied into
/// `buf` (overflow stashed in `read_buf`); a CLSE marks the session closed and
/// yields EOF; anything else is a protocol error. Shared by the normal
/// `poll_recv` path and the closed-session drain (`try_recv`) so both deliver
/// queued WRTEs identically before EOF.
fn deliver_data_msg(
    shared: &Arc<SessionInner>,
    read_buf: &mut Vec<u8>,
    read_pos: &mut usize,
    buf: &mut ReadBuf<'_>,
    msg: ADBTransportMessage,
) -> Poll<io::Result<()>> {
    match msg.header().command() {
        MessageCommand::Write => {
            let payload = msg.into_payload();
            let okay_payload = encode_okay_payload(shared.windowed, payload.len());
            let okay = match ADBTransportMessage::try_new(
                MessageCommand::Okay,
                shared.local_id,
                shared.remote_id,
                &okay_payload,
            ) {
                Ok(m) => m,
                Err(e) => return Poll::Ready(Err(io::Error::other(e.to_string()))),
            };
            // Synchronous, non-blocking enqueue — no await between recv and
            // OKAY (cancellation safety, P1-③).
            if let Err(e) = shared.writer.try_send_fire_forget(okay) {
                return Poll::Ready(Err(e));
            }
            if payload.is_empty() {
                // 0-byte frame: nothing to copy; the read still made progress.
                return Poll::Ready(Ok(()));
            }
            let to_copy = payload.len().min(buf.remaining());
            buf.put_slice(&payload[..to_copy]);
            if to_copy < payload.len() {
                *read_buf = payload;
                *read_pos = to_copy;
            }
            Poll::Ready(Ok(()))
        }
        MessageCommand::Clse => {
            shared.mark_closed();
            Poll::Ready(Ok(())) // EOF (0 bytes filled)
        }
        other => Poll::Ready(Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected command in data channel: {other}"),
        ))),
    }
}

/// Resolve an in-flight WRTE by polling its write-result oneshot.
///
/// The window debit (`record_sent`) lands here, AFTER the write is confirmed,
/// in one synchronous (non-await) segment (decisions P1-1 and P1-3). Returns
/// `Some(poll)` when there is an in-flight write to resolve (the caller
/// short-circuits with it), or `None` when the writer is idle and the caller
/// should proceed.
fn poll_inflight_write(
    cx: &mut Context<'_>,
    shared: &Arc<SessionInner>,
    send_flow: &mut FlowControl,
    write_state: &mut WriteState,
) -> Option<Poll<io::Result<usize>>> {
    let WriteState::Sending { ack, chunk_size } = write_state else {
        return None;
    };
    Some(match Pin::new(ack).poll(cx) {
        Poll::Ready(Ok(Ok(()))) => {
            let n = *chunk_size;
            send_flow.record_sent(n);
            *write_state = WriteState::Idle;
            tracing::trace!(
                "PersistentUsb: session {} sent WRTE size={n}, window={:?}",
                shared.local_id,
                send_flow.available_bytes()
            );
            Poll::Ready(Ok(n))
        }
        Poll::Ready(Ok(Err(e))) => {
            *write_state = WriteState::Idle;
            shared.mark_closed();
            Poll::Ready(Err(e))
        }
        Poll::Ready(Err(_canceled)) => {
            *write_state = WriteState::Idle;
            shared.mark_closed();
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "writer dropped ack",
            )))
        }
        Poll::Pending => Poll::Pending,
    })
}

/// Drive the shared `poll_write` state machine — the SINGLE send-side
/// flow-control policy, shared by both write paths.
///
/// Steps (preserving the windowed flow-control semantics of the sync impl):
/// 1. If a WRTE is already in flight, poll its `oneshot` ack; on success debit
///    the window (`record_sent`) in one synchronous segment (P1-①).
/// 2. Drain already-arrived OKAYs (non-blocking) to credit the window.
/// 3. If windowed and the window is exhausted, `poll_recv` the ack channel until
///    an OKAY credits it (registering the waker — backpressure).
/// 4. Enqueue ONE WRTE chunk with an ack `oneshot` and stash the `Sending`
///    state; poll it once so a synchronously-ready write completes immediately.
fn poll_write_impl(
    cx: &mut Context<'_>,
    shared: &Arc<SessionInner>,
    ack_rx: &mut mpsc::Receiver<ADBTransportMessage>,
    send_flow: &mut FlowControl,
    write_state: &mut WriteState,
    buf: &[u8],
) -> Poll<io::Result<usize>> {
    // 1. Resolve any in-flight WRTE first (window debit lands on confirmation).
    if let Some(poll) = poll_inflight_write(cx, shared, send_flow, write_state) {
        return poll;
    }

    if buf.is_empty() {
        return Poll::Ready(Ok(0));
    }
    if shared.is_closed() {
        return Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "session closed",
        )));
    }

    // 2. Credit the window from the lossless `recv_credit` atomic (the reader
    //    banks every OKAY's delta there even when the poke message is dropped on
    //    a full ack queue). Then drain any poke messages off the channel so it
    //    does not back up — `apply_ack` re-drains the (now-zero) atomic, so this
    //    cannot double-count. Draining the channel also surfaces a CLSE that
    //    landed on the ack side and a disconnected channel.
    send_flow.apply_delta(shared.take_recv_credit());
    loop {
        match ack_rx.try_recv() {
            Ok(msg) => apply_ack(&msg, send_flow, shared)?,
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "session ack channel closed",
                )));
            }
        }
    }
    if shared.is_closed() {
        return Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "session closed by remote",
        )));
    }

    // 3. Windowed backpressure: if the window is exhausted, register for a wake
    //    when an OKAY arrives. `poll_recv` registers the waker on Pending.
    //
    //    Credit lives in the lossless `recv_credit` atomic, so before parking we
    //    always re-drain it: an OKAY may have banked credit while dropping its
    //    poke on a full ack queue (then no message wakes us), or landed in the
    //    gap after step 2. Re-checking the atomic on each iteration — and again
    //    right before returning Pending — guarantees we never sleep holding
    //    credit. A dropped poke is safe precisely because it only happens when the
    //    queue is non-empty (so `poll_recv` returns a message and we loop) or the
    //    atomic already carries the credit we just folded in.
    if send_flow.is_windowed() {
        while !send_flow.can_send() {
            match ack_rx.poll_recv(cx) {
                Poll::Ready(Some(msg)) => {
                    apply_ack(&msg, send_flow, shared)?;
                    if shared.is_closed() {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "session closed by remote",
                        )));
                    }
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "session ack channel closed",
                    )));
                }
                Poll::Pending => {
                    // No poke pending. Fold any credit banked since the last drain;
                    // if it now lets us send, loop without parking. Otherwise park
                    // (the waker is registered by the poll_recv above).
                    send_flow.apply_delta(shared.take_recv_credit());
                    if send_flow.can_send() {
                        continue;
                    }
                    return Poll::Pending;
                }
            }
        }
    }

    // 4. Enqueue ONE chunk (clamped to MAX_PAYLOAD) with a write-result oneshot.
    let chunk_size = buf.len().min(MAX_PAYLOAD);
    let msg = match ADBTransportMessage::try_new(
        MessageCommand::Write,
        shared.local_id,
        shared.remote_id,
        &buf[..chunk_size],
    ) {
        Ok(m) => m,
        Err(e) => return Poll::Ready(Err(io::Error::other(e.to_string()))),
    };
    let (tx, rx) = oneshot::channel();
    // Non-blocking enqueue onto the writer task. A full writer queue surfaces as
    // backpressure (`WouldBlock`), which `poll_write` callers re-poll on wake.
    match shared.writer.tx.try_send(OutboundFrame::WithAck(msg, tx)) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            // Writer queue full; re-poll later. Wake immediately so the task is
            // re-scheduled (the writer drains quickly under the single-writer
            // model). This mirrors the reader's never-block discipline.
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Err(TrySendError::Closed(_)) => {
            shared.mark_closed();
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "writer task gone",
            )));
        }
    }
    *write_state = WriteState::Sending {
        ack: rx,
        chunk_size,
    };

    // Poll the just-created ack once so a synchronously-ready write completes now.
    poll_inflight_write(cx, shared, send_flow, write_state).unwrap_or(Poll::Pending)
}

impl AsyncRead for SessionReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        poll_read_impl(
            cx,
            &this.shared,
            &mut this.data_rx,
            &mut this.read_buf,
            &mut this.read_pos,
            buf,
        )
    }
}

impl AsyncWrite for SessionWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        poll_write_impl(
            cx,
            &this.shared,
            &mut this.ack_rx,
            &mut this.send_flow,
            &mut this.write_state,
            buf,
        )
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for MultiplexedSession {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        poll_read_impl(
            cx,
            &this.shared,
            &mut this.data_rx,
            &mut this.read_buf,
            &mut this.read_pos,
            buf,
        )
    }
}

impl AsyncWrite for MultiplexedSession {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        poll_write_impl(
            cx,
            &this.shared,
            &mut this.ack_rx,
            &mut this.send_flow,
            &mut this.write_state,
            buf,
        )
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a session registry containing the given local ids (with dummy,
    /// unused channels) for classification tests.
    fn sessions_with(ids: &[u32]) -> HashMap<u32, SessionChannels> {
        let mut map = HashMap::new();
        for &id in ids {
            // `classify_message` only inspects key presence, so the dropped
            // receivers are irrelevant here.
            let (data_tx, _) = mpsc::channel(1);
            let (ack_tx, _) = mpsc::channel(1);
            map.insert(
                id,
                SessionChannels {
                    data_tx,
                    ack_tx,
                    closed: Arc::new(AtomicBool::new(false)),
                    recv_credit: Arc::new(AtomicI64::new(0)),
                },
            );
        }
        map
    }

    fn msg(command: MessageCommand, arg0: u32, arg1: u32, payload: &[u8]) -> ADBTransportMessage {
        ADBTransportMessage::try_new(command, arg0, arg1, payload).expect("build message")
    }

    #[test]
    fn wrte_to_known_session_routes_to_data() {
        let sessions = sessions_with(&[42]);
        let m = msg(MessageCommand::Write, 7, 42, b"hello");
        assert_eq!(
            classify_message(&m, &sessions),
            RouteDecision::SessionData(42),
            "WRTE addressed to a known session must route to its data channel"
        );
    }

    #[test]
    fn okay_to_known_session_routes_to_ack() {
        let sessions = sessions_with(&[42]);
        let m = msg(MessageCommand::Okay, 7, 42, &[]);
        assert_eq!(
            classify_message(&m, &sessions),
            RouteDecision::SessionAck(42),
            "OKAY addressed to a known session must route to its ack channel"
        );
    }

    #[test]
    fn clse_to_known_session_routes_to_data() {
        let sessions = sessions_with(&[42]);
        let m = msg(MessageCommand::Clse, 7, 42, &[]);
        assert_eq!(
            classify_message(&m, &sessions),
            RouteDecision::SessionData(42),
            "CLSE for a known session is a data-channel event, not an ack"
        );
    }

    #[test]
    fn device_originated_open_routes_to_pending_opens() {
        // Device-originated OPEN: arg1 == 0 (no host local id yet), unknown.
        let sessions = sessions_with(&[42]);
        let m = msg(MessageCommand::Open, 99, 0, b"tcp:1234\0");
        assert_eq!(
            classify_message(&m, &sessions),
            RouteDecision::DeviceOpen,
            "an OPEN for an unregistered local id must surface as a device-originated OPEN"
        );
    }

    #[test]
    fn unknown_non_open_message_is_dropped() {
        let sessions = sessions_with(&[42]);
        // WRTE to a session id we don't know about → dropped.
        let m = msg(MessageCommand::Write, 7, 12345, b"data");
        assert_eq!(
            classify_message(&m, &sessions),
            RouteDecision::Unknown,
            "non-OPEN message to an unknown session id must be dropped"
        );
    }

    #[test]
    fn acceptor_send_flow_windowed_seeds_from_initial_grant() {
        // Windowed (connection delayed_ack) seeds the send window from the OPEN's
        // arg1 initial grant; adbd's later OKAYs credit it further.
        let grant = 32 * 1024 * 1024i64;
        let fc = acceptor_send_flow(true, grant);
        assert!(
            fc.is_windowed(),
            "delayed_ack must produce a windowed controller"
        );
        assert_eq!(
            fc.available_bytes(),
            Some(grant),
            "acceptor send window must seed from the OPEN arg1 initial grant"
        );
    }

    #[test]
    fn acceptor_send_flow_windowed_zero_initial_grant() {
        // The common reverse case: OPEN arg1=0 → windowed but 0 initial credit,
        // waiting for adbd's first OKAY to gain send credit.
        let fc = acceptor_send_flow(true, 0);
        assert!(fc.is_windowed(), "still windowed at the connection level");
        assert_eq!(fc.available_bytes(), Some(0), "0 initial send credit");
    }

    #[test]
    fn acceptor_send_flow_classic_is_stop_and_wait() {
        // Without delayed_ack the grant is ignored and the controller is classic.
        let fc = acceptor_send_flow(false, 0);
        assert!(!fc.is_windowed(), "classic mode must not be windowed");
        assert_eq!(fc.available_bytes(), None, "classic mode tracks no window");
    }

    #[tokio::test]
    async fn open_response_okay_on_ack_channel_succeeds() {
        // Classic / windowed success path: device sends OKAY → it lands on the
        // ack channel → await_open_response returns the OKAY message.
        let (ack_tx, mut ack_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);
        let (_data_tx, mut data_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);
        ack_tx
            .send(msg(MessageCommand::Okay, 7, 42, &[]))
            .await
            .expect("send OKAY");

        let resp = await_open_response(&mut ack_rx, &mut data_rx)
            .await
            .expect("OKAY on ack channel must succeed");
        assert_eq!(
            resp.header().command(),
            MessageCommand::Okay,
            "the ack-channel OKAY must be returned to proceed with the open"
        );
    }

    #[tokio::test]
    async fn open_response_clse_on_data_channel_fails_fast() {
        // Bug #3: OPEN rejection arrives as A_CLSE(arg0=0, arg1=local_id) on the
        // DATA channel (command != OKAY). await_open_response must fail fast with
        // the rejection error instead of waiting for an OKAY that never comes.
        let (_ack_tx, mut ack_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);
        let (data_tx, mut data_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);
        // arg0 = 0 exactly as AOSP send_close(0, ...) does.
        data_tx
            .send(msg(MessageCommand::Clse, 0, 42, &[]))
            .await
            .expect("send CLSE");

        let err = await_open_response(&mut ack_rx, &mut data_rx)
            .await
            .expect_err("CLSE on data channel must fail the open");
        match err {
            RustADBError::ADBRequestFailed(m) => assert!(
                m.contains("OPEN rejected by device (CLSE)"),
                "rejection error must be distinct and actionable, got: {m}"
            ),
            other => panic!("expected ADBRequestFailed rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn raw_tee_delivers_only_matching_messages() {
        let mut subscribers: Vec<RawSubscriber> = Vec::new();
        // Subscriber matches only WRTE messages.
        let (tx, mut rx) = mpsc::channel(RAW_SUBSCRIBER_CHANNEL_SIZE);
        subscribers.push(RawSubscriber {
            filter: Box::new(|m| m.header().command() == MessageCommand::Write),
            tx,
        });

        let wrte = msg(MessageCommand::Write, 1, 2, b"payload");
        let okay = msg(MessageCommand::Okay, 1, 2, &[]);

        PersistentUsbConnection::tee_raw(&mut subscribers, &wrte);
        PersistentUsbConnection::tee_raw(&mut subscribers, &okay);

        let received = rx.try_recv().expect("matching WRTE should be tee'd");
        assert_eq!(
            received.header().command(),
            MessageCommand::Write,
            "tee must deliver the matching message"
        );
        assert!(
            rx.try_recv().is_err(),
            "non-matching OKAY must not be tee'd to the subscriber"
        );
    }

    #[test]
    fn banner_with_delayed_ack_is_detected() {
        let banner = "device::ro.product.name=sdk;features=shell_v2,cmd,stat_v2,delayed_ack,abb\0";
        assert!(
            banner_advertises_delayed_ack(banner),
            "a banner whose features= list contains delayed_ack must be detected"
        );
    }

    #[test]
    fn banner_without_delayed_ack_is_not_detected() {
        let banner = "device::ro.product.name=sdk;features=shell_v2,cmd,stat_v2\0";
        assert!(
            !banner_advertises_delayed_ack(banner),
            "a banner without delayed_ack in features= must not be detected"
        );
    }

    #[test]
    fn banner_substring_does_not_false_match() {
        // A feature literally named with delayed_ack as a substring must not match
        // (we split on commas and compare whole tokens).
        let banner = "device::features=delayed_ack_extended,cmd\0";
        assert!(
            !banner_advertises_delayed_ack(banner),
            "a token that merely contains delayed_ack as a prefix must not match"
        );
    }

    #[test]
    fn banner_no_features_segment_is_not_detected() {
        assert!(
            !banner_advertises_delayed_ack("device::ro.product.name=sdk\0"),
            "a banner with no features= segment must not be detected"
        );
    }

    // A banner that advertises delayed_ack (capable device).
    const BANNER_WITH_DELAYED_ACK: &str =
        "device::ro.product.name=test;features=shell_v2,cmd,delayed_ack";
    // A banner that does not advertise delayed_ack (e.g. Android 11).
    const BANNER_WITHOUT_DELAYED_ACK: &str = "device::ro.product.name=test;features=shell_v2,cmd";

    #[test]
    fn negotiate_delayed_ack_android16_capable_is_enabled() {
        // Both ends advertise delayed_ack and the wire version is at the gate.
        assert!(
            negotiate_delayed_ack(true, BANNER_WITH_DELAYED_ACK, A_VERSION_SKIP_CHECKSUM),
            "delayed_ack must be enabled when local+banner advertise it and version >= A_VERSION_SKIP_CHECKSUM"
        );
    }

    #[test]
    fn negotiate_delayed_ack_legacy_version_is_disabled() {
        // Regression lock: device advertises delayed_ack but the negotiated wire
        // version is legacy. Enabling windowing here makes adbd ignore the
        // windowed OPEN (no OKAY → open_session times out). MUST stay false.
        assert!(
            !negotiate_delayed_ack(true, BANNER_WITH_DELAYED_ACK, A_VERSION_LEGACY),
            "delayed_ack must NOT be enabled at A_VERSION_LEGACY even if the banner advertises it"
        );
    }

    #[test]
    fn negotiate_delayed_ack_no_banner_feature_is_disabled() {
        // Android 11: capable wire version but the banner lacks delayed_ack.
        assert!(
            !negotiate_delayed_ack(true, BANNER_WITHOUT_DELAYED_ACK, A_VERSION_SKIP_CHECKSUM),
            "delayed_ack must NOT be enabled when the device banner does not advertise it"
        );
    }

    #[test]
    fn negotiate_delayed_ack_local_opt_out_is_disabled() {
        // Local end opted out even though the device is fully capable.
        assert!(
            !negotiate_delayed_ack(false, BANNER_WITH_DELAYED_ACK, A_VERSION_SKIP_CHECKSUM),
            "delayed_ack must NOT be enabled when the local end did not advertise it"
        );
    }

    #[test]
    fn negotiate_delayed_ack_above_threshold_is_enabled() {
        // A version strictly above the threshold with a capable banner.
        assert!(
            negotiate_delayed_ack(true, BANNER_WITH_DELAYED_ACK, A_VERSION_SKIP_CHECKSUM + 1),
            "delayed_ack must be enabled for any version >= A_VERSION_SKIP_CHECKSUM with a capable banner"
        );
    }

    #[tokio::test]
    async fn raw_tee_prunes_disconnected_subscribers() {
        let mut subscribers: Vec<RawSubscriber> = Vec::new();
        let (tx, rx) = mpsc::channel(RAW_SUBSCRIBER_CHANNEL_SIZE);
        subscribers.push(RawSubscriber {
            filter: Box::new(|_| true),
            tx,
        });
        // Drop the receiver → subscriber is now disconnected.
        drop(rx);

        let m = msg(MessageCommand::Write, 1, 2, b"x");
        PersistentUsbConnection::tee_raw(&mut subscribers, &m);

        assert!(
            subscribers.is_empty(),
            "tee must prune a subscriber whose receiver was dropped"
        );
    }

    // --- Async session-stream integration tests (in-memory, no USB) ----------
    //
    // These exercise the writer-task contract, the read-side OKAY emission,
    // windowed backpressure, cancellation safety, and teardown by wiring a
    // `MultiplexedSession` directly to channels via `new_for_test` and emulating
    // the writer task in the test.

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A stand-in for the writer task: receive one outbound frame, reply `Ok` on
    /// any attached ack oneshot, and return the frame for assertions.
    async fn pump_writer(rx: &mut mpsc::Receiver<OutboundFrame>) -> ADBTransportMessage {
        match rx.recv().await.expect("writer frame") {
            OutboundFrame::FireForget(m) => m,
            OutboundFrame::WithAck(m, ack) => {
                let _ = ack.send(Ok(()));
                m
            }
        }
    }

    #[tokio::test]
    async fn read_emits_okay_and_delivers_payload() {
        let (mut session, data_tx, _ack_tx, mut writer_rx, _ctl) =
            MultiplexedSession::new_for_test(10, 20, true, FlowControl::new_windowed(0));

        // Reader task feeds one WRTE into the data channel.
        data_tx
            .send(msg(MessageCommand::Write, 20, 10, b"hello"))
            .await
            .expect("send WRTE");

        let mut buf = [0u8; 5];
        session.read_exact(&mut buf).await.expect("read payload");
        assert_eq!(
            &buf, b"hello",
            "WRTE payload must be delivered to the reader"
        );

        // The session must have enqueued the crediting OKAY (windowed: 5 as i32 LE).
        let okay = writer_rx.recv().await.expect("OKAY enqueued");
        match okay {
            OutboundFrame::FireForget(m) => {
                assert_eq!(
                    m.header().command(),
                    MessageCommand::Okay,
                    "credit is an OKAY"
                );
                assert_eq!(
                    m.payload(),
                    &5_i32.to_le_bytes(),
                    "windowed OKAY payload must be the delivered byte count (i32 LE)"
                );
            }
            OutboundFrame::WithAck(..) => panic!("OKAY must be fire-and-forget"),
        }
    }

    #[tokio::test]
    async fn write_goes_through_writer_task_with_ack() {
        // Windowed with a credited window so the write is not blocked.
        let (mut session, _data_tx, _ack_tx, mut writer_rx, _ctl) =
            MultiplexedSession::new_for_test(
                10,
                20,
                true,
                FlowControl::new_windowed(INITIAL_DELAYED_ACK_BYTES),
            );

        // Drive the write and the writer concurrently: poll_write enqueues a
        // WithAck frame and awaits its oneshot; the emulated writer replies Ok.
        let writer = tokio::spawn(async move {
            let frame = pump_writer(&mut writer_rx).await;
            assert_eq!(
                frame.header().command(),
                MessageCommand::Write,
                "WRTE on the wire"
            );
            assert_eq!(frame.payload(), b"data", "payload must match");
        });

        session.write_all(b"data").await.expect("write succeeds");
        session.flush().await.expect("flush");
        writer.await.expect("writer task");
    }

    #[tokio::test]
    async fn write_blocks_until_window_credited_then_proceeds() {
        // Opener window starts at 0 → first write must block until an OKAY
        // credits the send window.
        let (mut session, _data_tx, ack_tx, mut writer_rx, _ctl) =
            MultiplexedSession::new_for_test(10, 20, true, FlowControl::new_windowed(0));
        // Grab the shared credit atomic before the session moves into the task —
        // crediting now flows through it (the OKAY message is only a wakeup poke).
        let recv_credit = session.recv_credit_handle();

        let write = tokio::spawn(async move {
            session.write_all(b"abc").await.expect("write after credit");
            session
        });

        // The write should be parked on the exhausted window — no frame yet.
        tokio::task::yield_now().await;
        assert!(
            writer_rx.try_recv().is_err(),
            "no WRTE must be enqueued while the send window is exhausted"
        );

        // Credit the window the way the reader does: bank the signed delta in the
        // lossless atomic FIRST, then send the OKAY poke to wake the parked writer.
        recv_credit.fetch_add(1024, Ordering::AcqRel);
        ack_tx
            .send(msg(MessageCommand::Okay, 20, 10, &1024_i32.to_le_bytes()))
            .await
            .expect("send OKAY credit");

        // Now the writer task should see the WRTE; reply Ok so the write returns.
        let frame = pump_writer(&mut writer_rx).await;
        assert_eq!(
            frame.payload(),
            b"abc",
            "credited write must flush the chunk"
        );
        let _session = write.await.expect("write task completes");
    }

    #[tokio::test]
    async fn read_returns_eof_on_clse() {
        let (mut session, data_tx, _ack_tx, _writer_rx, _ctl) =
            MultiplexedSession::new_for_test(10, 20, false, FlowControl::new_classic());

        data_tx
            .send(msg(MessageCommand::Clse, 20, 10, &[]))
            .await
            .expect("send CLSE");

        let mut buf = [0u8; 8];
        let n = session.read(&mut buf).await.expect("read after CLSE");
        assert_eq!(n, 0, "a CLSE must surface as EOF (0 bytes)");
    }

    #[tokio::test]
    async fn drop_without_close_enqueues_clse_and_unregisters() {
        let (session, _data_tx, _ack_tx, mut writer_rx, mut control_rx) =
            MultiplexedSession::new_for_test(10, 20, false, FlowControl::new_classic());

        drop(session);

        // Best-effort Drop must enqueue a CLSE fire-and-forget...
        let frame = writer_rx.recv().await.expect("CLSE enqueued on drop");
        match frame {
            OutboundFrame::FireForget(m) => {
                assert_eq!(
                    m.header().command(),
                    MessageCommand::Clse,
                    "drop sends CLSE"
                );
            }
            OutboundFrame::WithAck(..) => panic!("drop CLSE must be fire-and-forget"),
        }
        // ...and ask the reader to unregister the session id.
        match control_rx
            .recv()
            .await
            .expect("unregister enqueued on drop")
        {
            ReaderControl::Unregister(id) => assert_eq!(id, 10, "drop unregisters the local id"),
            _ => panic!("drop must enqueue an Unregister"),
        }
    }

    #[tokio::test]
    async fn close_sends_clse_then_drop_does_not_duplicate() {
        let (session, _data_tx, _ack_tx, mut writer_rx, mut control_rx) =
            MultiplexedSession::new_for_test(10, 20, false, FlowControl::new_classic());

        // Emulate the writer replying to the close's WithAck CLSE.
        let writer = tokio::spawn(async move {
            let frame = pump_writer(&mut writer_rx).await;
            assert_eq!(
                frame.header().command(),
                MessageCommand::Clse,
                "close sends CLSE"
            );
            // No further frame must arrive (Drop must not duplicate the CLSE).
            assert!(
                writer_rx.try_recv().is_err(),
                "graceful close must not be followed by a duplicate CLSE from Drop"
            );
        });

        session.close().await;
        writer.await.expect("writer task");

        // Drop still unregisters the session id (idempotent cleanup).
        match control_rx.recv().await.expect("unregister enqueued") {
            ReaderControl::Unregister(id) => assert_eq!(id, 10),
            _ => panic!("close+drop must unregister"),
        }
    }

    /// PR1: once a connection-level CLSE has been flushed (graceful
    /// `shutdown`/`close` set `conn_closed`), a session's `Drop` must NOT enqueue
    /// its own per-stream CLSE — the device already knows the whole connection is
    /// gone and the writer task is being retired. It must still unregister.
    #[tokio::test]
    async fn drop_after_connection_closed_suppresses_per_stream_clse() {
        let (session, _data_tx, _ack_tx, mut writer_rx, mut control_rx) =
            MultiplexedSession::new_for_test(10, 20, false, FlowControl::new_classic());

        // Emulate a prior graceful connection-level shutdown.
        session.conn_closed_flag().store(true, Ordering::Relaxed);

        drop(session);

        // No CLSE must be enqueued (the connection-level CLSE already covered it).
        assert!(
            writer_rx.try_recv().is_err(),
            "session Drop must not enqueue a per-stream CLSE after a connection-level close"
        );
        // ...but the reader must still be asked to unregister the session id.
        match control_rx
            .recv()
            .await
            .expect("unregister still enqueued on drop")
        {
            ReaderControl::Unregister(id) => assert_eq!(id, 10, "drop still unregisters the id"),
            _ => panic!("drop must still enqueue an Unregister"),
        }
    }

    /// PR1: `flush_connection_clse` (the shared core of `shutdown`/`close`) sends
    /// exactly one connection-level CLSE with an ack, sets `conn_closed`, and is
    /// idempotent — a second call enqueues nothing more.
    #[tokio::test]
    async fn flush_connection_clse_sends_once_and_is_idempotent() {
        // A bare connection wired to a writer channel we can observe. We only need
        // the writer handle + the conn_closed flag, so build the minimum.
        let (writer_tx, mut writer_rx) = mpsc::channel(WRITER_CHANNEL_SIZE);
        let conn_closed = Arc::new(AtomicBool::new(false));

        // Emulate the writer task: ack the first frame so send_with_ack completes.
        let pump = tokio::spawn(async move {
            let mut frames = Vec::new();
            // First (and only expected) frame: the connection-level CLSE.
            if let Some(frame) = writer_rx.recv().await {
                match frame {
                    OutboundFrame::WithAck(m, ack) => {
                        let _ = ack.send(Ok(()));
                        frames.push(m);
                    }
                    OutboundFrame::FireForget(m) => frames.push(m),
                }
            }
            // Drain anything else (there must be none) without blocking forever.
            while let Ok(extra) = writer_rx.try_recv() {
                match extra {
                    OutboundFrame::WithAck(m, ack) => {
                        let _ = ack.send(Ok(()));
                        frames.push(m);
                    }
                    OutboundFrame::FireForget(m) => frames.push(m),
                }
            }
            frames
        });

        // Drive the shared flush helper twice against a bare writer channel.
        let writer = WriterHandle { tx: writer_tx };
        flush_connection_clse_impl(&writer, &conn_closed).await;
        assert!(
            conn_closed.load(Ordering::Relaxed),
            "flush must set conn_closed"
        );
        // Second call: idempotent, enqueues nothing.
        flush_connection_clse_impl(&writer, &conn_closed).await;

        drop(writer); // close the channel so the pump's loop can end
        let frames = pump.await.expect("writer pump");
        assert_eq!(frames.len(), 1, "exactly one connection-level CLSE");
        assert_eq!(
            frames[0].header().command(),
            MessageCommand::Clse,
            "the flushed frame is a CLSE"
        );
        assert_eq!(frames[0].header().arg0(), 0, "connection CLSE has arg0=0");
        assert_eq!(frames[0].header().arg1(), 0, "connection CLSE has arg1=0");
    }

    #[tokio::test]
    async fn cancelled_read_does_not_lose_the_frame_or_its_credit() {
        // Cancellation safety: a `read` future cancelled before any WRTE arrives
        // must not consume a later WRTE — and once it does arrive, the OKAY is
        // still emitted exactly once.
        let (mut session, data_tx, _ack_tx, mut writer_rx, _ctl) =
            MultiplexedSession::new_for_test(10, 20, true, FlowControl::new_windowed(0));

        // Start a read with nothing queued, then cancel it via a timeout.
        {
            let mut buf = [0u8; 4];
            let r = tokio::time::timeout(Duration::from_millis(20), session.read(&mut buf)).await;
            assert!(r.is_err(), "read must still be pending (and is cancelled)");
        }
        // No OKAY may have been emitted yet (no WRTE was delivered).
        assert!(
            writer_rx.try_recv().is_err(),
            "a cancelled read with no data must not emit a spurious OKAY"
        );

        // Now deliver a WRTE; the next read must succeed and emit exactly one OKAY.
        data_tx
            .send(msg(MessageCommand::Write, 20, 10, b"data"))
            .await
            .expect("send WRTE");
        let mut buf = [0u8; 4];
        session
            .read_exact(&mut buf)
            .await
            .expect("read after cancel");
        assert_eq!(&buf, b"data", "the WRTE survives the earlier cancellation");

        let okay = writer_rx.recv().await.expect("OKAY emitted once");
        match okay {
            OutboundFrame::FireForget(m) => {
                assert_eq!(m.header().command(), MessageCommand::Okay);
                assert_eq!(m.payload(), &4_i32.to_le_bytes(), "credit = 4 bytes");
            }
            OutboundFrame::WithAck(..) => panic!("OKAY is fire-and-forget"),
        }
        assert!(
            writer_rx.try_recv().is_err(),
            "exactly one OKAY must be emitted for one delivered WRTE (no double-credit)"
        );
    }

    #[tokio::test]
    async fn write_after_remote_close_fails_with_broken_pipe() {
        let (mut session, _data_tx, ack_tx, _writer_rx, _ctl) = MultiplexedSession::new_for_test(
            10,
            20,
            true,
            FlowControl::new_windowed(INITIAL_DELAYED_ACK_BYTES),
        );

        // Remote closes the stream via a CLSE on the ack channel.
        ack_tx
            .send(msg(MessageCommand::Clse, 20, 10, &[]))
            .await
            .expect("send remote CLSE");

        let err = session
            .write_all(b"data")
            .await
            .expect_err("write after remote close must fail");
        assert_eq!(
            err.kind(),
            io::ErrorKind::BrokenPipe,
            "a write to a remotely-closed session must surface as BrokenPipe"
        );
    }

    #[tokio::test]
    async fn drop_write_half_while_wrte_in_flight_is_clean() {
        // L1 regression: dropping the write half while a WRTE is still in flight
        // (enqueued, ack oneshot unresolved → `WriteState::Sending`) must not
        // panic and must still tear the session down (CLSE on the last shared
        // ref drop). The un-debited window is accepted: it dies with the half.
        let (session, _data_tx, _ack_tx, mut writer_rx, mut control_rx) =
            MultiplexedSession::new_for_test(
                10,
                20,
                true,
                FlowControl::new_windowed(INITIAL_DELAYED_ACK_BYTES),
            );
        let (read_half, write_half) = session.into_split();

        // Drive a write far enough to enqueue the WRTE and park in `Sending`
        // (the writer never replies on the ack oneshot, so it stays pending).
        let mut write_half = write_half;
        let writer = tokio::spawn(async move {
            // This write will park (Sending) and never complete; the task is
            // aborted when we drop our handle below.
            let _ = write_half.write_all(b"inflight").await;
            write_half
        });

        // Let the write enqueue its WRTE frame onto the writer queue.
        let frame = writer_rx.recv().await.expect("WRTE enqueued");
        match frame {
            OutboundFrame::WithAck(m, _ack) => {
                assert_eq!(
                    m.header().command(),
                    MessageCommand::Write,
                    "the in-flight frame must be a WRTE"
                );
                // Drop `_ack` here WITHOUT replying: the write stays in `Sending`.
            }
            OutboundFrame::FireForget(_) => panic!("a WRTE must be WithAck"),
        }

        // Abort the parked write task and reclaim the (still-`Sending`) half,
        // then drop it together with the read half.
        writer.abort();
        let _ = writer.await; // joins the aborted task (Err(cancelled)); no panic.
        drop(read_half);
        // `write_half` was moved into the aborted task and dropped there; the
        // last `SessionInner` ref now goes away, firing the best-effort CLSE.

        let clse = writer_rx.recv().await.expect("CLSE enqueued on full drop");
        match clse {
            OutboundFrame::FireForget(m) => assert_eq!(
                m.header().command(),
                MessageCommand::Clse,
                "dropping both halves must still send a CLSE"
            ),
            OutboundFrame::WithAck(..) => panic!("drop CLSE must be fire-and-forget"),
        }
        match control_rx.recv().await.expect("unregister on drop") {
            ReaderControl::Unregister(id) => assert_eq!(id, 10, "drop unregisters the local id"),
            _ => panic!("drop must unregister the session id"),
        }
    }

    /// PR3 (C1): when the reader classifies a CLSE it sets the shared `closed`
    /// flag DIRECTLY (lossless), so even if the bounded data queue is full and the
    /// CLSE message itself is dropped, the read half still reports EOF. This test
    /// emulates a full data queue: it sets `closed` without delivering any CLSE
    /// message and asserts the read returns EOF (0 bytes), and that any buffered
    /// data delivered first is still returned before EOF.
    #[tokio::test]
    async fn clse_closes_session_via_flag_even_if_data_queue_dropped_it() {
        let (mut session, data_tx, _ack_tx, _writer_rx, _ctl) =
            MultiplexedSession::new_for_test(10, 20, false, FlowControl::new_classic());

        // The reader banks a buffered WRTE that DID fit, then the CLSE that did
        // NOT fit (queue full) — modeled by setting `closed` without a CLSE msg.
        data_tx
            .send(msg(MessageCommand::Write, 20, 10, b"tail"))
            .await
            .expect("buffered WRTE fits");
        session.shared.closed.store(true, Ordering::Release);

        // Buffered data must still be delivered before EOF (no data abandoned).
        let mut buf = [0u8; 8];
        let n = session.read(&mut buf).await.expect("read buffered tail");
        assert_eq!(&buf[..n], b"tail", "buffered WRTE is delivered before EOF");

        // Then EOF, driven purely by the `closed` flag (the CLSE msg was dropped).
        let n2 = session.read(&mut buf).await.expect("read after close flag");
        assert_eq!(n2, 0, "a dropped CLSE still surfaces as EOF via the closed flag");
    }

    /// PR3 (C2): the windowed send credit is sourced from the shared `recv_credit`
    /// atomic that the reader fills losslessly — NOT from the ack message's
    /// payload. The ack message is only a wakeup poke. This test proves the
    /// separation: it banks the real credit in the atomic, then wakes the parked
    /// writer with an EMPTY-payload poke (0 wire delta). Under the old "credit
    /// from the OKAY payload" model the empty poke would credit 0 and the write
    /// would stay blocked forever; with the atomic as the source the write
    /// unblocks. (A poke is only ever dropped when the ack queue is full, i.e.
    /// other pokes are pending to wake on, so a banked credit always has a wakeup.)
    #[tokio::test]
    async fn windowed_write_credit_comes_from_atomic_not_poke_payload() {
        let (mut session, _data_tx, ack_tx, mut writer_rx, _ctl) =
            MultiplexedSession::new_for_test(10, 20, true, FlowControl::new_windowed(0));
        let recv_credit = session.recv_credit_handle();

        let write = tokio::spawn(async move {
            session.write_all(b"xyz").await.expect("write after atomic credit");
            session
        });

        // Parked: window starts at 0, nothing enqueued.
        tokio::task::yield_now().await;
        assert!(
            writer_rx.try_recv().is_err(),
            "no WRTE while the window is exhausted"
        );

        // Reader path: bank the credit in the atomic FIRST (lossless), then send a
        // bare wakeup poke that carries NO window delta in its payload.
        recv_credit.fetch_add(4096, Ordering::AcqRel);
        ack_tx
            .send(msg(MessageCommand::Okay, 20, 10, &[]))
            .await
            .expect("send empty OKAY poke");

        // The write must flush, crediting from the atomic (not the empty poke).
        let frame = pump_writer(&mut writer_rx).await;
        assert_eq!(
            frame.payload(),
            b"xyz",
            "credit from the atomic (not the empty poke payload) unblocks the write"
        );
        let _session = write.await.expect("write task completes");
    }

    /// The per-session span carries `local_id` as a field, so an event emitted
    /// while the span is entered is attributable to one session. This is the
    /// mechanism a `RUST_LOG=[session{local_id=...}]` filter relies on.
    ///
    /// Gated on `tracing-init` because the assertion needs `tracing-subscriber`
    /// (the only build that pulls it in); the library default stays a pure emitter.
    #[cfg(feature = "tracing-init")]
    #[test]
    fn session_span_records_local_id_field() {
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::registry::LookupSpan;
        use tracing_subscriber::{Layer, Registry};

        #[derive(Default)]
        struct Captured(Arc<Mutex<Option<u64>>>);

        impl Visit for Captured {
            fn record_u64(&mut self, field: &Field, value: u64) {
                if field.name() == "local_id" {
                    *self.0.lock().expect("capture lock") = Some(value);
                }
            }
            fn record_i64(&mut self, field: &Field, value: i64) {
                if field.name() == "local_id" {
                    *self.0.lock().expect("capture lock") =
                        Some(u64::try_from(value).expect("local_id non-negative"));
                }
            }
            fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        }

        struct CaptureLayer(Arc<Mutex<Option<u64>>>);

        impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for CaptureLayer {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                _id: &tracing::Id,
                _ctx: Context<'_, S>,
            ) {
                if attrs.metadata().name() == "session" {
                    let mut visitor = Captured(self.0.clone());
                    attrs.record(&mut visitor);
                }
            }
        }

        let captured = Arc::new(Mutex::new(None));
        let subscriber = Registry::default().with(CaptureLayer(captured.clone()));

        let local_id: u32 = 4242;
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("session", local_id);
            let _enter = span.enter();
        });

        assert_eq!(
            *captured.lock().expect("capture lock"),
            Some(u64::from(local_id)),
            "the `session` span must record `local_id` so per-session RUST_LOG filtering works"
        );
    }
}
