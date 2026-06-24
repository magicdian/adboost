//! [`SimDeviceBackend`]: an in-memory [`DeviceBackend`] over the
//! [`sim`](crate::message_devices::usb::sim) harness.
//!
//! This is the Phase-C piece of the simulator: where [`SimulatedDevice`] proves
//! the *client* (`PersistentConnection`) protocol logic, `SimDeviceBackend`
//! drives the *server* smartsocket frontend end-to-end through the
//! already-transport-neutral [`DeviceBackend`] trait — no USB, no TCP, no
//! hardware. It closes two coverage gaps the hand-rolled frontend test mocks
//! leave open:
//!
//! 1. **Real session bridging.** [`Self::open_local_service`] returns a genuine
//!    [`MultiplexedSession`] backed by a live `PersistentConnection<SimulatedDevice>`,
//!    so the frontend's bridge path runs for real (the frontend `MockBackend`'s
//!    `open_local_service` is `unimplemented!()`).
//! 2. **Real death → event emission.** A connection that actually dies (via
//!    [`SimRegistry::restart`], modeling an `adb root`/`unroot` adbd restart)
//!    fires its [`DeathSignal`](crate::message_devices) and this backend's
//!    watcher publishes the [`LifecycleEvent::TransportReset`] — instead of a
//!    test hand-feeding the event onto a channel.
//!
//! Re-enumeration is modeled faithfully: a [`SimRegistry::restart`] marks the
//! current connection dead forever (the old handle never revives — the IOKit
//! re-enumeration fact, see the harness honest-boundary note), and the next
//! [`Self::get_or_open`] mints a brand-new `SimulatedDevice` + connection — the
//! back-to-back `adb root; adb unroot` recovery shape.
//!
//! Structure deliberately mirrors [`DefaultDeviceBackend`]: a per-serial cache of
//! `Arc<PersistentConnection>`, a lazily-reaped `get_or_open`, a `OnceCell`
//! lifecycle broadcast hub, and a per-connection `closed_signal` → `TransportReset`
//! watcher.
//!
//! [`DefaultDeviceBackend`]: crate::server::DefaultDeviceBackend
//! [`SimulatedDevice`]: crate::message_devices::usb::sim::SimulatedDevice
//! [`MultiplexedSession`]: crate::usb::MultiplexedSession

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, OnceCell, broadcast, mpsc};

use crate::message_devices::usb::persistent::PersistentConnection;
use crate::message_devices::usb::sim::{DeviceProfile, Scenario, SimulatedDevice};
use crate::models::{ADBLocalCommand, DeviceFeatureSet};
use crate::server::backend::{
    BackendCapabilities, DeviceBackend, DeviceEntry, LifecycleEvent, TransportKind,
};
use crate::{Result, RustADBError};

/// Broadcast buffer for lifecycle events (mirrors `DefaultDeviceBackend`).
const LIFECYCLE_CHANNEL_SIZE: usize = 64;

/// A simulated connection alias — a persistent connection over a frame-level
/// simulated device.
type SimConnection = PersistentConnection<SimulatedDevice>;

/// One registered simulated device: how to build it, how it presents in the
/// device list, and its currently-open connection (if any).
struct SimEntry {
    /// Factory inputs: the profile (Android/adbd axis) the next `checkout` mints.
    profile: DeviceProfile,
    /// The scenario the next `checkout` mints (faults / death injection).
    scenario: Scenario,
    /// The device's transport kind for `-d`/`-e` selection.
    kind: TransportKind,
    /// The currently-open connection, if one has been opened and not reaped.
    conn: Option<Arc<SimConnection>>,
    /// A handle to the current connection's simulated device (shares its
    /// `SimState`), so `restart` can kill the reader while leaving the dead
    /// connection cached — exactly how a real cached connection reads as
    /// not-alive after an adbd restart until it is reaped.
    device: Option<SimulatedDevice>,
}

