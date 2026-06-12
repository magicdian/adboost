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
//!   it drops and `log::warn!`s so the loss is observable.
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
//! Teardown is explicit: [`PersistentUsbConnection::close`] /
//! [`MultiplexedSession::close`] send their CLSE and await confirmation. `Drop`
//! is best-effort: it enqueues CLSE fire-and-forget onto the writer channel and
//! `abort()`s the reader/writer tasks (Rust stable has no async `Drop`).

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    FlowControl, INITIAL_DELAYED_ACK_BYTES, MAX_PAYLOAD, encode_okay_payload,
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
    /// The single consumer side of the device-originated OPEN queue. `Option`
    /// so it can be taken by [`Self::incoming_opens`]; subsequent calls return
    /// an error. The matching sender lives in (and is kept alive by) the reader
    /// task, so the receiver reports disconnect once the reader stops.
    pending_opens_rx: Option<mpsc::Receiver<ADBTransportMessage>>,
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
            log::warn!(
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
        log::debug!(
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
            pending_opens_rx: Some(pending_opens_rx),
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
    /// (typically the xdb server layer) decides whether to accept the stream
    /// (reply `OKAY(device_local_id, host_local_id)` + register a session) or
    /// reject it (reply `CLSE(0, device_local_id)`). This crate intentionally
    /// implements no reverse policy.
    ///
    /// The queue is bounded; on overflow the reader drops the *incoming* OPEN
    /// and logs a warning rather than blocking (a blocked reader would stall
    /// every session). Drain it promptly.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::ADBRequestFailed`] if the receiver has already
    /// been taken by a previous call (there is a single consumer).
    pub fn incoming_opens(&mut self) -> Result<mpsc::Receiver<ADBTransportMessage>> {
        self.pending_opens_rx.take().ok_or_else(|| {
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
    async fn do_connect(
        transport: &mut USBTransport,
        private_key: &ADBRsaKey,
        features: &DeviceFeatureSet,
    ) -> Result<(u32, String)> {
        // Drain any stale messages from previous sessions on this USB pipe
        while let Ok(msg) = transport
            .read_message_with_timeout(Duration::from_millis(100))
            .await
        {
            log::trace!(
                "PersistentUsb: drained stale message: cmd={}",
                msg.header().command()
            );
        }

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

        // Try CNXN up to 3 times (adbd may send stale CLSE after unclean disconnect)
        for attempt in 1..=3 {
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
                    log::debug!(
                        "PersistentUsb: unencrypted connection established, device banner: {dev_banner:?}"
                    );
                    return Ok((response.header().arg0(), dev_banner));
                }
                MessageCommand::Auth => {
                    log::debug!("PersistentUsb: authentication required");
                    return Self::do_auth(transport, response, private_key).await;
                }
                MessageCommand::Stls => {
                    return Err(RustADBError::ADBRequestFailed(
                        "STLS not supported in persistent USB connection".into(),
                    ));
                }
                MessageCommand::Clse => {
                    // Stale CLSE from previous session — retry
                    log::debug!("PersistentUsb: got stale CLSE on attempt {attempt}, retrying");
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
        Err(RustADBError::ADBRequestFailed(
            "CNXN failed after 3 attempts (stale CLSE)".into(),
        ))
    }

    /// Returns `(device_protocol_version, device_banner)` from the accepted CNXN
    /// response — see [`Self::do_connect`].
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
            log::info!("PersistentUsb: auth OK (signature accepted), device banner: {banner:?}");
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
        log::info!("PersistentUsb: auth OK (public key accepted), device banner: {banner:?}");
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
    /// session. All sends use `try_send`; on overflow we drop and `log::warn!`
    /// so the loss is observable (never a silent drop).
    async fn reader_loop(
        mut transport: USBTransport,
        mut control_rx: mpsc::Receiver<ReaderControl>,
        pending_opens_tx: mpsc::Sender<ADBTransportMessage>,
    ) {
        let mut sessions: HashMap<u32, SessionChannels> = HashMap::new();
        let mut raw_subscribers: Vec<RawSubscriber> = Vec::new();

        loop {
            // A single ADB frame read is a cancel-safe atomic unit (nusb
            // `next_complete` is cancel-safe; the transport wraps it in its own
            // 1s timeout). We `select!` it against control-channel mutations so
            // registry changes are applied promptly without ever holding a lock
            // across the read `.await`.
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
                ReadStep::ReadTimeout | ReadStep::Control => continue,
                ReadStep::Closed => {
                    log::debug!("PersistentUsb reader: control channel closed, exiting");
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
                        log::warn!("PersistentUsb reader: skipping malformed frame: {e}");
                        continue;
                    }
                    log::warn!("PersistentUsb reader error (fatal): {e}");
                    break;
                }
            };

            log::trace!(
                "PersistentUsb reader: cmd={} arg0={} arg1={} payload_len={}",
                msg.header().command(),
                msg.header().arg0(),
                msg.header().arg1(),
                msg.payload().len()
            );

            // Tee to raw subscribers first (orthogonal to the primary route).
            Self::tee_raw(&mut raw_subscribers, &msg);

            // Primary routing decision (I/O-free, unit-testable).
            match classify_message(&msg, &sessions) {
                RouteDecision::SessionAck(id) | RouteDecision::SessionData(id) => {
                    let is_ack = msg.header().command() == MessageCommand::Okay;
                    if let Some(channels) = sessions.get(&id) {
                        let cmd = msg.header().command();
                        let target = if is_ack {
                            &channels.ack_tx
                        } else {
                            &channels.data_tx
                        };
                        if target.try_send(msg).is_err() {
                            // Bounded queue full. Do NOT block (would stall all
                            // sessions). Drop with an observable warning.
                            log::warn!(
                                "PersistentUsb: session {id} queue full, dropped {cmd} message"
                            );
                        }
                    }
                }
                RouteDecision::DeviceOpen => {
                    // Bounded queue, overflow policy = drop the incoming OPEN
                    // (the reader can never block on a full queue).
                    if pending_opens_tx.try_send(msg).is_err() {
                        log::warn!(
                            "PersistentUsb: incoming-OPEN queue full, dropped device-originated OPEN"
                        );
                    }
                }
                RouteDecision::Unknown => {
                    log::trace!(
                        "PersistentUsb: message for unknown session {} (cmd={}, dropping)",
                        msg.header().arg1(),
                        msg.header().command()
                    );
                }
            }
        }
        log::debug!("PersistentUsb reader task exiting");
    }

    /// Await either the next USB frame or the next reader-control message,
    /// applying control messages to the registry directly (so no lock crosses
    /// the read `.await`).
    async fn read_or_control(
        transport: &mut USBTransport,
        control_rx: &mut mpsc::Receiver<ReaderControl>,
        sessions: &mut HashMap<u32, SessionChannels>,
        raw_subscribers: &mut Vec<RawSubscriber>,
    ) -> ReadStep {
        tokio::select! {
            biased;
            ctrl = control_rx.recv() => match ctrl {
                Some(ReaderControl::Register(id, channels)) => {
                    sessions.insert(id, channels);
                    ReadStep::Control
                }
                Some(ReaderControl::Unregister(id)) => {
                    sessions.remove(&id);
                    ReadStep::Control
                }
                Some(ReaderControl::Subscribe(sub)) => {
                    raw_subscribers.push(sub);
                    ReadStep::Control
                }
                None => ReadStep::Closed,
            },
            read = transport.read_message_with_timeout(Duration::from_secs(1)) => match read {
                Ok(msg) => ReadStep::Message(msg),
                Err(RustADBError::UsbTimeout) => ReadStep::ReadTimeout,
                Err(e) => ReadStep::ReadError(e),
            },
        }
    }

    /// Writer task: the single owner of the USB bulk-OUT endpoint.
    ///
    /// Drains the outbound-frame queue and serializes every write. WRTE frames
    /// (`WithAck`) report their write `Result` back over a `oneshot`; OKAY /
    /// CLSE / OPEN / raw (`FireForget`) are written best-effort and logged on
    /// failure. The task exits when every sender (the connection + all session
    /// halves) has been dropped, draining the channel first.
    async fn writer_loop(
        mut transport: USBTransport,
        mut writer_rx: mpsc::Receiver<OutboundFrame>,
    ) {
        while let Some(frame) = writer_rx.recv().await {
            match frame {
                OutboundFrame::FireForget(msg) => {
                    if let Err(e) = transport.write_message(msg).await {
                        log::warn!("PersistentUsb writer: fire-and-forget write failed: {e}");
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
        log::debug!("PersistentUsb writer task exiting");
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
                        log::warn!(
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

    pub async fn open_session(&self, cmd: &ADBLocalCommand) -> Result<MultiplexedSession> {
        let local_id: u32 = {
            let mut rng = rand::rng();
            rng.random()
        };

        // Create separate channels for data (WRTE/CLSE) and acks (OKAY).
        // `data_rx` is `mut` because we borrow it during the open handshake to
        // observe an early CLSE (OPEN rejection) before moving it into the
        // returned `MultiplexedSession`.
        let (data_tx, mut data_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);
        let (ack_tx, mut ack_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);

        // Register in the reader's session map BEFORE sending OPEN (the reader
        // may respond fast). The control message is applied before any frame
        // for this id can be routed because the reader applies control messages
        // and reads from the same `select!` loop.
        self.control_tx
            .send(ReaderControl::Register(
                local_id,
                SessionChannels { data_tx, ack_tx },
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
        log::debug!(
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
        // window starts at 0 when delayed_ack is on: the device's first OKAY
        // payload credits it up to its grant. In classic mode it stays
        // stop-and-wait (no window). We seed the controller from any window
        // delta already carried by the OKAY response above (the device's first
        // OKAY may already carry its grant).
        let mut send_flow = if self.delayed_ack_negotiated {
            FlowControl::new_windowed(0)
        } else {
            FlowControl::new_classic()
        };
        send_flow.on_okay_payload(response.payload());

        // With delayed_ack, send an initial OKAY granting our own receive window
        // (32 MiB as i32 LE in the payload). Without it (classic), the OKAY
        // carries an empty payload and just signals readiness. adbd won't send
        // WRTE until it gets this initial OKAY.
        let ready_payload = encode_okay_payload(
            self.delayed_ack_negotiated,
            usize::try_from(INITIAL_DELAYED_ACK_BYTES).unwrap_or(usize::MAX),
        );
        let ready_msg = ADBTransportMessage::try_new(
            MessageCommand::Okay,
            local_id,
            remote_id,
            &ready_payload,
        )?;
        self.writer
            .try_send_fire_forget(ready_msg)
            .map_err(|_| RustADBError::SendError)?;

        // Take any window deltas already buffered on the ack channel (the device
        // may have credited us between OPEN and now); non-blocking.
        while let Ok(extra) = ack_rx.try_recv() {
            send_flow.on_okay_payload(extra.payload());
        }

        let close_state = Arc::new(AtomicBool::new(false));
        let inner = SessionInner {
            local_id,
            remote_id,
            writer: self.writer.clone(),
            control_tx: self.control_tx.clone(),
            closed: close_state,
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

    /// Gracefully close the connection: enqueues a connection-level CLSE and
    /// aborts the background tasks after the writer drains.
    ///
    /// `Drop` does this best-effort automatically; call `close` explicitly when
    /// you want to ensure the CLSE is flushed before the connection is dropped.
    pub async fn close(mut self) {
        // Best-effort connection-level CLSE.
        if let Ok(clse) = ADBTransportMessage::try_new(MessageCommand::Clse, 0, 0, &[]) {
            let _ = self.writer.send_with_ack(clse).await;
        }
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

/// Outcome of a single reader `select!` step.
enum ReadStep {
    Message(ADBTransportMessage),
    ReadTimeout,
    Control,
    Closed,
    ReadError(RustADBError),
}

impl Drop for PersistentUsbConnection {
    fn drop(&mut self) {
        // Best-effort connection-level CLSE: fire-and-forget onto the writer
        // queue (we cannot `.await` in Drop). If the queue is full or the writer
        // is gone, the abort below still tears the connection down.
        if let Ok(clse) = ADBTransportMessage::try_new(MessageCommand::Clse, 0, 0, &[])
            && let Err(e) = self.writer.try_send_fire_forget(clse)
        {
            // Best-effort contract: a full/closed writer queue means the
            // connection-level CLSE could not be delivered. The `abort` below
            // still tears the connection down, but the device may keep the
            // stream open until it times out — log it so the leak is observable.
            log::warn!("PersistentUsb: could not enqueue connection CLSE on drop: {e}");
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
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        // Best-effort CLSE + unregister. Fire-and-forget; cannot `.await` here.
        if !self.is_closed()
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
            log::warn!(
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

/// Channels for a single multiplexed session (held by the reader task).
pub struct SessionChannels {
    pub data_tx: mpsc::Sender<ADBTransportMessage>,
    pub ack_tx: mpsc::Sender<ADBTransportMessage>,
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

/// Apply a single ack-channel message: OKAY credits the window from its payload
/// delta; CLSE closes the stream; anything else is an error.
fn apply_ack(
    msg: &ADBTransportMessage,
    send_flow: &mut FlowControl,
    shared: &SessionInner,
) -> io::Result<()> {
    match msg.header().command() {
        MessageCommand::Okay => {
            if !send_flow.on_okay_payload(msg.payload()) {
                // Malformed OKAY payload (len not in {0,4}). AOSP drops the
                // packet; we log and keep the window unchanged rather than fail.
                log::warn!(
                    "PersistentUsb: ignoring OKAY with invalid payload len {}",
                    msg.payload().len()
                );
            }
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
    if shared.is_closed() {
        return Poll::Ready(Ok(()));
    }

    // Return buffered data first (no new OKAY needed; it was acked on arrival).
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

    match data_rx.poll_recv(cx) {
        Poll::Ready(Some(msg)) => match msg.header().command() {
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
        },
        Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "session channel closed",
        ))),
        Poll::Pending => Poll::Pending,
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
            log::trace!(
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

    // 2. Credit the window with any OKAYs that already arrived (non-blocking).
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
                Poll::Pending => return Poll::Pending,
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
            map.insert(id, SessionChannels { data_tx, ack_tx });
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

        // Credit the window via an OKAY on the ack channel.
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
}
