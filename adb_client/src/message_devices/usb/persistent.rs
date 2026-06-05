//! Persistent USB connection with session multiplexing.
//!
//! Holds a single CNXN+AUTH'd USB connection and allows multiple concurrent
//! ADB sessions (shell, tcp, sync) to be opened without re-authenticating.
//! A background reader thread demultiplexes incoming messages by `local_id`.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rand::RngExt;

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

/// Boxed predicate used to filter raw-tee'd messages for a subscriber.
type RawFilter = Box<dyn Fn(&ADBTransportMessage) -> bool + Send>;

/// A registered raw subscriber: its filter predicate plus the sender side of
/// its bounded queue. Lives in [`PersistentUsbConnection::raw_subscribers`].
struct RawSubscriber {
    filter: RawFilter,
    tx: SyncSender<ADBTransportMessage>,
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

/// A persistent USB connection to an ADB device that supports concurrent sessions.
///
/// Unlike the per-operation model in `ADBUSBDevice`, this holds the USB handle
/// permanently and multiplexes multiple sessions over a single authenticated connection.
pub struct PersistentUsbConnection {
    /// Writer half — serialized access via mutex.
    writer: Arc<Mutex<USBTransport>>,
    /// Session registry: `local_id` -> channels for incoming messages.
    sessions: Arc<Mutex<HashMap<u32, SessionChannels>>>,
    /// Reader thread handle.
    reader_handle: Option<thread::JoinHandle<()>>,
    /// Flag to signal shutdown.
    shutdown: Arc<AtomicBool>,
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
    /// thread, so the receiver reports disconnect once the reader stops.
    pending_opens_rx: Mutex<Option<Receiver<ADBTransportMessage>>>,
    /// Registered raw subscribers (see [`Self::subscribe_raw`]). The reader
    /// thread holds a clone and tees matching messages to each.
    raw_subscribers: Arc<Mutex<Vec<RawSubscriber>>>,
}

impl PersistentUsbConnection {
    /// Create a new persistent connection from a USB transport.
    ///
    /// Performs CNXN+AUTH handshake, then spawns a reader thread for message demuxing.
    ///
    /// Advertises the honest [`DeviceFeatureSet::default`] banner. To advertise a
    /// custom feature set, use [`Self::new_with_features`].
    pub fn new(transport: USBTransport, private_key_path: Option<PathBuf>) -> Result<Self> {
        Self::new_with_features(transport, private_key_path, DeviceFeatureSet::default())
    }

    /// Create a new persistent connection advertising an explicit feature set.
    ///
    /// The `features` set determines the `host::features=` list sent in the CNXN
    /// banner. Only advertise features this end actually implements — see
    /// [`DeviceFeatureSet`].
    pub fn new_with_features(
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
        transport.connect()?;

        // Perform CNXN handshake; the device's banner tells us which features it
        // supports so we can negotiate `delayed_ack` (intersection of both ends).
        let device_banner = Self::do_connect(&mut transport, &private_key, &features)?;
        let delayed_ack_negotiated =
            features.delayed_ack && banner_advertises_delayed_ack(&device_banner);
        log::debug!(
            "PersistentUsb: delayed_ack negotiated = {delayed_ack_negotiated} (local={}, device_banner_has_it={})",
            features.delayed_ack,
            banner_advertises_delayed_ack(&device_banner)
        );

        let sessions: Arc<Mutex<HashMap<u32, SessionChannels>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let raw_subscribers: Arc<Mutex<Vec<RawSubscriber>>> = Arc::new(Mutex::new(Vec::new()));
        let (pending_opens_tx, pending_opens_rx) = mpsc::sync_channel(PENDING_OPENS_CHANNEL_SIZE);

        // Clone transport for reader thread (shares Arc<DeviceHandle>)
        let reader_transport = transport.clone();
        let reader_sessions = sessions.clone();
        let reader_shutdown = shutdown.clone();
        let reader_raw_subscribers = raw_subscribers.clone();

        let reader_handle = thread::Builder::new()
            .name("usb-reader".into())
            .spawn(move || {
                // The reader thread owns the OPEN-queue sender, keeping the
                // channel alive for its whole lifetime; the receiver reports
                // disconnect only once this thread exits.
                let pending_opens_tx = pending_opens_tx;
                Self::reader_loop(
                    reader_transport,
                    &reader_sessions,
                    &reader_shutdown,
                    &reader_raw_subscribers,
                    &pending_opens_tx,
                );
            })
            .map_err(|e| RustADBError::IOError(io::Error::other(e)))?;

        let writer = Arc::new(Mutex::new(transport));

        Ok(Self {
            writer,
            sessions,
            reader_handle: Some(reader_handle),
            shutdown,
            features,
            delayed_ack_negotiated,
            pending_opens_rx: Mutex::new(Some(pending_opens_rx)),
            raw_subscribers,
        })
    }