/// The programmable device set behind a [`SimDeviceBackend`]: a registry of
/// serial → simulated device, with re-enumeration (`restart`) and add/remove
/// driving the device list and lifecycle.
#[derive(Clone, Default)]
pub struct SimRegistry {
    entries: Arc<Mutex<HashMap<String, SimEntry>>>,
}

impl SimRegistry {
    /// A fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a device with `serial`, presenting `profile` and
    /// `kind`, opening healthy connections. Returns `self` for chaining.
    #[must_use]
    pub async fn with_device(
        self,
        serial: impl Into<String>,
        profile: DeviceProfile,
        kind: TransportKind,
    ) -> Self {
        self.add_device(serial, profile, Scenario::healthy(), kind)
            .await;
        self
    }

    /// Register (or replace) a device, including the [`Scenario`] its minted
    /// connections use (e.g. to inject a reader death or a CLSE-reject).
    pub async fn add_device(
        &self,
        serial: impl Into<String>,
        profile: DeviceProfile,
        scenario: Scenario,
        kind: TransportKind,
    ) {
        self.entries.lock().await.insert(
            serial.into(),
            SimEntry {
                profile,
                scenario,
                kind,
                conn: None,
                device: None,
            },
        );
    }

    /// Remove a device entirely (models an unplug — it leaves the device list).
    pub async fn remove_device(&self, serial: &str) {
        self.entries.lock().await.remove(serial);
    }

    /// Restart `serial`'s adbd: kill the current connection's reader so the cached
    /// connection becomes observably not-alive (its `DeathSignal` fires), while
    /// leaving it cached — exactly how a real cached connection reads dead after
    /// an adbd restart until the next `get_or_open` reaps it and mints a
    /// brand-new connection (the dead handle never revives; only a fresh checkout
    /// recovers — the literal back-to-back `adb root; adb unroot` model). Returns
    /// the (now-dying) connection's `Arc` so a caller can await its death.
    pub async fn restart(&self, serial: &str) -> Option<Arc<SimConnection>> {
        let mut entries = self.entries.lock().await;
        let entry = entries.get_mut(serial)?;
        if let Some(device) = &entry.device {
            device.kill();
        }
        entry.conn.clone()
    }

    /// The current device list as `DeviceEntry`s (serial + kind + parsed banner
    /// capabilities), the authoritative set for `host:devices` / transport-id.
    async fn list(&self) -> Vec<DeviceEntry> {
        let entries = self.entries.lock().await;
        let mut out: Vec<DeviceEntry> = entries
            .iter()
            .map(|(serial, e)| {
                DeviceEntry::new(serial.clone())
                    .with_kind(e.kind)
                    .with_capabilities(DeviceFeatureSet::from_banner(&e.profile.banner))
            })
            .collect();
        // Stable order so transport-id assignment (1-based over the sorted set) is
        // deterministic across calls.
        out.sort_by(|a, b| a.serial.cmp(&b.serial));
        out
    }
}

/// An in-memory [`DeviceBackend`] over a [`SimRegistry`]. See the module docs.
pub struct SimDeviceBackend {
    registry: SimRegistry,
    /// Lazily-initialized lifecycle broadcast hub (created on first
    /// `subscribe_lifecycle`), matching `DefaultDeviceBackend`.
    lifecycle: OnceCell<broadcast::Sender<LifecycleEvent>>,
}

impl SimDeviceBackend {
    /// Build a backend over `registry`.
    #[must_use]
    pub fn new(registry: SimRegistry) -> Self {
        Self {
            registry,
            lifecycle: OnceCell::new(),
        }
    }

    /// The registry this backend serves (for tests to `restart` / add / remove).
    #[must_use]
    pub fn registry(&self) -> &SimRegistry {
        &self.registry
    }

    /// Get-or-open the live connection for `serial`, reaping a dead one and
    /// minting a fresh `SimulatedDevice` (re-enumeration). Mirrors
    /// `DefaultDeviceBackend::get_or_open`: a dead cached connection is dropped
    /// and a brand-new one is built, and a death watcher is spawned so the
    /// connection's eventual death publishes `TransportReset`.
    async fn get_or_open(&self, serial: &str) -> Result<Arc<SimConnection>> {
        let mut entries = self.registry.entries.lock().await;
        let entry = entries.get_mut(serial).ok_or_else(|| {
            RustADBError::DeviceNotFound(format!("sim backend: unknown serial {serial}"))
        })?;

        // Reuse a live connection; reap a dead one before reopening.
        if let Some(conn) = &entry.conn {
            if conn.is_alive() {
                return Ok(Arc::clone(conn));
            }
            entry.conn = None;
        }

        // Mint a brand-new simulated device + connection (the re-enumeration
        // model: a fresh handle, never the dead one). Keep a clone of the device
        // handle in the entry — it shares the connection's `SimState`, so a later
        // `restart` can kill the reader of THIS connection.
        let device = SimulatedDevice::with_scenario(entry.profile.clone(), entry.scenario.clone());
        let device_handle = device.clone();
        let conn = Arc::new(
            PersistentConnection::new_with_features(
                device,
                Some(dummy_key_path()),
                advertised_features(),
            )
            .await?,
        );
        entry.conn = Some(Arc::clone(&conn));
        entry.device = Some(device_handle);

        // Publish the connection's eventual death as a TransportReset, if a
        // frontend has subscribed to the lifecycle hub.
        if let Some(tx) = self.lifecycle.get() {
            spawn_transport_reset_watch(tx.clone(), serial.to_owned(), &conn);
        }
        Ok(conn)
    }

    /// Drop `serial`'s cached connection + device handle so the next
    /// `get_or_open` mints a brand-new one (re-enumeration). Used by the
    /// reopen-on-dead path.
    async fn reap_dead(&self, serial: &str) {
        if let Some(entry) = self.registry.entries.lock().await.get_mut(serial) {
            entry.conn = None;
            entry.device = None;
        }
    }
}

/// No on-disk key: `new_with_features` falls back to a random key when the path
/// is absent (`read_adb_private_key` returns `Ok(None)` on `NotFound`).
fn dummy_key_path() -> std::path::PathBuf {
    std::path::PathBuf::from("/nonexistent/adboost-sim-backend/no-such-key")
}

/// The feature set the backend advertises to its simulated devices — the honest
/// default (windowing on), so windowed flow control is negotiated against an
/// Android-16 profile.
fn advertised_features() -> DeviceFeatureSet {
    DeviceFeatureSet::default()
}

/// Spawn the connection-death → `TransportReset` watcher (mirrors
/// `DefaultDeviceBackend::spawn_transport_reset_watch`): await a standalone death
/// future that holds no reference to the connection, then broadcast the event.
fn spawn_transport_reset_watch(
    tx: broadcast::Sender<LifecycleEvent>,
    serial: String,
    conn: &Arc<SimConnection>,
) {
    let death = conn.closed_signal();
    tokio::spawn(async move {
        death.await;
        let _ = tx.send(LifecycleEvent::TransportReset(serial));
    });
}

impl DeviceBackend for SimDeviceBackend {
    async fn list_devices(&self) -> Vec<DeviceEntry> {
        self.registry.list().await
    }

    async fn subscribe_changes(&self) -> mpsc::Receiver<Vec<DeviceEntry>> {
        // One snapshot of the current set, then close — enough to drive a single
        // `host:track-devices` assertion deterministically.
        let (tx, rx) = mpsc::channel(1);
        let snapshot = self.registry.list().await;
        tokio::spawn(async move {
            let _ = tx.send(snapshot).await;
        });
        rx
    }