    /// Create from `vendor_id/product_id`.
    ///
    /// Advertises the honest [`DeviceFeatureSet::default`] banner. To advertise a
    /// custom feature set, use [`Self::new_from_ids_with_features`].
    pub fn new_from_ids(
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
    }

    /// Create from `vendor_id/product_id`, advertising an explicit feature set.
    pub fn new_from_ids_with_features(
        vendor_id: u16,
        product_id: u16,
        private_key_path: Option<PathBuf>,
        features: DeviceFeatureSet,
    ) -> Result<Self> {
        let transport = USBTransport::new(vendor_id, product_id)?;
        Self::new_with_features(transport, private_key_path, features)
    }

    /// The feature set advertised to the device in the CNXN banner.
    #[must_use]
    pub fn device_features(&self) -> &DeviceFeatureSet {
        &self.features
    }

    /// Subscribe to device-originated `OPEN` messages (pull model).
    ///
    /// Returns the consumer side of a bounded queue. The reader thread routes
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
    /// been taken by a previous call (there is a single consumer), or
    /// [`RustADBError::PoisonError`] if the internal mutex is poisoned.
    pub fn incoming_opens(&self) -> Result<Receiver<ADBTransportMessage>> {
        let mut guard = self.pending_opens_rx.lock()?;
        guard.take().ok_or_else(|| {
            RustADBError::ADBRequestFailed(
                "incoming_opens: receiver already taken (single consumer only)".into(),
            )
        })
    }

    /// Subscribe to a raw, filtered copy of every inbound message (low-level
    /// primitive, committed stable public API).
    ///
    /// The reader thread tees every received [`ADBTransportMessage`] for which
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
    pub fn subscribe_raw(
        &self,
        filter: impl Fn(&ADBTransportMessage) -> bool + Send + 'static,
    ) -> Result<Receiver<ADBTransportMessage>> {
        let (tx, rx) = mpsc::sync_channel(RAW_SUBSCRIBER_CHANNEL_SIZE);
        let mut subs = self.raw_subscribers.lock()?;
        subs.push(RawSubscriber {
            filter: Box::new(filter),
            tx,
        });
        Ok(rx)
    }

    /// Send a raw [`ADBTransportMessage`] over the connection (low-level
    /// primitive, committed stable public API).
    ///
    /// Writes through the shared writer mutex, exactly like `open_session`. The
    /// caller is responsible for all protocol semantics (id allocation, flow
    /// control). Pairs with [`Self::subscribe_raw`] for relay/reverse use.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::PoisonError`] if the writer mutex is poisoned, or
    /// any error from the underlying transport write.
    pub fn send_raw(&self, msg: ADBTransportMessage) -> Result<()> {
        let mut writer = self.writer.lock()?;
        writer.write_message(msg)
    }

    /// Perform CNXN+AUTH handshake on a connected transport.
    ///
    /// Returns the device's CNXN banner string (used to negotiate features such
    /// as `delayed_ack`).
    fn do_connect(
        transport: &mut USBTransport,
        private_key: &ADBRsaKey,
        features: &DeviceFeatureSet,
    ) -> Result<String> {
        // Drain any stale messages from previous sessions on this USB pipe
        while let Ok(msg) =
            transport.read_message_with_timeout(std::time::Duration::from_millis(100))
        {
            log::trace!(
                "PersistentUsb: drained stale message: cmd={}",
                msg.header().command()
            );
        }

        // Honest banner: advertise only features this end actually implements
        // (see `DeviceFeatureSet`). The trailing NUL matches a real adb server.
        let banner = features.to_banner_string();

        // Try CNXN up to 3 times (adbd may send stale CLSE after unclean disconnect)
        for attempt in 1..=3 {
            let cnxn_msg = ADBTransportMessage::try_new(
                MessageCommand::Cnxn,
                0x0100_0000, // A_VERSION
                1_048_576,
                banner.as_bytes(),
            )?;
            transport.write_message(cnxn_msg)?;

            let response = transport.read_message()?;

            match response.header().command() {
                MessageCommand::Cnxn => {
                    let dev_banner = String::from_utf8_lossy(response.payload()).into_owned();
                    log::debug!(
                        "PersistentUsb: unencrypted connection established, device banner: {dev_banner:?}"
                    );
                    return Ok(dev_banner);
                }
                MessageCommand::Auth => {
                    log::debug!("PersistentUsb: authentication required");
                    return Self::do_auth(transport, response, private_key);
                }
                MessageCommand::Stls => {
                    return Err(RustADBError::ADBRequestFailed(
                        "STLS not supported in persistent USB connection".into(),
                    ));
                }
                MessageCommand::Clse => {
                    // Stale CLSE from previous session — retry
                    log::debug!("PersistentUsb: got stale CLSE on attempt {attempt}, retrying");
                    std::thread::sleep(std::time::Duration::from_millis(100));
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

    fn do_auth(
        transport: &mut USBTransport,
        message: ADBTransportMessage,
        private_key: &ADBRsaKey,
    ) -> Result<String> {
        if message.header().arg0() != AUTH_TOKEN {
            return Err(RustADBError::ADBRequestFailed(format!(
                "AUTH message with type != TOKEN ({})",
                message.header().arg0()
            )));
        }

        let sign = private_key.sign(message.into_payload())?;
        let sig_msg = ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_SIGNATURE, 0, &sign)?;
        transport.write_message(sig_msg)?;

        let received = transport.read_message()?;
        if received.header().command() == MessageCommand::Cnxn {
            let banner = String::from_utf8_lossy(received.payload()).into_owned();
            log::info!("PersistentUsb: auth OK (signature accepted), device banner: {banner:?}");
            return Ok(banner);
        }

        // Send public key
        let mut pubkey = private_key.android_pubkey_encode()?.into_bytes();
        pubkey.push(b'\0');
        let pk_msg =
            ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_RSAPUBLICKEY, 0, &pubkey)?;
        transport.write_message(pk_msg)?;

        let final_resp = transport.read_message_with_timeout(Duration::from_secs(10))?;
        final_resp.assert_command(MessageCommand::Cnxn)?;
        let banner = String::from_utf8_lossy(final_resp.payload()).into_owned();
        log::info!("PersistentUsb: auth OK (public key accepted), device banner: {banner:?}");
        Ok(banner)
    }

    /// Reader loop: the single owner of the USB bulk-IN endpoint.
    ///
    /// There is exactly ONE reader thread per connection — a second reader would
    /// deadlock on the IN-endpoint mutex (see `usb_transport`). So all inbound
    /// routing (session-data, session-ack, device-OPEN, raw tee) happens in this
    /// one loop. The routing decision is factored into the I/O-free
    /// [`classify_message`] so it can be unit-tested without hardware (D1).
    ///
    /// The reader must NEVER block: blocking on a full queue would stall every
    /// session. All sends use `try_send`; on overflow we drop and `log::warn!`
    /// so the loss is observable (never a silent drop).
    fn reader_loop(
        mut transport: USBTransport,
        sessions: &Arc<Mutex<HashMap<u32, SessionChannels>>>,
        shutdown: &Arc<AtomicBool>,
        raw_subscribers: &Arc<Mutex<Vec<RawSubscriber>>>,
        pending_opens_tx: &SyncSender<ADBTransportMessage>,
    ) {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let msg = match transport.read_message_with_timeout(Duration::from_secs(1)) {
                Ok(msg) => msg,
                // Normal read timeout — `transfer_blocking` hit its deadline.
                // `nusb` surfaces this as `TransferError::Cancelled`, which the
                // transport maps to `RustADBError::UsbTimeout`. Match on it
                // structurally instead of string-matching the error message.
                Err(RustADBError::UsbTimeout) => continue,
                Err(e) => {
                    // Check if we're shutting down
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    log::warn!("PersistentUsb reader error: {e}");
                    // USB likely disconnected
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
            // Each subscriber that matches the filter gets a clone; on overflow
            // we drop that clone with a warning (never block the reader).
            Self::tee_raw(raw_subscribers, &msg);

            // Primary routing decision (I/O-free, unit-testable).
            let decision = {
                let sessions_lock = match sessions.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        log::warn!("PersistentUsb reader: sessions mutex poisoned: {e}");
                        break;
                    }
                };
                classify_message(&msg, &sessions_lock)
            };

            match decision {
                RouteDecision::SessionAck(id) | RouteDecision::SessionData(id) => {
                    let is_ack = matches!(decision, RouteDecision::SessionAck(_));
                    let sessions_lock = match sessions.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            log::warn!("PersistentUsb reader: sessions mutex poisoned: {e}");
                            break;
                        }
                    };
                    if let Some(channels) = sessions_lock.get(&id) {
                        let cmd = msg.header().command();
                        let target = if is_ack {
                            &channels.ack_tx
                        } else {
                            &channels.data_tx
                        };
                        if target.try_send(msg).is_err() {
                            // Bounded queue full. Do NOT block (would stall all
                            // sessions). Drop with an observable warning; full
                            // windowed backpressure arrives in a later PR.
                            log::warn!(
                                "PersistentUsb: session {id} queue full, dropped {cmd} message"
                            );
                        }
                    }
                }
                RouteDecision::DeviceOpen => {
                    // Bounded queue, overflow policy = drop the incoming OPEN
                    // (simplest correct non-blocking behavior; the reader can
                    // never block on a full queue). Observable via warning.
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
        log::debug!("PersistentUsb reader thread exiting");
    }

    /// Tee a received message to every raw subscriber whose filter matches.
    /// Never blocks: on a full subscriber queue the clone is dropped with a
    /// warning. Dead (disconnected) subscribers are pruned lazily.
    fn tee_raw(raw_subscribers: &Arc<Mutex<Vec<RawSubscriber>>>, msg: &ADBTransportMessage) {
        let mut subs = match raw_subscribers.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!("PersistentUsb reader: raw-subscribers mutex poisoned: {e}");
                return;
            }
        };
        if subs.is_empty() {
            return;
        }
        subs.retain(|sub| {
            if (sub.filter)(msg) {
                match sub.tx.try_send(msg.clone()) {
                    Ok(()) => true,
                    Err(mpsc::TrySendError::Full(_)) => {
                        log::warn!(
                            "PersistentUsb: raw subscriber queue full, dropped {} message",
                            msg.header().command()
                        );
                        true
                    }
                    // Receiver dropped → prune this subscriber.
                    Err(mpsc::TrySendError::Disconnected(_)) => false,
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
    /// otherwise it uses classic strict stop-and-wait. The blocking `Read`/`Write`
    /// trait contract is identical in both modes (see [`MultiplexedSession`]).
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::PoisonError`] if an internal mutex is poisoned,
    /// [`RustADBError::ADBRequestFailed`] on a missing/late OKAY, or any
    /// transport error.
    pub fn open_session(&self, cmd: &ADBLocalCommand) -> Result<MultiplexedSession> {
        let mut rng = rand::rng();
        let local_id: u32 = rng.random();

        // Create separate channels for data (WRTE/CLSE) and acks (OKAY)
        let (data_tx, data_rx) = mpsc::sync_channel(SESSION_CHANNEL_SIZE);
        let (ack_tx, ack_rx) = mpsc::sync_channel(SESSION_CHANNEL_SIZE);

        // Register in session map BEFORE sending OPEN (reader may respond fast)
        {
            let mut sessions = self.sessions.lock()?;
            sessions.insert(local_id, SessionChannels { data_tx, ack_tx });
        }

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

        {
            let mut writer = self.writer.lock()?;
            writer.write_message(open_msg)?;
        }

        // Wait for OKAY response on ack channel
        let response = ack_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
            RustADBError::ADBRequestFailed("open_session: timeout waiting for OKAY".into())
        })?;

        if response.header().command() != MessageCommand::Okay {
            // Unregister
            self.sessions.lock()?.remove(&local_id);
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
        {
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
            let mut writer = self.writer.lock()?;
            writer.write_message(ready_msg)?;
        }

        Ok(MultiplexedSession {
            local_id,
            remote_id,
            writer: self.writer.clone(),
            data_rx,
            ack_rx,
            sessions: self.sessions.clone(),
            read_buf: Vec::new(),
            read_pos: 0,
            closed: false,
            windowed: self.delayed_ack_negotiated,
            send_flow,
        })
    }

    /// Open a SYNC v1 file-transfer session multiplexed on this connection.
    ///
    /// Returns a [`SyncSession`] for `adb push`/`adb pull`. It opens a normal
    /// `sync:` session ([`Self::open_session`]) and rides the shared reader-loop
    /// demux like any other stream — so file transfer runs on the SAME
    /// authenticated USB connection as concurrent shell/tcp sessions. This is
    /// the crate-side mechanism that removes the need to open a separate,
    /// exclusive `ADBUSBDevice` for push/pull (which would double-claim the USB
    /// interface and conflict with this connection's exclusive claim).
    ///
    /// SYNC v2 + compression is out of scope; this is v1 only.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::open_session`] (poisoned mutex, missing
    /// OKAY, or transport error).
    pub fn open_sync_session(&self) -> Result<SyncSession> {
        let session = self.open_session(&ADBLocalCommand::Sync)?;
        Ok(SyncSession::new(session))
    }

    /// Open a `shell,v2` session on this connection and decode the inner-frame
    /// protocol (separate stdout/stderr + exit code).
    ///
    /// Unlike the v1 [`Self::shell_exec`] (which streams raw bytes and cannot
    /// report an exit code), this requests the shell-v2 service
    /// (`shell,v2,raw:<cmd>`) and returns a [`ShellV2Session`] that decodes the
    /// `[id][len][payload]` frames. Call [`ShellV2Session::execute`] to run the
    /// command to completion and obtain stdout/stderr/exit-code.
    ///
    /// Requires the device to support `shell_v2` (advertised in its CNXN
    /// banner). The v1 [`Self::shell_exec`] remains available for back-compat.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::open_session`] (poisoned mutex, missing
    /// OKAY, or transport error).
    pub fn open_shell_v2(&self, cmd: &str) -> Result<ShellV2Session> {
        // Non-empty args ⇒ `ADBLocalCommand` formats the service as
        // `shell,v2,raw:<cmd>` (shell-v2), vs the empty-args `shell:<cmd>` (v1).
        let command = ADBLocalCommand::ShellCommand(cmd.to_string(), vec!["v2".to_string()]);
        let session = self.open_session(&command)?;
        Ok(ShellV2Session::new(session))
    }

    /// Convenience: execute a shell command and return stdout + exit code.
    ///
    /// This is the v1 (raw, no inner framing) path: it cannot report an exit
    /// code (always `None`). For separated stdout/stderr and a real exit code,
    /// use [`Self::open_shell_v2`].
    pub fn shell_exec(&self, cmd: &str) -> Result<(String, Option<u8>)> {
        let command = ADBLocalCommand::ShellCommand(cmd.to_string(), vec![]);
        let mut session = self.open_session(&command)?;

        let mut output = Vec::new();
        loop {
            let mut buf = [0u8; 4096];
            match session.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(RustADBError::IOError(e)),
            }
        }

        // The last byte of shell output is the exit code (ADB shell v2 protocol isn't used here,
        // but the simple shell protocol doesn't include exit code in the stream).
        // For compatibility with the existing API, return None for exit code.
        let text = String::from_utf8_lossy(&output).to_string();
        Ok((text, None))
    }

    /// Check if the connection is still alive (reader thread running).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        match &self.reader_handle {
            Some(h) => !h.is_finished(),
            None => false,
        }
    }
}

impl Drop for PersistentUsbConnection {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader_handle.take() {
            // Give reader thread time to notice shutdown
            let _ = handle.join();
        }
        // Disconnect the writer transport
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.disconnect();
        }
    }
}

/// A multiplexed session over a persistent USB connection.
///
/// Represents a single ADB stream (e.g., one shell command or one TCP connection).
/// Implements `Read + Write` for use as a byte stream. Thread-safe writes via
/// the shared writer mutex; reads come from a dedicated per-session channel.
pub struct MultiplexedSession {
    local_id: u32,
    remote_id: u32,
    writer: Arc<Mutex<USBTransport>>,
    /// Channel for data messages (WRTE, CLSE)
    data_rx: Receiver<ADBTransportMessage>,
    /// Channel for flow control (OKAY)
    ack_rx: Receiver<ADBTransportMessage>,
    sessions: Arc<Mutex<HashMap<u32, SessionChannels>>>,
    read_buf: Vec<u8>,
    read_pos: usize,
    closed: bool,
    /// Whether `delayed_ack` windowed flow control is active for this session.
    /// Governs both the OKAY payload emitted on the read side and the windowing
    /// on the write side.
    windowed: bool,
    /// Send-side flow control window (windowed mode) / stop-and-wait marker
    /// (classic mode). Touched only by the write path.
    send_flow: FlowControl,
}

/// Channels for a single multiplexed session.
pub struct SessionChannels {
    pub data_tx: SyncSender<ADBTransportMessage>,
    pub ack_tx: SyncSender<ADBTransportMessage>,
}

impl MultiplexedSession {
    /// Get the local session ID.
    #[must_use]
    pub fn local_id(&self) -> u32 {
        self.local_id
    }