    async fn subscribe_lifecycle(&self) -> mpsc::Receiver<LifecycleEvent> {
        let sender = self
            .lifecycle
            .get_or_init(|| async { broadcast::channel(LIFECYCLE_CHANNEL_SIZE).0 })
            .await;
        let mut bcast = sender.subscribe();
        let (tx, rx) = mpsc::channel(LIFECYCLE_CHANNEL_SIZE);
        tokio::spawn(async move {
            loop {
                match bcast.recv().await {
                    Ok(ev) => {
                        if tx.send(ev).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    async fn transport_alive(&self, serial: &str) -> bool {
        // Primary `wait-for-disconnect` signal: a cached connection whose I/O died
        // reads as not-alive even while the device is still listed (the MTK
        // adbd-restart shape). Falls back to presence when nothing is cached.
        let entries = self.registry.entries.lock().await;
        match entries.get(serial) {
            Some(entry) => match &entry.conn {
                Some(conn) => conn.is_alive(),
                None => true, // listed but never opened → present
            },
            None => false,
        }
    }

    async fn open_local_service(
        &self,
        serial: &str,
        cmd: &ADBLocalCommand,
    ) -> Result<crate::usb::MultiplexedSession> {
        // Cover the re-enumeration race (the back-to-back `adb root; adb unroot`
        // shape): `get_or_open` may hand back a connection that was just killed
        // but whose `is_alive()` has not flipped yet, so its `open_session` fails.
        // Mirror `DefaultDeviceBackend::open_session_with_reopen`: on a failure
        // against a now-dead connection, reap it and reopen ONCE. A failure on a
        // still-alive connection is a real service rejection — return it.
        let conn = self.get_or_open(serial).await?;
        match conn.open_session(cmd).await {
            Ok(session) => Ok(session),
            Err(e) if !conn.is_alive() => {
                drop(conn);
                self.reap_dead(serial).await;
                let fresh = self.get_or_open(serial).await?;
                tracing::debug!(serial, "sim backend: reopened after dead-on-open ({e})");
                fresh.open_session(cmd).await
            }
            Err(e) => Err(e),
        }
    }

    async fn capabilities(&self) -> BackendCapabilities {
        // The sim bridges plain local services (shell/tcp via open_local_service);
        // sync/shell_v2/reverse are out of Phase-C scope.
        BackendCapabilities::default()
    }

    async fn device_capabilities(
        &self,
        serial: &str,
        _timeout: std::time::Duration,
    ) -> Option<DeviceFeatureSet> {
        // Per-device negotiation truth: the banner the profile advertises. Drives
        // the frontend's honest per-device `host:features` gating (B-feat).
        let entries = self.registry.entries.lock().await;
        entries
            .get(serial)
            .map(|e| DeviceFeatureSet::from_banner(&e.profile.banner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{AdbServerFrontend, ServerCapabilities};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Build a frontend over a `SimDeviceBackend` serving `registry`.
    ///
    /// Advertises `shell_v2` at the server level — `build()` (unlike `serve()`)
    /// does not run the `negotiated_with(backend_caps)` step, so we set the
    /// server capability explicitly; the per-device `host:features` reply then
    /// intersects it with each device's banner (the B-feat behavior under test).
    fn frontend(registry: SimRegistry) -> Arc<AdbServerFrontend<SimDeviceBackend>> {
        Arc::new(
            AdbServerFrontend::builder(Arc::new(SimDeviceBackend::new(registry)))
                .capabilities(ServerCapabilities::default().with_shell_v2())
                .build(),
        )
    }

    /// Drive one terminal `host:*` request against `handle_client`, returning the
    /// raw reply bytes (the server closes after a host query).
    async fn round_trip(
        frontend: Arc<AdbServerFrontend<SimDeviceBackend>>,
        request: &str,
    ) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = frontend.handle_client(stream).await;
        });
        let mut client = TcpStream::connect(addr).await.expect("connect");
        client
            .write_all(format!("{:04x}{request}", request.len()).as_bytes())
            .await
            .expect("write req");
        client.flush().await.expect("flush");
        let mut buf = Vec::new();
        let _ = client.read_to_end(&mut buf).await;
        server.await.expect("server task");
        buf
    }

    /// Drive a transport-select request, reading exactly the 4-byte OKAY (a
    /// successful selection keeps the socket open, so we read 4 then drop).
    async fn round_trip_select(
        frontend: Arc<AdbServerFrontend<SimDeviceBackend>>,
        request: &str,
    ) -> [u8; 4] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = frontend.handle_client(stream).await;
        });
        let mut client = TcpStream::connect(addr).await.expect("connect");
        client
            .write_all(format!("{:04x}{request}", request.len()).as_bytes())
            .await
            .expect("write req");
        client.flush().await.expect("flush");
        let mut resp = [0u8; 4];
        client.read_exact(&mut resp).await.expect("read OKAY");
        drop(client);
        server.await.expect("server task");
        resp
    }

    fn android16_registry() -> SimRegistry {
        SimRegistry::default()
    }

    // -- host-protocol parity ------------------------------------------------

    /// `host:devices` lists the registry's devices with their wire state.
    #[tokio::test]
    async fn host_devices_lists_registered_devices() {
        let reg = android16_registry();
        reg.add_device(
            "SIMUSB1",
            DeviceProfile::android_16(),
            Scenario::healthy(),
            TransportKind::Usb,
        )
        .await;
        let resp = round_trip(frontend(reg), "host:devices").await;
        let body = String::from_utf8_lossy(&resp);
        assert!(
            body.contains("SIMUSB1") && body.contains("device"),
            "host:devices must list the registered serial in the `device` state; got {body:?}"
        );
    }

    /// `host:transport-usb` (the `adb -d` selection) must resolve the USB-kind
    /// device when a USB and a Local device are both present, and reply OKAY.
    #[tokio::test]
    async fn transport_usb_selects_usb_kind_device() {
        let reg = android16_registry();
        reg.add_device(
            "USBDEV",
            DeviceProfile::android_16(),
            Scenario::healthy(),
            TransportKind::Usb,
        )
        .await;
        reg.add_device(
            "LOCALDEV",
            DeviceProfile::android_16(),
            Scenario::healthy(),
            TransportKind::Local,
        )
        .await;
        let resp = round_trip_select(frontend(reg), "host:transport-usb").await;
        assert_eq!(
            &resp, b"OKAY",
            "host:transport-usb must uniquely select the USB-kind device (adb -d) and OKAY"
        );
    }

    /// `host:transport-local` (the `adb -e` selection) selects the Local-kind
    /// device under the same two-device set.
    #[tokio::test]
    async fn transport_local_selects_local_kind_device() {
        let reg = android16_registry();
        reg.add_device(
            "USBDEV",
            DeviceProfile::android_16(),
            Scenario::healthy(),
            TransportKind::Usb,
        )
        .await;
        reg.add_device(
            "LOCALDEV",
            DeviceProfile::android_16(),
            Scenario::healthy(),
            TransportKind::Local,
        )
        .await;
        let resp = round_trip_select(frontend(reg), "host:transport-local").await;
        assert_eq!(
            &resp, b"OKAY",
            "host:transport-local must uniquely select the Local-kind device (adb -e) and OKAY"
        );
    }

    /// B-feat (server side): `host-serial:<serial>:features` advertises only the
    /// features the device's banner carried — a feature-less device is NOT
    /// offered `shell_v2`, an Android-16 device IS.
    #[tokio::test]
    async fn host_features_are_per_device_honest() {
        let reg = android16_registry();
        reg.add_device(
            "FULLDEV",
            DeviceProfile::android_16(),
            Scenario::healthy(),
            TransportKind::Usb,
        )
        .await;
        reg.add_device(
            "BAREDEV",
            DeviceProfile::featureless(),
            Scenario::healthy(),
            TransportKind::Usb,
        )
        .await;
        let f = frontend(reg);

        let full = round_trip(Arc::clone(&f), "host-serial:FULLDEV:features").await;
        let full_body = String::from_utf8_lossy(&full);
        assert!(
            full_body.contains("shell_v2"),
            "B-feat: an Android-16 device must be offered shell_v2; got {full_body:?}"
        );

        let bare = round_trip(f, "host-serial:BAREDEV:features").await;
        let bare_body = String::from_utf8_lossy(&bare);
        assert!(
            !bare_body.contains("shell_v2"),
            "B-feat: a feature-less device must NOT be offered shell_v2; got {bare_body:?}"
        );
    }

    // -- real session bridging (the gap MockBackend leaves) ------------------

    /// The real bridge path: select a transport, then issue `shell:...`. The
    /// frontend opens a real sim-backed `MultiplexedSession` via
    /// `open_local_service` (the path the frontend `MockBackend` cannot run),
    /// replies OKAY, and pumps the device's echoed bytes back to the client.
    #[tokio::test]
    async fn shell_service_bridges_through_real_session() {
        let reg = SimRegistry::default();
        reg.add_device(
            "BRIDGE1",
            DeviceProfile::android_16(),
            // Echo the client's bytes back, then close the stream so the bridge
            // sees EOF and the client's read_to_end returns.
            Scenario::healthy()
                .with_echo_bytes(64)
                .with_close_after_first_write(),
            TransportKind::Usb,
        )
        .await;
        let frontend = frontend(reg);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = frontend.handle_client(stream).await;
        });
        let mut client = TcpStream::connect(addr).await.expect("connect");

        // 1. Select the transport.
        let req = "host:transport:BRIDGE1";
        client
            .write_all(format!("{:04x}{req}", req.len()).as_bytes())
            .await
            .expect("write transport");
        let mut okay = [0u8; 4];
        client.read_exact(&mut okay).await.expect("transport OKAY");
        assert_eq!(&okay, b"OKAY", "transport selection must OKAY");

        // 2. Open a shell service — bridged onto a real sim session.
        let shell = "shell:run";
        client
            .write_all(format!("{:04x}{shell}", shell.len()).as_bytes())
            .await
            .expect("write shell");
        let mut svc_okay = [0u8; 4];
        client
            .read_exact(&mut svc_okay)
            .await
            .expect("shell service OKAY");
        assert_eq!(
            &svc_okay, b"OKAY",
            "the bridged shell service must reply OKAY before pumping data"
        );

        // 3. After the OKAY the socket is a raw byte stream. Write payload bytes;
        //    the bridge forwards them to the device as a WRTE, the device echoes
        //    them back (and then CLSEs), and the bridge pumps the echo to us.
        client
            .write_all(b"ping-over-bridge")
            .await
            .expect("write session payload");
        client.flush().await.expect("flush payload");

        let mut echoed = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.read_to_end(&mut echoed),
        )
        .await
        .expect("the bridge must EOF after the device closes the stream");
        assert_eq!(
            echoed, b"ping-over-bridge",
            "the real bridge must pump the device's echoed session bytes back to the client"
        );
        // Close the client so the bridge's host→device half sees EOF and the
        // server task completes (the bridge joins BOTH directions).
        drop(client);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .expect("server task must finish once the client closes");
    }

    // -- reconnect / re-enumeration (the headline Phase-C unlock) ------------

    /// `wait-for-any-disconnect` unblocks (two OKAYs) when the device's real sim
    /// connection has died. This is the headline reconnect unlock: the death is
    /// *produced* by the real connection (via a registry `restart`), and the
    /// frontend's `transport_alive` entry check reads the cached connection as
    /// not-alive — exactly the MTK-adbd-restart shape where the serial never
    /// leaves the device list yet the transport is gone.
    #[tokio::test]
    async fn wait_for_disconnect_unblocks_on_real_connection_death() {
        let reg = SimRegistry::default();
        reg.add_device(
            "ROOTDEV",
            DeviceProfile::android_16(),
            Scenario::healthy(),
            TransportKind::Usb,
        )
        .await;
        let backend = Arc::new(SimDeviceBackend::new(reg.clone()));
        let frontend = Arc::new(AdbServerFrontend::builder(Arc::clone(&backend)).build());

        // Open a live connection, then kill it via restart so the cached transport
        // is genuinely dead (the real death edge), while the serial stays listed.
        let session = backend
            .open_local_service("ROOTDEV", &ADBLocalCommand::Shell)
            .await
            .expect("open a live session so the connection exists");
        drop(session);
        let dead = reg
            .restart("ROOTDEV")
            .await
            .expect("the opened connection is registered");
        // Wait for its I/O tasks to wind down so is_alive() reads false.
        tokio::time::timeout(std::time::Duration::from_secs(5), dead.wait_closed())
            .await
            .expect("the restarted connection's death edge must fire");
        assert!(
            !backend.transport_alive("ROOTDEV").await,
            "after restart the cached connection must read as not-alive (real death edge)"
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let f = Arc::clone(&frontend);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = f.handle_client(stream).await;
        });