    /// Get the remote session ID.
    #[must_use]
    pub fn remote_id(&self) -> u32 {
        self.remote_id
    }

    /// Split into independent read and write halves for concurrent use.
    /// The session map entry is cleaned up when BOTH halves are dropped.
    #[must_use]
    pub fn into_split(mut self) -> (SessionReadHalf, SessionWriteHalf) {
        let local_id = self.local_id;
        let remote_id = self.remote_id;
        let writer = self.writer.clone();
        let sessions = self.sessions.clone();

        // Use ManuallyDrop + ptr::read to move fields out before forgetting self
        let data_rx = std::mem::replace(&mut self.data_rx, {
            // Create a dummy receiver that will never be used
            let (_, rx) = mpsc::sync_channel(1);
            rx
        });
        let ack_rx = std::mem::replace(&mut self.ack_rx, {
            let (_, rx) = mpsc::sync_channel(1);
            rx
        });
        let read_buf = std::mem::take(&mut self.read_buf);
        let read_pos = self.read_pos;
        let closed = self.closed;
        let windowed = self.windowed;
        let send_flow = std::mem::replace(&mut self.send_flow, FlowControl::new_classic());

        // Prevent Drop from sending CLSE or removing from sessions
        self.closed = true; // suppress CLSE in Drop
        // Drop will still remove from sessions map, but we'll re-insert...
        // Actually, better: just mark closed so Drop only removes from map.
        // We need to NOT remove from map. Let's just forget self.
        // But we already replaced fields with dummies, so Drop is safe to run
        // on the dummies. Actually the Drop will send CLSE if !closed and remove from map.
        // Set closed=true above prevents CLSE. But it will still remove from sessions.
        // We don't want that. So let's just std::mem::forget.
        std::mem::forget(self);

        let close_state = Arc::new(std::sync::atomic::AtomicBool::new(closed));
        let cleanup = Arc::new(SessionCleanup {
            local_id,
            remote_id,
            writer: writer.clone(),
            sessions,
            closed: close_state.clone(),
        });

        let read_half = SessionReadHalf {
            local_id,
            remote_id,
            writer: writer.clone(),
            data_rx,
            read_buf,
            read_pos,
            closed: close_state.clone(),
            windowed,
            _cleanup: cleanup.clone(),
        };

        let write_half = SessionWriteHalf {
            local_id,
            remote_id,
            writer,
            ack_rx,
            closed: close_state,
            send_flow,
            _cleanup: cleanup,
        };

        (read_half, write_half)
    }
}

/// Shared cleanup logic — sends CLSE and removes from sessions map when last reference dropped.
struct SessionCleanup {
    local_id: u32,
    remote_id: u32,
    writer: Arc<Mutex<USBTransport>>,
    sessions: Arc<Mutex<HashMap<u32, SessionChannels>>>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        if !self.closed.load(std::sync::atomic::Ordering::Relaxed)
            && let Ok(clse) = ADBTransportMessage::try_new(
                MessageCommand::Clse,
                self.local_id,
                self.remote_id,
                &[],
            )
            && let Ok(mut writer) = self.writer.lock()
        {
            let _ = writer.write_message(clse);
        }
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(&self.local_id);
        }
    }
}

/// Read half of a split `MultiplexedSession`.
pub struct SessionReadHalf {
    local_id: u32,
    remote_id: u32,
    writer: Arc<Mutex<USBTransport>>,
    data_rx: Receiver<ADBTransportMessage>,
    read_buf: Vec<u8>,
    read_pos: usize,
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// Whether `delayed_ack` windowing is active (governs the OKAY payload).
    windowed: bool,
    _cleanup: Arc<SessionCleanup>,
}

/// Write half of a split `MultiplexedSession`.
pub struct SessionWriteHalf {
    local_id: u32,
    remote_id: u32,
    writer: Arc<Mutex<USBTransport>>,
    ack_rx: Receiver<ADBTransportMessage>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// Send-side flow-control window (windowed) / stop-and-wait marker (classic).
    send_flow: FlowControl,
    _cleanup: Arc<SessionCleanup>,
}

/// Map a poisoned writer mutex into an `io::Error` for the `Read`/`Write` impls
/// (which can only return `io::Result`). Avoids the forbidden `lock().unwrap()`.
fn lock_writer(
    writer: &Mutex<USBTransport>,
) -> io::Result<std::sync::MutexGuard<'_, USBTransport>> {
    writer
        .lock()
        .map_err(|_| io::Error::other(RustADBError::PoisonError.to_string()))
}