        let req = "host-serial:ROOTDEV:wait-for-any-disconnect";
        let mut client = TcpStream::connect(addr).await.expect("connect");
        client
            .write_all(format!("{:04x}{req}", req.len()).as_bytes())
            .await
            .expect("write req");
        client.flush().await.expect("flush");

        let mut buf = Vec::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_to_end(&mut buf),
        )
        .await;
        assert!(
            read.is_ok(),
            "wait-for-disconnect must unblock on the dead transport well within 5s"
        );
        assert_eq!(
            buf, b"OKAYOKAY",
            "a real sim connection death must satisfy wait-for-disconnect with two OKAYs"
        );
        let _ = server.await;
    }

    /// Back-to-back recovery: after a `restart` kills the connection, the next
    /// `open_local_service` mints a brand-new connection and succeeds — the
    /// literal `adb root; adb unroot` reopen (the dead handle never revives; only
    /// a fresh checkout recovers).
    #[tokio::test]
    async fn back_to_back_restart_recovers_via_reopen() {
        let reg = SimRegistry::default();
        reg.add_device(
            "FLIPDEV",
            DeviceProfile::android_16(),
            Scenario::healthy(),
            TransportKind::Usb,
        )
        .await;
        let backend = SimDeviceBackend::new(reg.clone());

        let s1 = backend
            .open_local_service("FLIPDEV", &ADBLocalCommand::Shell)
            .await
            .expect("first session opens");
        let remote1 = s1.remote_id();
        drop(s1);

        // adbd "restart": the current connection dies forever.
        if let Some(conn) = reg.restart("FLIPDEV").await {
            drop(conn);
        }

        // The next open must mint a brand-new connection and succeed.
        let s2 = backend
            .open_local_service("FLIPDEV", &ADBLocalCommand::Shell)
            .await
            .expect("after restart, a fresh reopen must succeed (re-enumeration recovery)");
        assert_ne!(remote1, 0, "the first session had a valid remote id");
        let _ = s2.remote_id();
    }

    /// An unknown serial's service open fails cleanly (no panic/hang) — the
    /// negative path.
    #[tokio::test]
    async fn unknown_serial_open_fails_cleanly() {
        let backend = SimDeviceBackend::new(SimRegistry::default());
        let err = backend
            .open_local_service("NOSUCH", &ADBLocalCommand::Shell)
            .await
            .err()
            .expect("opening an unknown serial must error, not panic");
        assert!(
            matches!(err, RustADBError::DeviceNotFound(_)),
            "an unknown serial must surface DeviceNotFound; got {err:?}"
        );
    }
}