/// Immutable per-session wire context shared by the read/write helpers: the
/// writer mutex plus the local/remote socket ids. Grouping these keeps the
/// shared helpers under clippy's argument-count limit and makes the read/write
/// call sites read uniformly.
struct SessionWire<'a> {
    writer: &'a Mutex<USBTransport>,
    local_id: u32,
    remote_id: u32,
}

/// Mutable re-buffering state for the read side (the tail of a WRTE payload that
/// did not fit into the caller's `buf` on the previous `read`).
struct ReadBuffer<'a> {
    buf: &'a mut Vec<u8>,
    pos: &'a mut usize,
}

/// Shared read-with-ack body for both `SessionReadHalf` and `MultiplexedSession`.
///
/// Receives one message from the data channel (after draining the local
/// re-buffered tail), emits the appropriate OKAY (windowed: payload = bytes just
/// delivered as i32 LE; classic: empty payload), copies the WRTE payload into
/// `buf`, and re-buffers any remainder. This is the SINGLE implementation of the
/// receive-side flow-control policy — both read paths call it, so the windowing
/// is not copy-pasted (code-reuse-thinking-guide).
fn read_with_ack(
    wire: &SessionWire<'_>,
    data_rx: &Receiver<ADBTransportMessage>,
    windowed: bool,
    rebuf: ReadBuffer<'_>,
    buf: &mut [u8],
) -> io::Result<ReadOutcome> {
    let ReadBuffer {
        buf: read_buf,
        pos: read_pos,
    } = rebuf;
    // Return buffered data first (no new OKAY needed; it was acked on arrival).
    if *read_pos < read_buf.len() {
        let available = &read_buf[*read_pos..];
        let to_copy = available.len().min(buf.len());
        buf[..to_copy].copy_from_slice(&available[..to_copy]);
        *read_pos += to_copy;
        if *read_pos >= read_buf.len() {
            read_buf.clear();
            *read_pos = 0;
        }
        return Ok(ReadOutcome::Data(to_copy));
    }

    let msg = data_rx
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "session channel closed"))?;

    match msg.header().command() {
        MessageCommand::Write => {
            let payload = msg.into_payload();
            // Emit the OKAY crediting the receive window: in windowed mode the
            // payload is the just-delivered byte count (i32 LE), eagerly per
            // flush; in classic mode it is empty (the stop-and-wait rendezvous).
            let okay_payload = encode_okay_payload(windowed, payload.len());
            let okay = ADBTransportMessage::try_new(
                MessageCommand::Okay,
                wire.local_id,
                wire.remote_id,
                &okay_payload,
            )
            .map_err(|e| io::Error::other(e.to_string()))?;
            {
                let mut writer = lock_writer(wire.writer)?;
                writer
                    .write_message(okay)
                    .map_err(|e| io::Error::other(e.to_string()))?;
            }
            if payload.is_empty() {
                return Ok(ReadOutcome::Data(0));
            }
            let to_copy = payload.len().min(buf.len());
            buf[..to_copy].copy_from_slice(&payload[..to_copy]);
            if to_copy < payload.len() {
                *read_buf = payload;
                *read_pos = to_copy;
            }
            Ok(ReadOutcome::Data(to_copy))
        }
        MessageCommand::Clse => Ok(ReadOutcome::Closed),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected command in data channel: {other}"),
        )),
    }
}

/// Result of [`read_with_ack`]: either bytes were copied into the buffer, or the
/// stream was closed (the caller flips its own `closed` flag, which differs
/// between the owned session and the split half).
enum ReadOutcome {
    Data(usize),
    Closed,
}

/// Shared windowed-write body for both `SessionWriteHalf` and `MultiplexedSession`.
///
/// This is the SINGLE implementation of the send-side flow-control policy. Both
/// write paths call it, so the windowing logic is not copy-pasted.
///
/// Behavior (preserves the blocking `io::Write` contract, D7):
/// - Drain any already-arrived OKAYs (non-blocking) to credit the window.
/// - If windowed and the window is exhausted (`<= 0`), BLOCK on the ack channel
///   until an OKAY credits it (or a CLSE closes the stream). This is the only
///   point that blocks for backpressure.
/// - Send ONE chunk (clamped to `MAX_PAYLOAD` = 1 MiB), debit the window, and
///   return its length. Multiple WRTEs may be in flight up to the 32 MiB window
///   — we do NOT wait for this chunk's own OKAY (that's the pipelining).
/// - In classic mode (`!windowed`) preserve strict stop-and-wait: after sending
///   the chunk, block for exactly one OKAY before returning.
///
/// Borrowed close-state accessors. The owned session uses a plain `bool` while
/// the split halves share an atomic, so close detection is abstracted behind two
/// closures rather than a concrete type.
struct CloseState<'a> {
    is_closed: &'a dyn Fn() -> bool,
    set_closed: &'a dyn Fn(),
}

/// Returns the number of `buf` bytes accepted into the pipeline.
fn windowed_write(
    wire: &SessionWire<'_>,
    ack_rx: &Receiver<ADBTransportMessage>,
    send_flow: &mut FlowControl,
    close: &CloseState<'_>,
    buf: &[u8],
) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    if (close.is_closed)() {
        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "session closed"));
    }

    // 1. Credit the window with any OKAYs that have already arrived (non-blocking).
    drain_acks(ack_rx, send_flow, close)?;
    if (close.is_closed)() {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "session closed by remote",
        ));
    }

    // 2. Windowed backpressure: if the window is exhausted, block until credited.
    if send_flow.is_windowed() {
        while !send_flow.can_send() {
            let response = ack_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timeout waiting for OKAY to reopen send window",
                )
            })?;
            apply_ack(&response, send_flow, close)?;
            if (close.is_closed)() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "session closed by remote",
                ));
            }
            // Opportunistically credit anything else already queued.
            drain_acks(ack_rx, send_flow, close)?;
        }
    }

    // 3. Send one chunk, clamped to MAX_PAYLOAD (decoupled from the window).
    let chunk_size = buf.len().min(MAX_PAYLOAD);
    let chunk = &buf[..chunk_size];
    let msg =
        ADBTransportMessage::try_new(MessageCommand::Write, wire.local_id, wire.remote_id, chunk)
            .map_err(|e| io::Error::other(e.to_string()))?;
    {
        let mut writer = lock_writer(wire.writer)?;
        writer
            .write_message(msg)
            .map_err(|e| io::Error::other(e.to_string()))?;
    }
    send_flow.record_sent(chunk_size);
    log::trace!(
        "PersistentUsb: session {} sent WRTE size={chunk_size}, window={:?}",
        wire.local_id,
        send_flow.available_bytes()
    );

    // 4. Classic mode preserves strict stop-and-wait: block for one OKAY.
    if !send_flow.is_windowed() {
        let response = ack_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "timeout waiting for OKAY after WRTE",
            )
        })?;
        apply_ack(&response, send_flow, close)?;
        if (close.is_closed)() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "session closed by remote",
            ));
        }
    }

    Ok(chunk_size)
}

/// Drain all currently-queued ack-channel messages (non-blocking), applying each
/// to the send window / close state.
fn drain_acks(
    ack_rx: &Receiver<ADBTransportMessage>,
    send_flow: &mut FlowControl,
    close: &CloseState<'_>,
) -> io::Result<()> {
    loop {
        match ack_rx.try_recv() {
            Ok(msg) => apply_ack(&msg, send_flow, close)?,
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "session ack channel closed",
                ));
            }
        }
    }
}

/// Apply a single ack-channel message: OKAY credits the window from its payload
/// delta; CLSE closes the stream; anything else is an error.
fn apply_ack(
    msg: &ADBTransportMessage,
    send_flow: &mut FlowControl,
    close: &CloseState<'_>,
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
            (close.set_closed)();
            Ok(())
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected OKAY/CLSE on ack channel, got {other}"),
        )),
    }
}

impl Read for SessionReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(0);
        }
        let wire = SessionWire {
            writer: &self.writer,
            local_id: self.local_id,
            remote_id: self.remote_id,
        };
        let rebuf = ReadBuffer {
            buf: &mut self.read_buf,
            pos: &mut self.read_pos,
        };
        match read_with_ack(&wire, &self.data_rx, self.windowed, rebuf, buf)? {
            ReadOutcome::Data(n) => Ok(n),
            ReadOutcome::Closed => {
                self.closed
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(0)
            }
        }
    }
}

impl Write for SessionWriteHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let closed = &self.closed;
        let is_closed = || closed.load(std::sync::atomic::Ordering::Relaxed);
        let set_closed = || closed.store(true, std::sync::atomic::Ordering::Relaxed);
        let wire = SessionWire {
            writer: &self.writer,
            local_id: self.local_id,
            remote_id: self.remote_id,
        };
        let close = CloseState {
            is_closed: &is_closed,
            set_closed: &set_closed,
        };
        windowed_write(&wire, &self.ack_rx, &mut self.send_flow, &close, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for MultiplexedSession {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.closed {
            return Ok(0);
        }
        let wire = SessionWire {
            writer: &self.writer,
            local_id: self.local_id,
            remote_id: self.remote_id,
        };
        let rebuf = ReadBuffer {
            buf: &mut self.read_buf,
            pos: &mut self.read_pos,
        };
        match read_with_ack(&wire, &self.data_rx, self.windowed, rebuf, buf)? {
            ReadOutcome::Data(n) => Ok(n),
            ReadOutcome::Closed => {
                self.closed = true;
                Ok(0)
            }
        }
    }
}

impl Write for MultiplexedSession {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // The owned session's `closed` is a plain `bool`, not the shared atomic
        // of the split halves, so close-detection threads through a Cell.
        let closed = std::cell::Cell::new(self.closed);
        let is_closed = || closed.get();
        let set_closed = || closed.set(true);
        let wire = SessionWire {
            writer: &self.writer,
            local_id: self.local_id,
            remote_id: self.remote_id,
        };
        let close = CloseState {
            is_closed: &is_closed,
            set_closed: &set_closed,
        };
        let result = windowed_write(&wire, &self.ack_rx, &mut self.send_flow, &close, buf);
        // Mirror any close observed during the write back onto the session.
        if closed.get() {
            self.closed = true;
        }
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for MultiplexedSession {
    fn drop(&mut self) {
        // Send CLSE to remote
        if !self.closed
            && let Ok(clse) = ADBTransportMessage::try_new(
                MessageCommand::Clse,
                self.local_id,
                self.remote_id,
                &[],
            )
            && let Ok(mut writer) = self.writer.lock()
        {
            let _ = writer.write_message(clse);
        }

        // Unregister from sessions map
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(&self.local_id);
        }
    }
}

// MultiplexedSession is Send: all fields are Send-safe (Mutex, Arc, Receiver, Vec, etc.)

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
            let (data_tx, _) = mpsc::sync_channel(1);
            let (ack_tx, _) = mpsc::sync_channel(1);
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

    #[test]
    fn raw_tee_delivers_only_matching_messages() {
        let subscribers: Arc<Mutex<Vec<RawSubscriber>>> = Arc::new(Mutex::new(Vec::new()));
        // Subscriber matches only WRTE messages.
        let (tx, rx) = mpsc::sync_channel(RAW_SUBSCRIBER_CHANNEL_SIZE);
        subscribers.lock().expect("lock").push(RawSubscriber {
            filter: Box::new(|m| m.header().command() == MessageCommand::Write),
            tx,
        });

        let wrte = msg(MessageCommand::Write, 1, 2, b"payload");
        let okay = msg(MessageCommand::Okay, 1, 2, &[]);

        PersistentUsbConnection::tee_raw(&subscribers, &wrte);
        PersistentUsbConnection::tee_raw(&subscribers, &okay);

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

    #[test]
    fn raw_tee_prunes_disconnected_subscribers() {
        let subscribers: Arc<Mutex<Vec<RawSubscriber>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::sync_channel(RAW_SUBSCRIBER_CHANNEL_SIZE);
        subscribers.lock().expect("lock").push(RawSubscriber {
            filter: Box::new(|_| true),
            tx,
        });
        // Drop the receiver → subscriber is now disconnected.
        drop(rx);

        let m = msg(MessageCommand::Write, 1, 2, b"x");
        PersistentUsbConnection::tee_raw(&subscribers, &m);

        assert!(
            subscribers.lock().expect("lock").is_empty(),
            "tee must prune a subscriber whose receiver was dropped"
        );
    }
}
