//! The listening ADB server frontend: accept loop + per-client host-protocol
//! state machine + local-service bridge.
//!
//! [`AdbServerFrontend`] binds a TCP socket (default `:5037`), and for each
//! client runs the smartsocket host protocol ([`super::protocol`]) until either
//! a terminal host query is answered or a transport is selected and a local
//! service (`shell:` / `tcp:`) is bridged onto the [`DeviceBackend`]. The
//! protocol framing lives in pure functions; this layer only does I/O and
//! routing.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::backend::{DeviceBackend, DeviceEntry, LifecycleEvent, TransportKind};
use super::capabilities::{KillPolicy, ServerCapabilities};
use super::forward::{ForwardRegistry, parse_forward, parse_killforward};
use super::forward_handle::ForwardHandle;
use super::on_disconnect::OnDisconnect;
use super::protocol;
use crate::models::{ADBLocalCommand, DeviceFeatureSet};

/// Builder for [`AdbServerFrontend`].
pub struct AdbServerFrontendBuilder<B: DeviceBackend> {
    backend: Arc<B>,
    addr: SocketAddr,
    caps: ServerCapabilities,
    on_disconnect: OnDisconnect,
}

impl<B: DeviceBackend> AdbServerFrontendBuilder<B> {
    /// Set the bind address (default `127.0.0.1:5037`).
    #[must_use]
    pub fn addr(mut self, addr: SocketAddr) -> Self {
        self.addr = addr;
        self
    }

    /// Set the advertised [`ServerCapabilities`] (default honest-minimal).
    #[must_use]
    pub fn capabilities(mut self, caps: ServerCapabilities) -> Self {
        self.caps = caps;
        self
    }

    /// Set the [`OnDisconnect`] policy: what happens to a device's `forward` /
    /// `reverse` rules when its transport disconnects. Defaults to
    /// [`OnDisconnect::ReleaseAll`] (release them, matching standard `adb`).
    #[must_use]
    pub fn on_disconnect(mut self, policy: OnDisconnect) -> Self {
        self.on_disconnect = policy;
        self
    }

    /// Finish building the frontend.
    #[must_use]
    pub fn build(self) -> AdbServerFrontend<B> {
        AdbServerFrontend {
            backend: self.backend,
            addr: self.addr,
            caps: self.caps,
            forwards: Arc::new(ForwardRegistry::default()),
            on_disconnect: self.on_disconnect,
        }
    }
}

/// An ADB server frontend: listens for native `adb`/`scrcpy` clients and
/// bridges their local services onto a [`DeviceBackend`].
pub struct AdbServerFrontend<B: DeviceBackend> {
    backend: Arc<B>,
    addr: SocketAddr,
    caps: ServerCapabilities,
    /// Server-global registry of active `host:forward` rules (host-side
    /// listeners). Shared because forward rules outlive the client socket that
    /// created them.
    forwards: Arc<ForwardRegistry>,
    /// What to do with a serial's forward/reverse rules when its transport
    /// disconnects. Consumed by the disconnect-handling task (PR3); stored here
    /// so the builder's choice survives into [`Self::serve`].
    on_disconnect: OnDisconnect,
}

/// How long a serial-aware capability query may spend establishing a device
/// connection to learn its banner (see
/// [`DeviceBackend::device_capabilities`](super::backend::DeviceBackend::device_capabilities)).
/// Cache hits return instantly; this bounds only the cold-handshake case so a
/// slow/unreachable device degrades to "unknown caps → conservative" instead of
/// stalling a `host:features` reply or a `shell,v2` gate. 2s comfortably covers a
/// healthy USB/TCP CNXN while staying well under a client's patience.
const DEVICE_CAPS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Outcome of dispatching the first (host) request on a client socket.
enum HostOutcome {
    /// A transport was selected; the next request on this socket is the local
    /// service to bridge to the named device serial.
    TransportSelected(String),
    /// The reply has been written; close the socket.
    Close,
}

impl<B: DeviceBackend> AdbServerFrontend<B> {
    /// Start a builder for a frontend over `backend`.
    pub fn builder(backend: Arc<B>) -> AdbServerFrontendBuilder<B> {
        AdbServerFrontendBuilder {
            backend,
            addr: SocketAddr::from(([127, 0, 0, 1], 5037)),
            caps: ServerCapabilities::default(),
            on_disconnect: OnDisconnect::default(),
        }
    }

    /// A [`ForwardHandle`] over this frontend's forward registry and backend —
    /// the caller-facing API for releasing a device's `forward` / `reverse`
    /// rules on demand.
    ///
    /// [`Self::serve`] consumes `self`, so obtain the handle (and clone it as
    /// needed) *before* serving if you want to release rules while the server
    /// runs — e.g. under [`OnDisconnect::Retain`], or from an
    /// [`OnDisconnect::Notify`] callback.
    #[must_use]
    pub fn handle(&self) -> ForwardHandle<B> {
        ForwardHandle::new(Arc::clone(&self.forwards), Arc::clone(&self.backend))
    }

    /// Bound address (useful when built with port 0 to discover the OS-assigned
    /// port — though [`Self::serve`] consumes `self`, callers can read this
    /// before serving).
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Run the accept loop until the listener errors fatally or the process
    /// stops it. Binding failure (e.g. `:5037` already taken) is returned as
    /// `Err` so the caller decides whether it is fatal.
    ///
    /// Each accepted client is served on its own task, so a slow or stuck client
    /// never blocks the accept loop.
    ///
    /// # Errors
    ///
    /// Returns the bind error if the listener cannot be created.
    pub async fn serve(mut self) -> std::io::Result<()> {
        // Negotiate optional host-features against what the backend can actually
        // bridge. This is the honest-banner step: we only advertise `sync_v2` /
        // `shell_v2` if the backend reports it implements them, so a client never
        // negotiates a richer wire framing the bridge cannot satisfy.
        let backend_caps = self.backend.capabilities().await;
        self.caps = self.caps.negotiated_with(backend_caps);

        let listener = TcpListener::bind(self.addr).await?;
        let actual = listener.local_addr().unwrap_or(self.addr);
        tracing::info!(
            "adb server frontend listening on {actual} (features: {})",
            self.caps.features_csv()
        );

        // Unless the policy is Retain (caller manages release itself), subscribe
        // to the backend's lifecycle stream and spawn the disconnect handler:
        // when a device's transport vanishes, release (or notify about) its
        // forward/reverse rules. Spawned before the accept loop so a disconnect
        // is handled even with no clients connected.
        if !matches!(self.on_disconnect, OnDisconnect::Retain) {
            let events = self.backend.subscribe_lifecycle().await;
            let handle = self.handle();
            let policy = self.on_disconnect.clone();
            tokio::spawn(handle_disconnects(events, handle, policy));
        }

        let shared = Arc::new(self);
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    continue;
                }
            };
            // SEG A: low-latency interactive echo for this client socket.
            enable_client_nodelay(&stream, peer);
            let me = Arc::clone(&shared);
            tokio::spawn(async move {
                if let Err(e) = me.handle_client(stream).await {
                    tracing::debug!("client {peer} ended: {e}");
                }
            });
        }
    }

    /// Per-client state machine: one host service, then optionally a local
    /// service if a transport was selected.
    ///
    /// Crate-visible under `test`/`test-support` so the in-memory
    /// [`SimDeviceBackend`](crate::server::sim_backend) harness (a sibling module)
    /// can drive one client connection end-to-end without going through
    /// [`Self::serve`]'s accept loop; otherwise private (this is not a stable
    /// public API).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn handle_client(&self, stream: TcpStream) -> std::io::Result<()> {
        self.handle_client_impl(stream).await
    }

    /// Private production entry point (the accept loop calls this directly).
    #[cfg(not(any(test, feature = "test-support")))]
    async fn handle_client(&self, stream: TcpStream) -> std::io::Result<()> {
        self.handle_client_impl(stream).await
    }

    /// The actual per-client state machine body.
    async fn handle_client_impl(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let Some(service) = read_request(&mut stream).await? else {
            return Ok(()); // clean EOF before any request
        };
        match self.dispatch_host_service(&mut stream, &service).await? {
            HostOutcome::Close => Ok(()),
            HostOutcome::TransportSelected(serial) => {
                let Some(local) = read_request(&mut stream).await? else {
                    return Ok(()); // client selected a transport then hung up
                };
                // After selecting a transport, a client may still issue a `host:*`
                // request on the same connection rather than a local service to
                // bridge. AOSP `adb` does this: e.g. `ADBProxyDevice::shell_command`
                // sends `host:transport:<serial>` then `host:features` to decide
                // shell v1 vs v2, and `ADBProxyDevice::forward` sends
                // `host:transport:<serial>` then `host:forward:...`. Route these
                // post-transport host requests to their host handlers against the
                // already-chosen serial instead of `map_local_service` (which would
                // reject them as "service not supported").
                if let Some(svc) = local.strip_prefix("host:") {
                    if is_forward_family(svc) {
                        return match svc {
                            "list-forward" => self.serve_list_forward(&mut stream).await,
                            "killforward-all" => self.serve_killforward_all(&mut stream).await,
                            _ => self.serve_forward_family(&mut stream, svc, &serial).await,
                        };
                    }
                    // `host:features` / `host:version` are answered for the
                    // already-chosen transport. `features` is **per-device**: the
                    // transport is selected, so we intersect the server's features
                    // with this device's banner — native `adb -s <serial> shell`
                    // reads exactly this reply to pick shell v1 vs v2, so a device
                    // lacking `shell_v2` is correctly steered to v1 here.
                    match svc {
                        "features" => {
                            let csv = self.device_features_csv(&serial).await;
                            stream
                                .write_all(&reply_or_overflow(protocol::okay_data(&csv)))
                                .await?;
                            return Ok(());
                        }
                        "version" => {
                            stream
                                .write_all(&reply_or_overflow(protocol::okay_data(
                                    self.caps.version_hex(),
                                )))
                                .await?;
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                self.serve_local_service(stream, &local, &serial).await
            }
        }
    }

    /// Dispatch a `host:*` / `host-serial:*` request. Either terminal (writes a
    /// reply, returns [`HostOutcome::Close`]) or a transport selection (writes a
    /// reply, returns [`HostOutcome::TransportSelected`]).
    async fn dispatch_host_service(
        &self,
        stream: &mut TcpStream,
        service: &str,
    ) -> std::io::Result<HostOutcome> {
        // The pinned-device family prefixes (`host-serial:`/`host-usb:`/
        // `host-local:`/`host-transport-id:`) all resolve one device and run the
        // sub-service against it; handled together so this dispatcher stays focused
        // on the `host:` services proper.
        if let Some(outcome) = self.dispatch_pinned_prefix(stream, service).await? {
            return Ok(outcome);
        }

        let Some(svc) = service.strip_prefix("host:") else {
            stream
                .write_all(&protocol::fail(&format!("unknown service: {service}")))
                .await?;
            return Ok(HostOutcome::Close);
        };

        // Simple host *data queries* (version/features/devices/devices-l) all
        // share the `OKAY` + framed-payload shape; compute the payload here and
        // emit it uniformly, keeping this dispatcher focused on routing.
        if let Some(payload) = self.host_data_query_payload(svc).await {
            stream
                .write_all(&reply_or_overflow(protocol::okay_data(&payload)))
                .await?;
            return Ok(HostOutcome::Close);
        }

        match svc {
            "track-devices" => {
                self.serve_track_devices(stream).await?;
                Ok(HostOutcome::Close)
            }
            "kill" => {
                match self.caps.kill_policy() {
                    KillPolicy::Reject => {
                        stream
                            .write_all(&protocol::fail("kill not permitted"))
                            .await?;
                    }
                    KillPolicy::Shutdown => {
                        stream.write_all(&protocol::okay()).await?;
                        // The accept loop owns process lifetime; signal-based
                        // shutdown is the CLI's job. Here we just accept and let
                        // the socket close. A richer takeover hook can be added
                        // when a shutdown channel is threaded through.
                        tracing::info!("host:kill accepted (KillPolicy::Shutdown)");
                    }
                }
                Ok(HostOutcome::Close)
            }
            "transport-any" => self.select_transport_any(stream).await,
            "transport-usb" => {
                self.select_transport_kind(stream, Some(TransportKind::Usb))
                    .await
            }
            "transport-local" => {
                self.select_transport_kind(stream, Some(TransportKind::Local))
                    .await
            }
            _ if svc.starts_with("transport-id:") => {
                let id_str = &svc["transport-id:".len()..];
                self.select_transport_by_id(stream, id_str).await
            }
            _ if svc.starts_with("transport:") => {
                let serial = svc["transport:".len()..].to_string();
                self.select_transport_by_serial(stream, serial).await
            }
            _ if svc.starts_with("tport:") => {
                self.select_tport(stream, &svc["tport:".len()..]).await
            }
            _ if is_forward_family(svc) => {
                // `host:*forward*` with no explicit serial: resolve against the
                // single connected device (transport-any semantics).
                self.serve_host_forward(stream, svc).await?;
                Ok(HostOutcome::Close)
            }
            "reconnect-offline" => {
                // We model no "offline" devices (a USB/TCP device is either listed
                // as `device` or absent), so this is a success no-op: reply a bare
                // OKAY (the status the client reads). Matches `ADBProxyServer::
                // reconnect_offline`, which reads exactly one OKAY.
                stream.write_all(&protocol::okay()).await?;
                Ok(HostOutcome::Close)
            }
            _ if svc.starts_with("wait-for-") => {
                // Top-level `host:wait-for-*` carries no serial — it waits on the
                // device *set* filtered by transport kind (pinned_serial = None).
                self.serve_wait_for(stream, &svc["wait-for-".len()..], None)
                    .await?;
                Ok(HostOutcome::Close)
            }
            _ if svc.starts_with("connect:") => {
                let addr = &svc["connect:".len()..];
                self.serve_connect(stream, addr).await?;
                Ok(HostOutcome::Close)
            }
            _ if svc.starts_with("disconnect:") => {
                let addr = &svc["disconnect:".len()..];
                self.serve_disconnect(stream, addr).await?;
                Ok(HostOutcome::Close)
            }
            other => {
                stream
                    .write_all(&protocol::fail(&format!("unknown host service: {other}")))
                    .await?;
                Ok(HostOutcome::Close)
            }
        }
    }

    /// Payload for a simple host *data query* (`version`/`features`/`devices`/
    /// `devices-l`), or `None` if `svc` is not one. The caller frames it as
    /// `OKAY` + `%04x`+payload — these four share that exact shape.
    async fn host_data_query_payload(&self, svc: &str) -> Option<String> {
        match svc {
            "version" => Some(self.caps.version_hex().to_string()),
            "features" => Some(self.caps.features_csv()),
            "devices" => Some(format_devices(&self.backend.list_devices().await, false)),
            "devices-l" => Some(format_devices(&self.backend.list_devices().await, true)),
            _ => None,
        }
    }

    /// Route the pinned-device family prefixes — `host-serial:`/`host-usb:`/
    /// `host-local:`/`host-transport-id:` — that each name a single device (by
    /// serial, kind, or transport id) and then run a sub-service against it.
    /// Returns `Ok(Some(outcome))` when `service` matched one of these prefixes,
    /// `Ok(None)` when it did not (so the caller falls through to `host:`).
    async fn dispatch_pinned_prefix(
        &self,
        stream: &mut TcpStream,
        service: &str,
    ) -> std::io::Result<Option<HostOutcome>> {
        // `host-serial:<serial>:<sub>` carries its own serial. The serial may
        // itself contain colons (TCP/IP `ip:port`), so split on the known
        // sub-service anchor rather than the first colon.
        if let Some(rest) = service.strip_prefix("host-serial:") {
            if let Some((serial, sub)) = split_host_serial(rest) {
                return Ok(Some(self.dispatch_host_serial(stream, serial, sub).await?));
            }
            stream
                .write_all(&protocol::fail("malformed host-serial request"))
                .await?;
            return Ok(Some(HostOutcome::Close));
        }

        // `host-usb:<sub>` / `host-local:<sub>` pin the device by transport *kind*
        // (native `adb -d`/`-e` phase 1): resolve the one matching device, then run
        // the same sub-service against its serial.
        for (prefix, kind) in [
            ("host-usb:", TransportKind::Usb),
            ("host-local:", TransportKind::Local),
        ] {
            if let Some(sub) = service.strip_prefix(prefix) {
                return Ok(Some(self.dispatch_host_kind(stream, kind, sub).await?));
            }
        }

        // `host-transport-id:<N>:<sub>` is the transport-id-pinned analogue,
        // emitted by modern `adb` during the `adb root` reconnect handshake
        // (`host-transport-id:<N>:wait-for-any-disconnect`).
        if let Some(rest) = service.strip_prefix("host-transport-id:") {
            return Ok(Some(self.dispatch_host_transport_id(stream, rest).await?));
        }

        Ok(None)
    }

    /// `host-serial:<serial>:<sub>` single-device queries.
    async fn dispatch_host_serial(
        &self,
        stream: &mut TcpStream,
        serial: &str,
        sub: &str,
    ) -> std::io::Result<HostOutcome> {
        let devices = self.backend.list_devices().await;
        let entry = devices.iter().find(|d| d.serial == serial);
        match sub {
            "get-state" => {
                let state = entry.map_or("offline", |d| d.state.as_wire());
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(state)))
                    .await?;
            }
            "get-serialno" => {
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(serial)))
                    .await?;
            }
            "features" => {
                // Per-device: `host-serial:<serial>:features` names its device,
                // so intersect with that device's banner (same honest reply as the
                // post-transport `host:features` path).
                let csv = self.device_features_csv(serial).await;
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(&csv)))
                    .await?;
            }
            "list-forward" => {
                self.serve_list_forward(stream).await?;
            }
            "killforward-all" => {
                self.serve_killforward_all(stream).await?;
            }
            _ if sub.starts_with("forward:") || sub.starts_with("killforward:") => {
                // `host-serial:<serial>:forward:...` — the serial is explicit, so
                // it must actually exist before we bind anything.
                if entry.is_none() {
                    stream
                        .write_all(&protocol::fail("device not found"))
                        .await?;
                    return Ok(HostOutcome::Close);
                }
                self.serve_forward_family(stream, sub, serial).await?;
            }
            _ if sub.starts_with("transport") || sub == "tport" => {
                // `host-serial:<serial>:transport` selects that device.
                return self
                    .select_transport_by_serial(stream, serial.to_string())
                    .await;
            }
            _ if sub.starts_with("wait-for-") => {
                // `host-serial:<serial>:wait-for-<transport>-<state>` (and the
                // `host-transport-id:` family that resolves through here) pins the
                // wait to a specific serial. This is the path the `adb root`
                // reconnect handshake takes: `wait-for-any-disconnect` blocks until
                // *this* serial vanishes from the device list. Share
                // `serve_wait_for`, passing the pinned serial.
                self.serve_wait_for(stream, &sub["wait-for-".len()..], Some(serial))
                    .await?;
            }
            other => {
                stream
                    .write_all(&protocol::fail(&format!(
                        "unknown host-serial sub-service: {other}"
                    )))
                    .await?;
            }
        }
        Ok(HostOutcome::Close)
    }

    /// `host-usb:<sub>` / `host-local:<sub>` single-device queries, pinned by
    /// transport `kind` (native `adb -d` / `adb -e`). Resolves the one device of
    /// that kind — replying `FAIL` with the kind-specific AOSP wording on zero /
    /// more-than-one — then runs the sub-service through [`Self::dispatch_host_serial`]
    /// so kind- and serial-pinned queries share identical sub-service semantics.
    async fn dispatch_host_kind(
        &self,
        stream: &mut TcpStream,
        kind: TransportKind,
        sub: &str,
    ) -> std::io::Result<HostOutcome> {
        match self.resolve_single_by_kind(Some(kind)).await {
            Ok(serial) => self.dispatch_host_serial(stream, &serial, sub).await,
            Err(reason) => {
                stream.write_all(&protocol::fail(reason)).await?;
                Ok(HostOutcome::Close)
            }
        }
    }

    /// `host-transport-id:<N>:<sub>` single-device queries, pinned by transport
    /// *id*. `rest` is the `<N>:<sub>` tail after the prefix. Resolves N → serial
    /// (reusing [`Self::serial_for_transport_id`]) and runs the sub-service through
    /// [`Self::dispatch_host_serial`], the same funnel [`Self::dispatch_host_kind`]
    /// uses for kind-pinned queries. Unlike `host-serial:`, `<N>` is a bare u64
    /// that never contains a colon, so a plain `split_once(':')` is correct.
    /// Failure wording mirrors [`Self::select_transport_by_id`] so every id-keyed
    /// path reports the same AOSP-aligned errors.
    async fn dispatch_host_transport_id(
        &self,
        stream: &mut TcpStream,
        rest: &str,
    ) -> std::io::Result<HostOutcome> {
        let Some((id_str, sub)) = rest.split_once(':') else {
            stream
                .write_all(&protocol::fail("malformed host-transport-id request"))
                .await?;
            return Ok(HostOutcome::Close);
        };
        let Ok(id) = id_str.parse::<u64>() else {
            stream
                .write_all(&protocol::fail("invalid transport id"))
                .await?;
            return Ok(HostOutcome::Close);
        };
        if let Some(serial) = self.serial_for_transport_id(id).await {
            self.dispatch_host_serial(stream, &serial, sub).await
        } else {
            stream
                .write_all(&protocol::fail("no device for transport id"))
                .await?;
            Ok(HostOutcome::Close)
        }
    }

    /// Handle a `host:`-prefixed forward-family service that carries no explicit
    /// serial. `list-forward` / `killforward-all` are device-independent; the
    /// per-rule `forward:` / `killforward:` resolve against the single device.
    async fn serve_host_forward(&self, stream: &mut TcpStream, svc: &str) -> std::io::Result<()> {
        match svc {
            "list-forward" => self.serve_list_forward(stream).await,
            "killforward-all" => self.serve_killforward_all(stream).await,
            _ => {
                let serial = match self.resolve_single_serial().await {
                    Ok(s) => s,
                    Err(reason) => return stream.write_all(&protocol::fail(&reason)).await,
                };
                self.serve_forward_family(stream, svc, &serial).await
            }
        }
    }

    /// `host:connect:<addr>` — ask the backend to connect a TCP/IP device. AOSP
    /// replies `OKAY` + a framed human-readable status string (e.g. `connected to
    /// 127.0.0.1:5555`), which `adb` prints verbatim; a backend error is a FAIL.
    async fn serve_connect(&self, stream: &mut TcpStream, addr: &str) -> std::io::Result<()> {
        match self.backend.connect(addr).await {
            Ok(status) => {
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(&status)))
                    .await
            }
            Err(e) => stream.write_all(&protocol::fail(&format!("{e}"))).await,
        }
    }

    /// `host:disconnect:<addr>` — ask the backend to drop a TCP/IP device (empty
    /// `addr` drops all). Same `OKAY` + framed status framing as connect.
    async fn serve_disconnect(&self, stream: &mut TcpStream, addr: &str) -> std::io::Result<()> {
        match self.backend.disconnect(addr).await {
            Ok(status) => {
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(&status)))
                    .await
            }
            Err(e) => stream.write_all(&protocol::fail(&format!("{e}"))).await,
        }
    }

    /// `host:wait-for-<transport>-<state>` — block until a device matching the
    /// request reaches `<state>`, then reply `OKAY`.
    ///
    /// `arg` is the suffix after `wait-for-`, i.e. `<transport>-<state>`
    /// (`any-device`, `usb-device`, `local-device`, `any-disconnect`, …).
    ///
    /// Two states are observable by this backend:
    /// - `device`: a device of the requested kind is *present* in the list. We
    ///   cannot see recovery/sideload/bootloader, so those FAIL fast. Polled
    ///   (`POLL_INTERVAL` / `MAX_WAIT`); a never-arriving device is a single FAIL.
    /// - `disconnect`: the target's transport is *torn down*. This is the state the
    ///   `adb root` / `adb unroot` reconnect handshake waits on — after adbd
    ///   restarts, the cached connection's reader/writer die. AOSP's server pins
    ///   this to the exact transport it just talked to and detects teardown at the
    ///   I/O layer (sub-second, never polling); we mirror that with an entry
    ///   `transport_alive` check + a `TransportReset` lifecycle event, bounded by a
    ///   10s `DISCONNECT_FALLBACK` (not 60s, and not presence-polling — see below).
    ///
    /// `pinned_serial` selects the target:
    /// - `Some(s)`: the request named a specific device (`host-serial:<s>:` or
    ///   `host-transport-id:<N>:` resolved to `s`). `disconnect` waits for *that*
    ///   serial's transport to die.
    /// - `None`: top-level `host:wait-for-*` with no serial — the wait is over the
    ///   device *set* filtered by transport kind ([`kind_matches`]).
    ///
    /// The transport token *is* honored for the kind-filtered (`None`) paths:
    /// `wait-for-usb-device` waits for a USB device specifically.
    ///
    /// Framing: **two** bare OKAYs once the wait is satisfied (no length-prefixed
    /// payload), via [`protocol::okay_twice`]. AOSP's client reads two OKAYs for
    /// `wait-for-*` (accept + satisfied); adboost does NOT emit a blanket accept
    /// OKAY at the smartsocket layer (the old doc comment here wrongly claimed it
    /// did — `handle_client` dispatches straight to the service), so each service
    /// that needs two emits them itself, exactly as the `forward` family does
    /// (`okay_twice`). Sending only one desyncs modern clients
    /// (`error: protocol fault (couldn't read status)`).
    async fn serve_wait_for(
        &self,
        stream: &mut TcpStream,
        arg: &str,
        pinned_serial: Option<&str>,
    ) -> std::io::Result<()> {
        // Poll cadence + overall bound for the `device`-present branch (a device
        // appearing is observable only by polling enumeration).
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
        const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
        // Bounded fallback for the event-driven `disconnect` branch. PR0
        // real-hardware data: the connection died within 250 ms max on every adbd
        // restart, so 10 s is generous headroom while being far shorter than the
        // old 60 s presence ceiling. It only ever fires when adbd did NOT actually
        // restart (a no-op `root`), in which case we still return cleanly (no FAIL)
        // to match native adb's "assume disconnected" semantics.
        const DISCONNECT_FALLBACK: std::time::Duration = std::time::Duration::from_secs(10);

        // Split `<transport>-<state>` on the LAST '-': the state is a single token
        // (device/recovery/sideload/bootloader/disconnect); the transport may be
        // any/usb/local. A bare arg with no '-' (e.g. a hand-typed
        // `wait-for-device`) is treated as the state directly — real `adb` always
        // sends the `<transport>-<state>` form, but accepting the bare state is
        // harmless and more forgiving.
        let (want, state) = arg
            .rsplit_once('-')
            .map_or((None, arg), |(transport, state)| {
                (parse_transport_kind(transport), state)
            });

        // `disconnect`: wait until the target transport is torn down, then OKAY.
        //
        // This is EVENT-DRIVEN (mirroring native adb's transport teardown), NOT a
        // presence poll. The old presence poll (`list_devices` until the serial
        // vanished) was fundamentally broken: an `adb root`/`unroot` restarts adbd
        // but on most devices (MTK et al.) the USB device never re-enumerates, so
        // the serial stays listed forever and the wait hung the full 60 s (the
        // reported bug). PR0 proved the serial never left enumeration on 19/20
        // restart cycles.
        //
        // Two signals, ordered by PR0 data:
        //   1. PRIMARY — entry `transport_alive` check. The connection death
        //      routinely PRECEDES the `wait-for-disconnect` request (5/20 cycles
        //      the reader died before adbd's reply was even read), so checking on
        //      entry catches the common case immediately. Subscribe BEFORE this
        //      check so a death racing in just after it is still caught by the
        //      event (broadcast does not replay → TOCTOU-free only with this
        //      ordering).
        //   2. SECONDARY — a `TransportReset` (or a real `Disconnected`) lifecycle
        //      event, for the minority case where the wait arrives before the
        //      death. No generation counter is needed: between `root:` and
        //      `wait-for-disconnect` the client sends no device command, so no
        //      reopen can race in and mask the death (PRD R6).
        //
        // No FAIL on this branch: both satisfaction and the bounded fallback send
        // two OKAYs (the fallback assumes "disconnected, return cleanly", matching
        // native; logged at WARN so a never-restarting adbd is still diagnosable).
        if state == "disconnect" {
            let mut events = self.backend.subscribe_lifecycle().await;

            // Entry check (primary path). `target_matches` decides which serial(s)
            // satisfy the wait.
            let alive = match pinned_serial {
                Some(s) => self.backend.transport_alive(s).await,
                // Kind-filtered top-level wait: "alive" iff some device of the
                // requested kind is still present (no per-connection liveness to
                // consult without a pinned serial).
                None => self
                    .backend
                    .list_devices()
                    .await
                    .iter()
                    .any(|d| kind_matches(want, d.kind)),
            };
            if !alive {
                return stream.write_all(&protocol::okay_twice()).await;
            }

            let started = tokio::time::Instant::now();
            let deadline = started + DISCONNECT_FALLBACK;
            let target_matches = |s: &str| match pinned_serial {
                Some(pinned) => s == pinned,
                // Kind-filtered: any reset is a candidate; we cannot cheaply map a
                // bare serial back to its kind here, so accept it (the pinned case
                // — what the real `adb root` handshake uses — is the precise one).
                None => true,
            };
            loop {
                tokio::select! {
                    ev = events.recv() => match ev {
                        Some(LifecycleEvent::TransportReset(s)) if target_matches(&s) => {
                            return stream.write_all(&protocol::okay_twice()).await;
                        }
                        // A genuine unplug also satisfies a disconnect wait.
                        Some(LifecycleEvent::Disconnected(s)) if target_matches(&s) => {
                            return stream.write_all(&protocol::okay_twice()).await;
                        }
                        // Other serial / non-matching event: keep waiting (loop).
                        Some(_) => {}
                        // Lifecycle stream closed (server teardown): return cleanly.
                        None => return stream.write_all(&protocol::okay_twice()).await,
                    },
                    () = tokio::time::sleep_until(deadline) => {
                        tracing::warn!(
                            serial = ?pinned_serial,
                            waited_ms = started.elapsed().as_millis(),
                            "wait-for-disconnect fallback fired (no transport-reset signal; \
                             adbd may not have restarted); assuming disconnected and returning"
                        );
                        return stream.write_all(&protocol::okay_twice()).await;
                    }
                }
            }
        }

        if state != "device" {
            return stream
                .write_all(&protocol::fail(&format!(
                    "wait-for-{state} not supported (this server only observes the 'device' and 'disconnect' states)"
                )))
                .await;
        }

        // Poll the device set until at least one device of the requested kind is
        // present, bounded so a never-arriving device does not pin the connection
        // forever.
        let deadline = tokio::time::Instant::now() + MAX_WAIT;
        loop {
            if self
                .backend
                .list_devices()
                .await
                .iter()
                .any(|d| kind_matches(want, d.kind))
            {
                // Satisfaction: two bare OKAYs (accept + satisfied), like the
                // disconnect branch and the `forward` family (R1).
                return stream.write_all(&protocol::okay_twice()).await;
            }
            if tokio::time::Instant::now() >= deadline {
                // The device-present branch KEEPS a single FAIL on timeout: a
                // device that never appeared is a genuine failure (unlike the
                // disconnect branch, where the fallback means "assume disconnected").
                return stream
                    .write_all(&protocol::fail("wait-for timed out"))
                    .await;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Resolve the single device matching a requested transport kind, or an
    /// AOSP-style failure reason when zero or multiple match. This is the single
    /// `acquire_one_transport` analogue every transport-selection path funnels
    /// through, so they all agree on the zero/one/many wording.
    ///
    /// `want == None` is transport-any (the default / `-s` / forward paths);
    /// `Some(Usb)`/`Some(Local)` are `adb -d` / `adb -e`. The error wording is
    /// kind-specific to match native `adb` ([`no_devices_msg`]/[`ambiguous_msg`]).
    async fn resolve_single_by_kind(
        &self,
        want: Option<TransportKind>,
    ) -> Result<String, &'static str> {
        let devices = self.backend.list_devices().await;
        pick_single_by_kind(&devices, want).map(|d| d.serial.clone())
    }

    /// Resolve the single connected device's serial (transport-any semantics),
    /// or an AOSP-style failure reason when there are zero or multiple devices.
    async fn resolve_single_serial(&self) -> Result<String, String> {
        self.resolve_single_by_kind(None)
            .await
            .map_err(str::to_string)
    }

    /// Dispatch a forward-family service (`forward:...` / `killforward:...`) for
    /// an already-resolved `serial`. `svc` is the service *without* the `host:`
    /// or `host-serial:<serial>:` prefix.
    async fn serve_forward_family(
        &self,
        stream: &mut TcpStream,
        svc: &str,
        serial: &str,
    ) -> std::io::Result<()> {
        if let Some(arg) = svc.strip_prefix("forward:") {
            self.serve_forward_add(stream, arg, serial).await
        } else if let Some(arg) = svc.strip_prefix("killforward:") {
            self.serve_killforward(stream, arg).await
        } else {
            // Unreachable given the call sites' guards, but fail cleanly.
            stream
                .write_all(&protocol::fail(&format!("bad forward service: {svc}")))
                .await
        }
    }

    /// `forward:[norebind:]tcp:<local>;tcp:<remote>` — bind a host listener and
    /// register the rule. Success replies two OKAYs (plus the resolved port when
    /// the local port was `0`); errors reply a single FAIL.
    async fn serve_forward_add(
        &self,
        stream: &mut TcpStream,
        arg: &str,
        serial: &str,
    ) -> std::io::Result<()> {
        let req = match parse_forward(arg) {
            Ok(r) => r,
            Err(reason) => return stream.write_all(&protocol::fail(&reason)).await,
        };

        // Enforce `norebind` against an existing rule BEFORE binding.
        if req.norebind && self.forwards.contains(req.local_port).await {
            return stream
                .write_all(&protocol::fail("cannot rebind existing socket"))
                .await;
        }

        // Bind the host-side listener (port 0 ⇒ OS auto-assign).
        let bind_addr = SocketAddr::from(([127, 0, 0, 1], req.local_port));
        let listener = match TcpListener::bind(bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                return stream
                    .write_all(&protocol::fail(&format!("cannot bind listener: {e}")))
                    .await;
            }
        };
        let resolved_port = listener.local_addr().map_or(req.local_port, |a| a.port());

        // Spawn the accept loop: each inbound connection opens `tcp:<remote>` on
        // the device and bridges the two byte streams.
        let backend = Arc::clone(&self.backend);
        let serial_owned = serial.to_string();
        let remote_port = req.remote_port;
        let task = tokio::spawn(async move {
            run_forward_listener(listener, backend, serial_owned, remote_port).await;
        });

        self.forwards
            .insert(resolved_port, req.remote_port, serial.to_string(), task)
            .await;

        // Reply: two OKAYs, plus the resolved port iff the client asked for tcp:0.
        if req.local_is_zero {
            stream
                .write_all(&protocol::okay_twice_with_port(resolved_port))
                .await
        } else {
            stream.write_all(&protocol::okay_twice()).await
        }
    }

    /// `killforward:tcp:<local>` — remove a rule (aborting its listener).
    async fn serve_killforward(&self, stream: &mut TcpStream, arg: &str) -> std::io::Result<()> {
        let local_port = match parse_killforward(arg) {
            Ok(p) => p,
            Err(reason) => return stream.write_all(&protocol::fail(&reason)).await,
        };
        if self.forwards.remove(local_port).await {
            stream.write_all(&protocol::okay_twice()).await
        } else {
            stream
                .write_all(&protocol::fail(&format!(
                    "listener 'tcp:{local_port}' not found"
                )))
                .await
        }
    }

    /// `killforward-all` — remove every rule.
    async fn serve_killforward_all(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        self.forwards.remove_all().await;
        stream.write_all(&protocol::okay_twice()).await
    }

    /// `list-forward` — a SINGLE OKAY + framed body (the forward-family
    /// exception; see the AOSP framing research).
    async fn serve_list_forward(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        let body = self.forwards.list().await;
        stream
            .write_all(&reply_or_overflow(protocol::okay_data(&body)))
            .await
    }

    /// `host:transport-any` — select the single connected device (error if none
    /// or more than one), regardless of transport kind.
    async fn select_transport_any(&self, stream: &mut TcpStream) -> std::io::Result<HostOutcome> {
        self.select_transport_kind(stream, None).await
    }

    /// `host:transport-usb` / `host:transport-local` / `host:transport-any` —
    /// select the single device matching `want` (bare `OKAY`), or FAIL with the
    /// kind-specific AOSP wording on zero / more-than-one. `want == None` is
    /// `transport-any`.
    async fn select_transport_kind(
        &self,
        stream: &mut TcpStream,
        want: Option<TransportKind>,
    ) -> std::io::Result<HostOutcome> {
        match self.resolve_single_by_kind(want).await {
            Ok(serial) => {
                stream.write_all(&protocol::okay()).await?;
                Ok(HostOutcome::TransportSelected(serial))
            }
            Err(reason) => {
                stream.write_all(&protocol::fail(reason)).await?;
                Ok(HostOutcome::Close)
            }
        }
    }

    /// `host:transport:<serial>` — select a device by serial (bare OKAY).
    async fn select_transport_by_serial(
        &self,
        stream: &mut TcpStream,
        serial: String,
    ) -> std::io::Result<HostOutcome> {
        let devices = self.backend.list_devices().await;
        if devices.iter().any(|d| d.serial == serial) {
            stream.write_all(&protocol::okay()).await?;
            Ok(HostOutcome::TransportSelected(serial))
        } else {
            stream
                .write_all(&protocol::fail("device not found"))
                .await?;
            Ok(HostOutcome::Close)
        }
    }

    /// `host:transport-id:<N>` — select by 1-based sorted transport id (bare OKAY).
    async fn select_transport_by_id(
        &self,
        stream: &mut TcpStream,
        id_str: &str,
    ) -> std::io::Result<HostOutcome> {
        let Ok(id) = id_str.parse::<u64>() else {
            stream
                .write_all(&protocol::fail("invalid transport id"))
                .await?;
            return Ok(HostOutcome::Close);
        };
        if let Some(serial) = self.serial_for_transport_id(id).await {
            stream.write_all(&protocol::okay()).await?;
            Ok(HostOutcome::TransportSelected(serial))
        } else {
            stream
                .write_all(&protocol::fail("no device for transport id"))
                .await?;
            Ok(HostOutcome::Close)
        }
    }

    /// `host:tport:*` — like transport selection but replies OKAY + 8-byte LE id.
    /// The `rest` is the same selector tail as the `transport*` variants
    /// (`<serial>`, `-any`, `-id:<N>`, or empty for any).
    async fn select_tport(
        &self,
        stream: &mut TcpStream,
        rest: &str,
    ) -> std::io::Result<HostOutcome> {
        let devices = self.backend.list_devices().await;
        let serials: Vec<String> = devices.iter().map(|d| d.serial.clone()).collect();

        // Each selector resolves to the chosen serial, or its own AOSP-correct
        // failure reason. The `any`/empty branch must distinguish zero from
        // more-than-one (matching `select_transport_any`); the `id:` branch matches
        // `select_transport_by_id`'s messages. Collapsing these into a single
        // `Option` would misreport every case as "device not found" (e.g. `adb
        // shell` with no `-s` on multiple devices).
        let chosen: std::result::Result<String, &str> =
            if rest.is_empty() || rest == "any" || rest == "-any" {
                match devices.as_slice() {
                    [] => Err(no_devices_msg(None)),
                    [one] => Ok(one.serial.clone()),
                    _ => Err(ambiguous_msg(None)),
                }
            } else if let Some(id_str) = rest
                .strip_prefix("id:")
                .or_else(|| rest.strip_prefix("-id:"))
            {
                match id_str.parse::<u64>() {
                    Ok(id) => protocol::transport_id_for_index(id, &serials)
                        .ok_or("no device for transport id"),
                    Err(_) => Err("invalid transport id"),
                }
            } else if let Some(kind) = parse_transport_kind(rest) {
                // Bare `usb`/`local` kind tokens — modern `adb -d`/`-e` phase 2
                // (`host:tport:usb` / `host:tport:local`, confirmed via
                // ADB_TRACE on adb 35.0.2). Route through the same shared kind
                // resolver as `transport-usb`/`transport-local`, reusing the
                // already-fetched `devices` (no second `list_devices()`). Only
                // the *bare* tokens are kinds; the explicit `serial:` form below
                // still resolves a device literally named `usb`/`local`.
                pick_single_by_kind(&devices, Some(kind)).map(|d| d.serial.clone())
            } else {
                // `tport:serial:<serial>` or `tport:<serial>`
                let serial = rest.strip_prefix("serial:").unwrap_or(rest);
                devices
                    .iter()
                    .find(|d| d.serial == serial)
                    .map(|d| d.serial.clone())
                    .ok_or("device not found")
            };

        match chosen {
            Ok(serial) => {
                let id = protocol::transport_id_for(&serial, &serials).unwrap_or(0);
                stream.write_all(&protocol::okay_tport(id)).await?;
                Ok(HostOutcome::TransportSelected(serial))
            }
            Err(reason) => {
                stream.write_all(&protocol::fail(reason)).await?;
                Ok(HostOutcome::Close)
            }
        }
    }

    /// Map a 1-based transport id back to a serial over the current sorted set.
    async fn serial_for_transport_id(&self, id: u64) -> Option<String> {
        let serials: Vec<String> = self
            .backend
            .list_devices()
            .await
            .into_iter()
            .map(|d| d.serial)
            .collect();
        protocol::transport_id_for_index(id, &serials)
    }

    /// `host:track-devices` — write OKAY, then a full snapshot on every change.
    async fn serve_track_devices(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        stream.write_all(&protocol::okay()).await?;
        let mut rx = self.backend.subscribe_changes().await;
        while let Some(snapshot) = rx.recv().await {
            let listing = format_devices(&snapshot, false);
            let Some(frame) = protocol::encode_framed(&listing) else {
                tracing::warn!("track-devices snapshot too large to frame, skipping");
                continue;
            };
            stream.write_all(&frame).await?;
            stream.flush().await?;
        }
        Ok(())
    }

    /// Map a post-transport local service to a bridged session, or FAIL.
    ///
    /// The server is a transparent byte pipe for local services: `sync:` and
    /// `shell,v2` are bridged verbatim (the client and device speak the SYNC /
    /// shell-v2 sub-protocol end-to-end; the server only relays bytes). They are
    /// gated on the negotiated `host:features` so we never accept a richer
    /// framing we did not advertise (honest banner).
    async fn serve_local_service(
        &self,
        mut stream: TcpStream,
        service: &str,
        serial: &str,
    ) -> std::io::Result<()> {
        // `reverse:*` is a control service handled against the backend's reverse
        // API (the device binds the listener; the backend pumps inbound opens).
        if service.starts_with("reverse:") {
            return self.serve_reverse(&mut stream, service, serial).await;
        }

        // Look up the target device's real capabilities so the gate below can
        // reject a wire-framing service (`shell,v2` / `sync:`) the device cannot
        // satisfy, instead of passing the OPEN through to be `CLSE`d. Only the two
        // framing services consult this, so the (possibly handshake-bound) query
        // is skipped entirely for v1 `shell:` / `tcp:` / control services.
        let device_caps = if service == "sync:" || service.starts_with("shell,") {
            self.device_capabilities(serial).await
        } else {
            None
        };

        let cmd = match self.map_local_service(service, device_caps.as_ref()) {
            Ok(cmd) => cmd,
            Err(reason) => {
                stream.write_all(&protocol::fail(&reason)).await?;
                return Ok(());
            }
        };

        // Open via the injected backend (reuses PersistentUsbConnection).
        let session = match self.backend.open_local_service(serial, &cmd).await {
            Ok(s) => s,
            Err(e) => {
                stream
                    .write_all(&protocol::fail(&format!("open session failed: {e}")))
                    .await?;
                return Ok(());
            }
        };

        // Service accepted — the socket is now a raw byte stream.
        stream.write_all(&protocol::okay()).await?;
        crate::usb::bridge_tcp_session(stream, session).await;
        Ok(())
    }

    /// Query the backend for a device's banner-advertised capabilities, bounded
    /// by [`DEVICE_CAPS_TIMEOUT`]. Thin wrapper that fixes the timeout policy in
    /// one place; returns `None` (unknown → conservative) on timeout / error.
    async fn device_capabilities(&self, serial: &str) -> Option<DeviceFeatureSet> {
        self.backend
            .device_capabilities(serial, DEVICE_CAPS_TIMEOUT)
            .await
    }

    /// Per-device feature CSV for a serial-aware `host:features` reply: the
    /// server's negotiated features intersected with what THIS device's banner
    /// advertises, so a client gating `adb shell` on `host:features` picks v1 for
    /// a device that lacks `shell_v2` (graceful, no failed OPEN).
    async fn device_features_csv(&self, serial: &str) -> String {
        let device_caps = self.device_capabilities(serial).await;
        self.caps
            .intersected_with_device(device_caps.as_ref())
            .join(",")
    }

    /// Pure mapping from a post-transport service string to the
    /// [`ADBLocalCommand`] to open, or an AOSP-style FAIL reason. Capability
    /// gating (sync/shell-v2) is consulted here so an un-advertised service is
    /// rejected before any device session is opened.
    ///
    /// `device_caps` is the target device's banner-advertised feature set
    /// (`None` when unknown — not yet handshaked). The two **wire-framing**
    /// services (`sync:`, `shell,v2`) require BOTH that the server advertised the
    /// feature AND that this device supports it: passing `shell,v2` to a device
    /// whose banner lacks it makes the device `CLSE` the OPEN (the bug this gate
    /// prevents). The primary defense is the per-device `host:features` reply
    /// (the client then picks v1 itself); this is the defense-in-depth fallback
    /// for a client that opens v2 anyway.
    fn map_local_service(
        &self,
        service: &str,
        device_caps: Option<&DeviceFeatureSet>,
    ) -> Result<ADBLocalCommand, String> {
        // `sync:` — bridged verbatim, only when `sync_v2` is advertised AND the
        // target device supports it.
        if service == "sync:" {
            return if self.caps.device_has_feature("sync_v2", device_caps) {
                Ok(ADBLocalCommand::Raw(service.to_string()))
            } else {
                Err(format!("service not supported: {service}"))
            };
        }
        // `shell,...` (shell-v2 and its modifiers) — verbatim, only when
        // `shell_v2` is advertised AND the target device supports it. Bare
        // `shell:` (v1) is handled below and works on every device.
        if service.starts_with("shell,") {
            return if self.caps.device_has_feature("shell_v2", device_caps) {
                Ok(ADBLocalCommand::Raw(service.to_string()))
            } else {
                Err(format!("service not supported: {service}"))
            };
        }
        // Bare `shell:` is v1 (ShellCommand with empty args), NOT v2.
        if let Some(shell_cmd) = service.strip_prefix("shell:") {
            return Ok(ADBLocalCommand::ShellCommand(shell_cmd.to_string(), vec![]));
        }
        if let Some(port_str) = service.strip_prefix("tcp:") {
            return port_str
                .parse::<u16>()
                .map(ADBLocalCommand::TcpConnect)
                .map_err(|_| format!("invalid tcp port: {service}"));
        }
        // Device **control services** (`tcpip:<port>`, `usb:`, `root:`,
        // `reboot:[mode]`, `remount:`, `enable-verity:`, `disable-verity:`) are
        // structurally identical to bare `shell:` v1 — one OPEN, a short textual
        // reply, then CLSE — so the transparent `bridge_tcp_session` already
        // handles them. They need no capability gating (every adbd supports them)
        // and are forwarded verbatim as `Raw`. Note `tcpip:`/`usb:`/`reboot:`
        // restart adbd, which drops the USB connection; the bridge observes EOF
        // and the cached connection self-heals on the next open (`get_or_open`).
        if is_control_service(service) {
            return Ok(ADBLocalCommand::Raw(service.to_string()));
        }
        // `reverse:*` is routed earlier (serve_reverse) and never reaches here.
        // jdwp/localabstract are not bridged. Everything else is a stable FAIL.
        Err(format!("service not supported: {service}"))
    }

    /// Handle a post-transport `reverse:*` control service against the backend.
    ///
    /// `reverse:forward:[norebind:]<remote>;<local>` sets up a device-side
    /// listener (the backend owns the inbound-open pump + host-dial bridge);
    /// `reverse:killforward:<remote>` / `reverse:killforward-all` remove rules;
    /// `reverse:list-forward` lists them.
    ///
    /// Reply framing matches AOSP so native `adb` stays in sync: forward /
    /// killforward / killforward-all reply **two** OKAYs (`okay_twice` — connect
    /// then status; native adb reads both, the proxy client reads the first and
    /// ignores the rest); `list-forward` replies a single OKAY + framed body.
    /// Errors are a single FAIL.
    async fn serve_reverse(
        &self,
        stream: &mut TcpStream,
        service: &str,
        serial: &str,
    ) -> std::io::Result<()> {
        let rest = &service["reverse:".len()..];
        // reverse:list-forward → OKAY + framed body of "(reverse) <remote> <local>" lines.
        if rest == "list-forward" {
            return match self.backend.list_reverse(serial).await {
                Ok(body) => {
                    stream
                        .write_all(&reply_or_overflow(protocol::okay_data(&body)))
                        .await
                }
                Err(e) => stream.write_all(&protocol::fail(&format!("{e}"))).await,
            };
        }
        // reverse:killforward-all
        if rest == "killforward-all" {
            return match self.backend.reverse_remove_all(serial).await {
                Ok(()) => stream.write_all(&protocol::okay_twice()).await,
                Err(e) => stream.write_all(&protocol::fail(&format!("{e}"))).await,
            };
        }
        // reverse:killforward:<remote>
        if let Some(remote) = rest.strip_prefix("killforward:") {
            return match self.backend.reverse_remove(serial, remote).await {
                Ok(()) => stream.write_all(&protocol::okay_twice()).await,
                Err(e) => stream.write_all(&protocol::fail(&format!("{e}"))).await,
            };
        }
        // reverse:forward:[norebind:]<remote>;<local>  (order: remote;local)
        if let Some(arg) = rest.strip_prefix("forward:") {
            let arg = arg.strip_prefix("norebind:").unwrap_or(arg);
            let Some((remote, local)) = arg.split_once(';') else {
                return stream
                    .write_all(&protocol::fail(&format!("bad reverse forward: {arg}")))
                    .await;
            };
            return match self.backend.open_reverse(serial, remote, local).await {
                Ok(()) => stream.write_all(&protocol::okay_twice()).await,
                Err(e) => stream.write_all(&protocol::fail(&format!("{e}"))).await,
            };
        }
        stream
            .write_all(&protocol::fail(&format!(
                "unsupported reverse service: {service}"
            )))
            .await
    }
}

/// Whether `service` is a device **control service** the server bridges
/// verbatim: a single OPEN that yields a short textual reply then CLSE, exactly
/// like bare `shell:` v1. These are the post-transport services behind
/// `adb tcpip <port>` / `adb usb` / `adb root` / `adb unroot` /
/// `adb reboot [mode]` / `adb remount` / `adb {enable,disable}-verity`.
///
/// `reboot:` is matched by prefix because the mode is appended
/// (`reboot:bootloader`, `reboot:` for a plain reboot). The others are exact:
/// `tcpip:` always carries a port, but the port is opaque to us (we forward the
/// whole string), so a prefix match is enough and keeps an empty/garbage port a
/// device-side error rather than a silent accept of an unrelated `tcpip`-prefixed
/// service. `usb:` / `root:` / `unroot:` / `remount:` / `*-verity:` take no
/// argument.
fn is_control_service(service: &str) -> bool {
    matches!(
        service,
        "usb:" | "root:" | "unroot:" | "remount:" | "enable-verity:" | "disable-verity:"
    ) || service.starts_with("tcpip:")
        || service.starts_with("reboot:")
}

/// Whether a (prefix-stripped) service is a member of the forward family:
/// `forward:` / `killforward:` (per-rule) or `list-forward` / `killforward-all`
/// (device-independent).
fn is_forward_family(svc: &str) -> bool {
    svc == "list-forward"
        || svc == "killforward-all"
        || svc.starts_with("forward:")
        || svc.starts_with("killforward:")
}

/// Whether `sub` is a sub-service recognized by [`dispatch_host_serial`]. Used
/// as the *anchor* to split `host-serial:<serial>:<sub>`: the serial itself may
/// contain colons (a TCP/IP `ip:port` device), so the split point cannot be the
/// first (or last) colon — it is the colon that separates the serial from a
/// known sub-service. Mirror the exact member set of `dispatch_host_serial`'s
/// `match sub`; keep them in lockstep.
fn is_host_serial_sub(sub: &str) -> bool {
    matches!(sub, "get-state" | "get-serialno" | "features")
        || sub.starts_with("transport")
        || sub == "tport"
        || sub.starts_with("wait-for-")
        || is_forward_family(sub)
}

/// Parse an AOSP transport-type token into a requested [`TransportKind`] filter.
/// `"usb"` → `Usb`, `"local"` → `Local`, `"any"` (or anything else) → `None`
/// (no kind filter). Used by the `transport-usb`/`transport-local` services, the
/// `host-usb:`/`host-local:` prefixes, and `wait-for-<transport>-device`.
fn parse_transport_kind(token: &str) -> Option<TransportKind> {
    match token {
        "usb" => Some(TransportKind::Usb),
        "local" => Some(TransportKind::Local),
        _ => None,
    }
}

/// The filter + zero/one/many core of transport selection over an
/// already-fetched device slice: keep the devices matching `want`
/// ([`kind_matches`]) and require exactly one. This is the single shared core
/// behind every kind-aware selection path (`resolve_single_by_kind` wraps it
/// with a `list_devices()` fetch; `select_tport` calls it directly on its own
/// already-fetched slice to avoid a second fetch), so they all agree on the
/// kind-specific AOSP zero/more-than-one wording
/// ([`no_devices_msg`]/[`ambiguous_msg`]).
fn pick_single_by_kind(
    devices: &[DeviceEntry],
    want: Option<TransportKind>,
) -> Result<&DeviceEntry, &'static str> {
    let mut matching = devices.iter().filter(|d| kind_matches(want, d.kind));
    match (matching.next(), matching.next()) {
        (None, _) => Err(no_devices_msg(want)),
        (Some(one), None) => Ok(one),
        (Some(_), Some(_)) => Err(ambiguous_msg(want)),
    }
}

/// Does a device of `entry_kind` satisfy a request for `want`?
///
/// `want == None` is `transport-any` (matches every device). A *device* whose
/// `entry_kind == None` (a backend that does not tag transport kind) matches any
/// request — the conservative degradation that keeps untagged backends behaving as
/// they did before `-d`/`-e` existed (see [`DeviceEntry::kind`]). Only when both
/// sides are concrete must they be equal.
fn kind_matches(want: Option<TransportKind>, entry_kind: Option<TransportKind>) -> bool {
    match (want, entry_kind) {
        (None, _) | (_, None) => true,
        (Some(w), Some(e)) => w == e,
    }
}

/// AOSP `acquire_one_transport` wording for *zero* matching devices, by requested
/// kind. `adb` prints these verbatim, so match the bytes exactly (confirmed
/// against the `adb` 35.0.2 client binary).
fn no_devices_msg(want: Option<TransportKind>) -> &'static str {
    match want {
        None => "no devices/emulators found",
        Some(TransportKind::Usb) => "no devices found",
        Some(TransportKind::Local) => "no emulators found",
    }
}

/// AOSP `acquire_one_transport` wording for *more than one* matching device, by
/// requested kind (confirmed against the `adb` 35.0.2 client binary).
fn ambiguous_msg(want: Option<TransportKind>) -> &'static str {
    match want {
        None => "more than one device/emulator",
        Some(TransportKind::Usb) => "more than one USB device",
        Some(TransportKind::Local) => "more than one emulator",
    }
}

/// Split `host-serial:<serial>:<sub>` into `(serial, sub)`, tolerating a serial
/// that itself contains colons (TCP/IP `ip:port`, e.g. `172.20.1.45:5555`).
///
/// `rest` is the payload after the `host-serial:` prefix. Anchored on a *known*
/// sub-service rather than a colon position: we scan colon split points and take
/// the first one whose right-hand side is a recognized sub-service
/// ([`is_host_serial_sub`]). Scanning left-to-right gives the longest serial that
/// still leaves a valid sub-service, which is what AOSP's serial-prefix matching
/// achieves for `ip:port` serials.
///
/// When no split yields a known sub-service we fall back to the *first* colon, so
/// a request like `host-serial:dev1:bogus` still reaches
/// [`dispatch_host_serial`]'s `other` arm and produces the precise
/// `unknown host-serial sub-service: bogus` error (rather than collapsing to
/// `malformed`). Returns `None` only when there is no colon at all.
fn split_host_serial(rest: &str) -> Option<(&str, &str)> {
    rest.match_indices(':')
        .find_map(|(idx, _)| {
            let sub = &rest[idx + 1..];
            is_host_serial_sub(sub).then(|| (&rest[..idx], sub))
        })
        .or_else(|| rest.split_once(':'))
}

/// Read one smartsocket request: 4 ASCII hex length, then that many UTF-8 bytes.
///
/// Returns `Ok(None)` on a clean EOF before any bytes (the client just closed),
/// and an error on a partial/garbled frame.
async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let Some(len) = protocol::parse_hex_len(&len_buf) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "non-hex request length prefix",
        ));
    };
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    String::from_utf8(body).map(Some).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF8 service string")
    })
}

/// Render a device list as `host:devices` (short) or `host:devices-l` (long).
fn format_devices(devices: &[DeviceEntry], long: bool) -> String {
    let serials: Vec<String> = devices.iter().map(|d| d.serial.clone()).collect();
    let mut lines = Vec::with_capacity(devices.len());
    for d in devices {
        if long {
            use std::fmt::Write as _;
            let mut line = format!("{}\t{}", d.serial, d.state.as_wire());
            if let Some(p) = &d.product {
                let _ = write!(line, " product:{p}");
            }
            if let Some(m) = &d.model {
                let _ = write!(line, " model:{m}");
            }
            if let Some(dev) = &d.device {
                let _ = write!(line, " device:{dev}");
            }
            if let Some(id) = protocol::transport_id_for(&d.serial, &serials) {
                let _ = write!(line, " transport_id:{id}");
            }
            lines.push(line);
        } else {
            lines.push(format!("{}\t{}", d.serial, d.state.as_wire()));
        }
    }
    lines.join("\n")
}

/// A reply that should always fit the 4-hex frame; on the (impossible-for-our
/// payloads) overflow, degrade to a FAIL rather than panic.
fn reply_or_overflow(reply: Option<Vec<u8>>) -> Vec<u8> {
    reply.unwrap_or_else(|| protocol::fail("reply too large"))
}

/// Enable `TCP_NODELAY` on a freshly-accepted **client-facing** socket (SEG A:
/// `adb`/`scrcpy` client → adboost frontend).
///
/// Interactive shell echo is a small-packet round-trip (one keystroke → one
/// echoed byte); with Nagle on, every keystroke's echo is held an extra RTT
/// waiting to coalesce, so the shell visibly lags. The device-facing hop (SEG B)
/// already sets this; the client hop must too, since it carries both the
/// small-packet host-protocol handshake and the bridged interactive stream.
///
/// Unlike the SEG B `connect()` path, a failure here must NOT drop the
/// connection: this is an already-accepted, live client socket mid-serve, and
/// Nagle-on is a latency regression, not a correctness failure. Log and proceed
/// (mirrors the reverse host-dial pattern in `reverse_engine.rs`).
fn enable_client_nodelay(stream: &TcpStream, peer: SocketAddr) {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::debug!("client {peer}: set_nodelay failed: {e}");
    }
}

/// Host-side `forward` accept loop: for every inbound TCP connection on
/// `listener`, open `tcp:<remote_port>` on the device `serial` via `backend`
/// and bridge the two byte streams. Runs until the task is aborted (rule removed
/// / server shutdown) or the listener errors fatally.
///
/// A failure to open the device service for one inbound connection drops only
/// that connection (logged) — the listener keeps serving, matching adb's
/// per-connection forward semantics.
async fn run_forward_listener<B: DeviceBackend>(
    listener: TcpListener,
    backend: Arc<B>,
    serial: String,
    remote_port: u16,
) {
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("forward listener accept failed: {e}");
                continue;
            }
        };
        // SEG A: low-latency interactive echo for this forwarded-port client.
        enable_client_nodelay(&client, peer);
        let backend = Arc::clone(&backend);
        let serial = serial.clone();
        tokio::spawn(async move {
            let cmd = ADBLocalCommand::TcpConnect(remote_port);
            match backend.open_local_service(&serial, &cmd).await {
                Ok(session) => crate::usb::bridge_tcp_session(client, session).await,
                Err(e) => {
                    tracing::debug!("forward {peer}→tcp:{remote_port} open failed: {e}");
                }
            }
        });
    }
}

/// The disconnect-handling loop: drain the backend's [`LifecycleEvent`] stream
/// and apply the [`OnDisconnect`] policy to each vanished serial.
///
/// Spawned by [`AdbServerFrontend::serve`] only when the policy is not
/// [`OnDisconnect::Retain`] (that variant releases nothing, so the loop is not
/// even started). Ends when the backend closes the stream (server teardown).
async fn handle_disconnects<B: DeviceBackend>(
    mut events: mpsc::Receiver<LifecycleEvent>,
    handle: ForwardHandle<B>,
    policy: OnDisconnect,
) {
    // Drain with a `match` (NOT `while let Some(Disconnected(..))`): only
    // `Disconnected` (a permanent unplug / `host:disconnect`) releases the
    // serial's forward + reverse rules. `TransportReset` (an adbd restart — `adb
    // root`/`unroot`) must NOT release them (native adb keeps the host-side
    // listeners across a restart), so it is ignored here and the loop KEEPS
    // RUNNING. A `while let Some(Disconnected(..))` pattern would instead TERMINATE
    // the loop on the first `TransportReset`, silently disabling all subsequent
    // forward/reverse cleanup — a real bug trap.
    while let Some(event) = events.recv().await {
        let LifecycleEvent::Disconnected(serial) = event else {
            // `TransportReset` (or any future non-Disconnected variant): not a
            // permanent disconnect — release nothing, keep draining.
            continue;
        };
        match &policy {
            OnDisconnect::ReleaseAll => {
                let n = handle.release(&serial).await;
                tracing::info!(
                    serial = %serial,
                    "device disconnected; released {n} forward rule(s) + reverse rules"
                );
            }
            OnDisconnect::Notify(cb) => {
                tracing::debug!(serial = %serial, "device disconnected; notifying caller");
                cb(&serial);
            }
            // Retain never starts this loop (see `serve`), but match exhaustively
            // so a future construction path can't silently release.
            OnDisconnect::Retain => {}
        }
    }
    tracing::debug!("disconnect handler: lifecycle stream closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;
    use crate::server::backend::DeviceState;

    /// A hardware-free backend with a fixed device list. `open_local_service` is
    /// never called by these tests (they exercise the host-protocol arms that do
    /// not bridge), so it is left unimplemented.
    struct MockBackend {
        devices: Vec<DeviceEntry>,
    }

    impl DeviceBackend for MockBackend {
        async fn list_devices(&self) -> Vec<DeviceEntry> {
            self.devices.clone()
        }
        async fn subscribe_changes(&self) -> mpsc::Receiver<Vec<DeviceEntry>> {
            let (tx, rx) = mpsc::channel(1);
            let snapshot = self.devices.clone();
            tokio::spawn(async move {
                let _ = tx.send(snapshot).await;
                // Then close (drop tx) so track-devices test sees one snapshot.
            });
            rx
        }
        async fn open_local_service(
            &self,
            _serial: &str,
            _cmd: &ADBLocalCommand,
        ) -> Result<crate::usb::MultiplexedSession> {
            unimplemented!("bridge path needs USB hardware; not exercised in unit tests")
        }
        // Report each device's banner capabilities straight from its entry — the
        // realistic shape (a backend caches the parsed banner). `None` for an
        // unknown serial mirrors the real "not connected → unknown" case.
        async fn device_capabilities(
            &self,
            serial: &str,
            _timeout: std::time::Duration,
        ) -> Option<DeviceFeatureSet> {
            self.devices
                .iter()
                .find(|d| d.serial == serial)
                .and_then(|d| d.capabilities.clone())
        }
        // Reverse routing tests use these hardware-free stubs: list returns a
        // canned body; the kill/forward arms just succeed so the frontend's
        // reply framing can be asserted.
        async fn list_reverse(&self, _serial: &str) -> Result<String> {
            Ok("(reverse) tcp:5201 tcp:5201\n".to_string())
        }
        async fn reverse_remove_all(&self, _serial: &str) -> Result<()> {
            Ok(())
        }
        // connect/disconnect routing tests: echo AOSP-style status for a known
        // address, FAIL otherwise, so the frontend's OKAY+framed vs FAIL framing
        // can be asserted without real TCP devices.
        async fn connect(&self, addr: &str) -> Result<String> {
            if addr == "10.0.0.1:5555" {
                Ok(format!("connected to {addr}"))
            } else {
                Err(crate::RustADBError::ADBRequestFailed(format!(
                    "failed to connect to {addr}: refused"
                )))
            }
        }
        async fn disconnect(&self, addr: &str) -> Result<String> {
            if addr.is_empty() {
                Ok("disconnected everything (0 device(s))".to_string())
            } else if addr == "10.0.0.1:5555" {
                Ok(format!("disconnected {addr}"))
            } else {
                Err(crate::RustADBError::ADBRequestFailed(format!(
                    "no such device {addr}"
                )))
            }
        }
    }

    fn frontend_with(devices: Vec<DeviceEntry>) -> AdbServerFrontend<MockBackend> {
        AdbServerFrontend::builder(Arc::new(MockBackend { devices })).build()
    }

    /// Drive one request/response against `handle_client` over a real socketpair.
    /// Returns the raw bytes the server wrote back.
    async fn round_trip(frontend: Arc<AdbServerFrontend<MockBackend>>, request: &str) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = frontend.handle_client(stream).await;
        });

        let mut client = TcpStream::connect(addr).await.expect("connect");
        let framed = format!("{:04x}{request}", request.len());
        client
            .write_all(framed.as_bytes())
            .await
            .expect("write req");
        client.flush().await.expect("flush");

        let mut buf = Vec::new();
        // Read until the server closes (host queries close after replying).
        let _ = client.read_to_end(&mut buf).await;
        server.await.expect("server task");
        buf
    }

    /// Drive one *transport-selecting* request (e.g. `host:transport-usb`). Unlike
    /// [`round_trip`], a successful selection does NOT close the socket — the
    /// server keeps it open for the follow-up local-service request — so we read
    /// exactly the 4-byte `OKAY` (these reply a bare `OKAY`, no payload), then drop
    /// the client to EOF the server. Returns the 4 reply bytes.
    async fn round_trip_select(
        frontend: Arc<AdbServerFrontend<MockBackend>>,
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
        drop(client); // EOF → server's next read_request returns None → clean exit
        server.await.expect("server task");
        resp
    }

    /// Drive one `host:tport:*` request. Like [`round_trip_select`] a successful
    /// selection keeps the socket open, but `tport` replies `OKAY` + an 8-byte LE
    /// transport id, so we read exactly 12 bytes then drop the client to EOF the
    /// server. Returns the 12 reply bytes.
    async fn round_trip_tport(
        frontend: Arc<AdbServerFrontend<MockBackend>>,
        request: &str,
    ) -> [u8; 12] {
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

        let mut resp = [0u8; 12];
        client
            .read_exact(&mut resp)
            .await
            .expect("read 12-byte tport reply");
        drop(client); // EOF → server's next read_request returns None → clean exit
        server.await.expect("server task");
        resp
    }

    /// A backend that records reverse-release calls, for disconnect-handler
    /// tests. Its `subscribe_lifecycle` is driven manually via a returned sender.
    #[derive(Default)]
    struct DisconnectBackend {
        released_reverse: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl DeviceBackend for DisconnectBackend {
        async fn list_devices(&self) -> Vec<DeviceEntry> {
            Vec::new()
        }
        async fn subscribe_changes(&self) -> mpsc::Receiver<Vec<DeviceEntry>> {
            let (_tx, rx) = mpsc::channel(1);
            rx
        }
        async fn open_local_service(
            &self,
            _serial: &str,
            _cmd: &ADBLocalCommand,
        ) -> Result<crate::usb::MultiplexedSession> {
            unimplemented!("not exercised")
        }
        async fn release_reverse(&self, serial: &str) -> Result<()> {
            self.released_reverse
                .lock()
                .expect("test lock")
                .push(serial.to_owned());
            Ok(())
        }
    }

    /// Build a frontend over a `DisconnectBackend` carrying a forward rule for
    /// `serial`, plus the handle and reverse-release log. Returns everything the
    /// disconnect tests need.
    async fn disconnect_fixture(
        serial: &str,
        policy: OnDisconnect,
    ) -> (
        AdbServerFrontend<DisconnectBackend>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let backend = Arc::new(DisconnectBackend::default());
        let log = Arc::clone(&backend.released_reverse);
        let frontend = AdbServerFrontend::builder(backend)
            .on_disconnect(policy)
            .build();
        // Seed a forward rule for `serial` (no-op listener task stand-in).
        frontend
            .forwards
            .insert(7000, 7001, serial.to_string(), tokio::spawn(async {}))
            .await;
        (frontend, log)
    }

    /// Contract: a `Disconnected` event under `ReleaseAll` drops the serial's
    /// forward rule AND releases its reverse rules. This is the host-side fix for
    /// "USB unplugged but `forward --list` still shows the rule". The handler is
    /// source-agnostic — USB hotplug and TCP `host:disconnect` both arrive as the
    /// same `LifecycleEvent::Disconnected(serial)`, so one test covers both paths.
    #[tokio::test]
    async fn release_all_policy_drops_forward_and_reverse_on_disconnect() {
        let (frontend, reverse_log) =
            disconnect_fixture("YTGUSCNFMFAIK7ZP", OnDisconnect::ReleaseAll).await;
        let handle = frontend.handle();
        assert!(
            frontend.forwards.contains(7000).await,
            "precondition: forward rule present"
        );

        let (tx, rx) = mpsc::channel(4);
        let driver = tokio::spawn(handle_disconnects(rx, handle, OnDisconnect::ReleaseAll));
        tx.send(LifecycleEvent::Disconnected("YTGUSCNFMFAIK7ZP".to_string()))
            .await
            .expect("send event");
        drop(tx); // close stream so the handler loop ends
        driver.await.expect("handler task");

        assert!(
            !frontend.forwards.contains(7000).await,
            "forward rule must be released on disconnect"
        );
        assert_eq!(
            reverse_log.lock().expect("test lock").as_slice(),
            ["YTGUSCNFMFAIK7ZP"],
            "reverse rules must be released for the disconnected serial"
        );
    }

    /// Contract: under `Notify`, the handler releases NOTHING itself — it only
    /// invokes the callback with the serial. The rule stays until the caller acts.
    #[tokio::test]
    async fn notify_policy_invokes_callback_and_releases_nothing() {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_cl = Arc::clone(&seen);
        let policy = OnDisconnect::Notify(Arc::new(move |s: &str| {
            seen_cl.lock().expect("test lock").push(s.to_owned());
        }));
        let (frontend, reverse_log) = disconnect_fixture("DEV1", policy.clone()).await;
        let handle = frontend.handle();

        let (tx, rx) = mpsc::channel(4);
        let driver = tokio::spawn(handle_disconnects(rx, handle, policy));
        tx.send(LifecycleEvent::Disconnected("DEV1".to_string()))
            .await
            .expect("send event");
        drop(tx);
        driver.await.expect("handler task");

        assert_eq!(
            seen.lock().expect("test lock").as_slice(),
            ["DEV1"],
            "Notify must invoke the callback with the disconnected serial"
        );
        assert!(
            frontend.forwards.contains(7000).await,
            "Notify must NOT release the forward rule itself"
        );
        assert!(
            reverse_log.lock().expect("test lock").is_empty(),
            "Notify must NOT release reverse rules itself"
        );
    }

    /// Contract: `Retain` releases nothing. (`serve` does not even start the
    /// handler for Retain; this asserts the loop body is inert if reached.)
    #[tokio::test]
    async fn retain_policy_releases_nothing() {
        let (frontend, reverse_log) = disconnect_fixture("DEV2", OnDisconnect::Retain).await;
        let handle = frontend.handle();

        let (tx, rx) = mpsc::channel(4);
        let driver = tokio::spawn(handle_disconnects(rx, handle, OnDisconnect::Retain));
        tx.send(LifecycleEvent::Disconnected("DEV2".to_string()))
            .await
            .expect("send event");
        drop(tx);
        driver.await.expect("handler task");

        assert!(
            frontend.forwards.contains(7000).await,
            "Retain must keep the forward rule"
        );
        assert!(
            reverse_log.lock().expect("test lock").is_empty(),
            "Retain must not release reverse rules"
        );
    }

    /// SEG A regression: a client socket accepted by the frontend must have
    /// `TCP_NODELAY` enabled, so interactive shell echo is not held an RTT by
    /// Nagle. Drives a real loopback accept (the established harness) and asserts
    /// the *server-side* accepted socket reports `nodelay() == true`.
    #[tokio::test]
    async fn accepted_client_socket_has_nodelay_enabled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.expect("accept");
            enable_client_nodelay(&stream, peer);
            stream.nodelay().expect("nodelay getter")
        });

        let _client = TcpStream::connect(addr).await.expect("connect");
        let nodelay = server.await.expect("server task");
        assert!(
            nodelay,
            "accepted client socket must have TCP_NODELAY enabled (SEG A)"
        );
    }

    #[tokio::test]
    async fn host_version_replies_okay_plus_version() {
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:version").await;
        assert_eq!(resp, b"OKAY00040029");
    }

    #[tokio::test]
    async fn host_features_is_honest_minimal() {
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:features").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("OKAY"));
        assert!(body.contains("cmd,stat_v2,fixed_push_mkdir,apex"));
        assert!(
            !body.contains("shell_v2"),
            "default must not advertise shell_v2"
        );
    }

    #[tokio::test]
    async fn host_devices_lists_serials_and_state() {
        let f = Arc::new(frontend_with(vec![
            DeviceEntry::new("serialB"),
            DeviceEntry::new("serialA"),
        ]));
        let resp = round_trip(f, "host:devices").await;
        let body = String::from_utf8(resp).unwrap();
        // OKAY + %04x len + payload. Payload has both devices, tab-separated state.
        assert!(body.starts_with("OKAY"));
        assert!(body.contains("serialB\tdevice"));
        assert!(body.contains("serialA\tdevice"));
    }

    #[tokio::test]
    async fn host_devices_l_includes_transport_id_in_sorted_order() {
        let f = Arc::new(frontend_with(vec![
            DeviceEntry::new("zzz"),
            DeviceEntry::new("aaa"),
        ]));
        let resp = round_trip(f, "host:devices-l").await;
        let body = String::from_utf8(resp).unwrap();
        // sorted: aaa -> transport_id:1, zzz -> transport_id:2
        assert!(body.contains("aaa\tdevice"));
        assert!(body.contains("transport_id:1"));
        assert!(body.contains("transport_id:2"));
    }

    #[tokio::test]
    async fn transport_any_with_no_devices_fails() {
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:transport-any").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"));
        // AOSP transport-any (kTransportAny) wording: combined "device/emulator".
        assert!(body.contains("no devices/emulators found"), "got: {body}");
    }

    #[tokio::test]
    async fn transport_by_unknown_serial_fails() {
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("known")]));
        let resp = round_trip(f, "host:transport:nope").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"));
        assert!(body.contains("device not found"));
    }

    #[tokio::test]
    async fn tport_any_with_single_device_replies_okay_plus_8byte_id() {
        // tport SELECTS a transport, so the server keeps the socket open and
        // waits for the next (local-service) request. We must read the exact
        // 12-byte reply and then drop the client (EOF) rather than read_to_end,
        // which would deadlock against the server's pending read.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = f.handle_client(stream).await;
        });

        let mut client = TcpStream::connect(addr).await.expect("connect");
        let req = "host:tport:any";
        client
            .write_all(format!("{:04x}{req}", req.len()).as_bytes())
            .await
            .expect("write");
        client.flush().await.expect("flush");

        let mut resp = [0u8; 12];
        client
            .read_exact(&mut resp)
            .await
            .expect("read 12-byte tport reply");
        assert_eq!(&resp[..4], b"OKAY");
        assert_eq!(&resp[4..], &1u64.to_le_bytes(), "single device -> id 1");

        drop(client); // EOF → server's next read_request returns None → clean exit
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn tport_any_with_multiple_devices_fails_more_than_one() {
        // Modern `adb` selects a transport via `host:tport:any` before sending
        // `shell:` / forward / reverse. With multiple devices it must fail with
        // the AOSP-correct "more than one device/emulator", NOT "device not found".
        let f = Arc::new(frontend_with(vec![
            DeviceEntry::new("devA"),
            DeviceEntry::new("devB"),
        ]));
        let resp = round_trip(f, "host:tport:any").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(
            body.contains("more than one device/emulator"),
            "got: {body}"
        );
    }

    #[tokio::test]
    async fn tport_any_with_no_devices_fails_no_devices() {
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:tport:any").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no devices/emulators found"), "got: {body}");
    }

    #[tokio::test]
    async fn tport_by_unknown_serial_fails_device_not_found() {
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("known")]));
        let resp = round_trip(f, "host:tport:serial:nope").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("device not found"), "got: {body}");
    }

    #[tokio::test]
    async fn tport_by_unknown_id_fails_no_device_for_transport_id() {
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f, "host:tport:id:9").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no device for transport id"), "got: {body}");
    }

    #[tokio::test]
    async fn tport_by_invalid_id_fails_invalid_transport_id() {
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f, "host:tport:id:notanumber").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("invalid transport id"), "got: {body}");
    }

    // ---- `adb -d` / `adb -e`: transport-kind selection (USB / local) ----------
    //
    // Native `adb -d` sends `host-usb:features` then `transport-usb`; `adb -e`
    // sends `host-local:features` then `transport-local`. These cover both phases,
    // the mixed-topology disambiguation, the per-kind AOSP error wording, and the
    // `kind: None` (untagged backend) backward-compatible degradation.

    fn usb_dev(serial: &str) -> DeviceEntry {
        DeviceEntry::new(serial).with_kind(TransportKind::Usb)
    }
    fn local_dev(serial: &str) -> DeviceEntry {
        DeviceEntry::new(serial).with_kind(TransportKind::Local)
    }

    #[tokio::test]
    async fn transport_usb_selects_the_single_usb_device() {
        // `-d` phase 2 against one USB device → bare OKAY, transport selected.
        let f = Arc::new(frontend_with(vec![usb_dev("usb1")]));
        let resp = round_trip_select(f, "host:transport-usb").await;
        assert_eq!(&resp, b"OKAY");
    }

    #[tokio::test]
    async fn transport_local_selects_the_single_local_device() {
        // `-e` phase 2 against one local/TCP device → bare OKAY.
        let f = Arc::new(frontend_with(vec![local_dev("10.0.0.5:5555")]));
        let resp = round_trip_select(f, "host:transport-local").await;
        assert_eq!(&resp, b"OKAY");
    }

    #[tokio::test]
    async fn transport_usb_in_mixed_topology_picks_usb_not_tcp() {
        // The reported xdb case: one USB + one TCP device, non-conflicting serials.
        // `-d` must lock the USB device, `-e` the TCP one — true disambiguation.
        let devices = vec![usb_dev("usb1"), local_dev("10.0.0.5:5555")];
        let f = Arc::new(frontend_with(devices.clone()));
        let resp = round_trip_select(f, "host:transport-usb").await;
        assert_eq!(&resp, b"OKAY", "transport-usb selects the USB device");

        let f = Arc::new(frontend_with(devices));
        let resp = round_trip_select(f, "host:transport-local").await;
        assert_eq!(&resp, b"OKAY", "transport-local selects the TCP device");
    }

    #[tokio::test]
    async fn transport_usb_with_two_usb_devices_fails_more_than_one_usb_device() {
        // Ambiguity *within* the USB kind → AOSP USB-specific wording.
        let f = Arc::new(frontend_with(vec![usb_dev("usb1"), usb_dev("usb2")]));
        let resp = round_trip(f, "host:transport-usb").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("more than one USB device"), "got: {body}");
    }

    #[tokio::test]
    async fn transport_local_with_two_local_devices_fails_more_than_one_emulator() {
        let f = Arc::new(frontend_with(vec![
            local_dev("10.0.0.5:5555"),
            local_dev("10.0.0.6:5555"),
        ]));
        let resp = round_trip(f, "host:transport-local").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("more than one emulator"), "got: {body}");
    }

    #[tokio::test]
    async fn transport_usb_with_only_a_tcp_device_fails_no_devices_found() {
        // `-d` when the only device is TCP → USB-specific zero wording.
        let f = Arc::new(frontend_with(vec![local_dev("10.0.0.5:5555")]));
        let resp = round_trip(f, "host:transport-usb").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no devices found"), "got: {body}");
    }

    #[tokio::test]
    async fn transport_local_with_only_a_usb_device_fails_no_emulators_found() {
        // `-e` when the only device is USB → local-specific zero wording.
        let f = Arc::new(frontend_with(vec![usb_dev("usb1")]));
        let resp = round_trip(f, "host:transport-local").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no emulators found"), "got: {body}");
    }

    #[tokio::test]
    async fn host_usb_features_resolves_and_answers_features() {
        // `-d` phase 1: `host-usb:features` must NOT be `unknown service` (the
        // reported bug); it resolves the single USB device and answers features.
        let f = Arc::new(frontend_with(vec![usb_dev("usb1")]));
        let resp = round_trip(f, "host-usb:features").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("OKAY"), "got: {body}");
        assert!(
            body.contains("cmd,stat_v2,fixed_push_mkdir,apex"),
            "got: {body}"
        );
    }

    #[tokio::test]
    async fn host_local_get_state_resolves_local_device() {
        // `host-local:get-state` pins the single local device by kind.
        let f = Arc::new(frontend_with(vec![local_dev("10.0.0.5:5555")]));
        let resp = round_trip(f, "host-local:get-state").await;
        assert_eq!(
            resp,
            b"OKAY0006device",
            "got: {:?}",
            String::from_utf8_lossy(&resp)
        );
    }

    #[tokio::test]
    async fn host_usb_features_with_no_usb_device_fails_no_devices_found() {
        // `host-usb:` phase-1 error wording matches the USB transport selection.
        let f = Arc::new(frontend_with(vec![local_dev("10.0.0.5:5555")]));
        let resp = round_trip(f, "host-usb:features").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no devices found"), "got: {body}");
    }

    #[tokio::test]
    async fn transport_usb_untagged_backend_degrades_to_transport_any() {
        // A backend that does not tag kind (kind: None) must not regress: `-d`
        // behaves as transport-any — the single untagged device is selected.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip_select(f, "host:transport-usb").await;
        assert_eq!(&resp, b"OKAY");
    }

    #[tokio::test]
    async fn transport_usb_untagged_multi_device_is_ambiguous_with_usb_wording() {
        // Untagged + multiple devices: still ambiguous (matches any), reported with
        // the requested kind's wording (USB here).
        let f = Arc::new(frontend_with(vec![
            DeviceEntry::new("a"),
            DeviceEntry::new("b"),
        ]));
        let resp = round_trip(f, "host:transport-usb").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("more than one USB device"), "got: {body}");
    }

    // ---- `adb -d`/`-e` phase 2: `host:tport:usb` / `host:tport:local` ----------
    //
    // Modern `adb` (35.0.2, confirmed via ADB_TRACE) selects the transport with
    // `host:tport:usb` / `host:tport:local` — NOT the legacy `transport-usb`. The
    // bare `usb`/`local` tokens are kind tokens and route through the same shared
    // kind resolver, so wording matches `transport-usb`/`transport-local` exactly.

    #[tokio::test]
    async fn tport_usb_selects_single_usb_device_okay_plus_id() {
        // `-d` phase 2 against one USB device → OKAY + 8-byte transport id.
        let f = Arc::new(frontend_with(vec![usb_dev("usb1")]));
        let resp = round_trip_tport(f, "host:tport:usb").await;
        assert_eq!(&resp[..4], b"OKAY");
        assert_eq!(&resp[4..], &1u64.to_le_bytes(), "single device -> id 1");
    }

    #[tokio::test]
    async fn tport_local_selects_single_local_device_okay_plus_id() {
        // `-e` phase 2 against one local/TCP device → OKAY + 8-byte transport id.
        let f = Arc::new(frontend_with(vec![local_dev("10.0.0.5:5555")]));
        let resp = round_trip_tport(f, "host:tport:local").await;
        assert_eq!(&resp[..4], b"OKAY");
        assert_eq!(&resp[4..], &1u64.to_le_bytes(), "single device -> id 1");
    }

    #[tokio::test]
    async fn tport_usb_in_mixed_topology_picks_usb_local_picks_tcp() {
        // One USB + one TCP device: `tport:usb` locks the USB device, `tport:local`
        // the TCP one. Serials sort as "10.0.0.5:5555" (id 1) < "usb1" (id 2).
        let devices = vec![usb_dev("usb1"), local_dev("10.0.0.5:5555")];
        let f = Arc::new(frontend_with(devices.clone()));
        let resp = round_trip_tport(f, "host:tport:usb").await;
        assert_eq!(&resp[..4], b"OKAY", "tport:usb selects the USB device");
        assert_eq!(&resp[4..], &2u64.to_le_bytes(), "usb1 -> id 2");

        let f = Arc::new(frontend_with(devices));
        let resp = round_trip_tport(f, "host:tport:local").await;
        assert_eq!(&resp[..4], b"OKAY", "tport:local selects the TCP device");
        assert_eq!(&resp[4..], &1u64.to_le_bytes(), "10.0.0.5:5555 -> id 1");
    }

    #[tokio::test]
    async fn tport_usb_with_two_usb_devices_fails_more_than_one_usb_device() {
        // Ambiguity within the USB kind → AOSP USB-specific wording.
        let f = Arc::new(frontend_with(vec![usb_dev("usb1"), usb_dev("usb2")]));
        let resp = round_trip(f, "host:tport:usb").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("more than one USB device"), "got: {body}");
    }

    #[tokio::test]
    async fn tport_usb_with_only_a_tcp_device_fails_no_devices_found() {
        // `-d` when the only device is TCP → USB-specific zero wording.
        let f = Arc::new(frontend_with(vec![local_dev("10.0.0.5:5555")]));
        let resp = round_trip(f, "host:tport:usb").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no devices found"), "got: {body}");
    }

    #[tokio::test]
    async fn tport_local_with_only_a_usb_device_fails_no_emulators_found() {
        // `-e` when the only device is USB → local-specific zero wording.
        let f = Arc::new(frontend_with(vec![usb_dev("usb1")]));
        let resp = round_trip(f, "host:tport:local").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no emulators found"), "got: {body}");
    }

    #[tokio::test]
    async fn tport_usb_untagged_single_device_replies_okay_plus_id() {
        // Untagged backend (kind: None) must not regress: `tport:usb` degrades to
        // transport-any uniqueness — the single untagged device is selected.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip_tport(f, "host:tport:usb").await;
        assert_eq!(&resp[..4], b"OKAY");
        assert_eq!(&resp[4..], &1u64.to_le_bytes(), "single device -> id 1");
    }

    #[tokio::test]
    async fn host_features_after_transport_select_is_answered_not_rejected() {
        // Regression: `ADBProxyDevice::shell_command` sends `host:transport:<serial>`
        // and THEN `host:features` on the same connection to choose shell v1 vs v2.
        // The post-transport `host:features` must be answered from the server's
        // capabilities, not routed to `map_local_service` (which rejected it with
        // "service not supported", forcing the proxy down the v1 path with no exit
        // codes — observed as a SKIPPED `through_server.shell_exit_code`).
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = f.handle_client(stream).await;
        });

        let mut client = TcpStream::connect(addr).await.expect("connect");

        // 1) Select the transport — server replies OKAY and keeps the socket open.
        let req = "host:transport:solo";
        client
            .write_all(format!("{:04x}{req}", req.len()).as_bytes())
            .await
            .expect("write transport");
        client.flush().await.expect("flush");
        let mut okay = [0u8; 4];
        client.read_exact(&mut okay).await.expect("read OKAY");
        assert_eq!(&okay, b"OKAY", "transport selection must succeed");

        // 2) Post-transport host:features — must be OKAY + the advertised features,
        //    NOT a FAIL.
        let req = "host:features";
        client
            .write_all(format!("{:04x}{req}", req.len()).as_bytes())
            .await
            .expect("write features");
        client.flush().await.expect("flush");
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.expect("read features");
        let body = String::from_utf8(rest).unwrap();
        assert!(
            body.starts_with("OKAY"),
            "post-transport host:features must be answered, got: {body}"
        );
        assert!(
            body.contains("cmd"),
            "features must list the server's caps: {body}"
        );

        server.await.expect("server task");
    }

    #[tokio::test]
    async fn host_serial_get_state_reports_device_or_offline() {
        let f = Arc::new(frontend_with(vec![DeviceEntry {
            serial: "dev1".to_string(),
            state: DeviceState::Device,
            product: None,
            model: None,
            device: None,
            capabilities: None,
            kind: None,
        }]));
        let resp = round_trip(f.clone(), "host-serial:dev1:get-state").await;
        assert_eq!(resp, b"OKAY0006device");

        let resp = round_trip(f, "host-serial:ghost:get-state").await;
        assert_eq!(resp, b"OKAY0007offline");
    }

    #[test]
    fn split_host_serial_handles_tcp_ip_serial_with_colon() {
        // The bug: `ip:port` serials contain a colon, so first-colon splitting
        // mis-parses serial=`172.20.1.45`, sub=`5555:features`. The anchor on a
        // known sub-service must recover the full serial and full sub.
        let serial = "172.20.1.45:5555";

        for sub in [
            "features",
            "get-state",
            "get-serialno",
            "transport",
            "tport",
            "list-forward",
            "killforward-all",
            "forward:tcp:0;tcp:7777",
            "killforward:tcp:7777",
        ] {
            let rest = format!("{serial}:{sub}");
            assert_eq!(
                split_host_serial(&rest),
                Some((serial, sub)),
                "tcp/ip serial+sub must split on the known-sub anchor: {rest}"
            );
        }
    }

    #[test]
    fn split_host_serial_handles_usb_serial_without_colon() {
        // USB serials carry no colon — the legacy path must keep working.
        for sub in ["get-state", "features", "forward:tcp:0;tcp:7777"] {
            let rest = format!("dev1:{sub}");
            assert_eq!(
                split_host_serial(&rest),
                Some(("dev1", sub)),
                "usb serial must not regress: {rest}"
            );
        }
    }

    #[test]
    fn split_host_serial_unknown_sub_falls_back_to_first_colon() {
        // An unknown sub-service finds no anchor; fall back to the first colon so
        // `dispatch_host_serial` still emits the precise "unknown sub-service".
        assert_eq!(
            split_host_serial("dev1:bogus"),
            Some(("dev1", "bogus")),
            "unknown sub falls back to first-colon split"
        );
        // No colon at all → genuinely malformed.
        assert_eq!(split_host_serial("dev1"), None);
    }

    #[test]
    fn parse_transport_kind_maps_tokens() {
        assert_eq!(parse_transport_kind("usb"), Some(TransportKind::Usb));
        assert_eq!(parse_transport_kind("local"), Some(TransportKind::Local));
        assert_eq!(parse_transport_kind("any"), None);
        assert_eq!(parse_transport_kind(""), None);
    }

    #[test]
    fn kind_matches_treats_none_as_wildcard_on_both_sides() {
        use TransportKind::{Local, Usb};
        // want == None (transport-any) matches every device.
        assert!(kind_matches(None, Some(Usb)));
        assert!(kind_matches(None, Some(Local)));
        assert!(kind_matches(None, None));
        // entry kind == None (untagged backend) matches every request.
        assert!(kind_matches(Some(Usb), None));
        assert!(kind_matches(Some(Local), None));
        // Concrete-vs-concrete must be equal.
        assert!(kind_matches(Some(Usb), Some(Usb)));
        assert!(!kind_matches(Some(Usb), Some(Local)));
        assert!(!kind_matches(Some(Local), Some(Usb)));
    }

    #[test]
    fn error_wording_matches_aosp_per_kind() {
        // Locked against the `adb` 35.0.2 client binary strings.
        assert_eq!(no_devices_msg(None), "no devices/emulators found");
        assert_eq!(no_devices_msg(Some(TransportKind::Usb)), "no devices found");
        assert_eq!(
            no_devices_msg(Some(TransportKind::Local)),
            "no emulators found"
        );
        assert_eq!(ambiguous_msg(None), "more than one device/emulator");
        assert_eq!(
            ambiguous_msg(Some(TransportKind::Usb)),
            "more than one USB device"
        );
        assert_eq!(
            ambiguous_msg(Some(TransportKind::Local)),
            "more than one emulator"
        );
    }

    #[tokio::test]
    async fn host_serial_features_with_tcp_ip_serial_routes_correctly() {
        // Regression: `host-serial:172.20.1.45:5555:features` must route to the
        // features branch (OKAY + payload), not FAIL with
        // `unknown host-serial sub-service: 5555:features`.
        let f = Arc::new(frontend_with(vec![DeviceEntry {
            serial: "172.20.1.45:5555".to_string(),
            state: DeviceState::Device,
            product: None,
            model: None,
            device: None,
            capabilities: None,
            kind: None,
        }]));
        let resp = round_trip(f, "host-serial:172.20.1.45:5555:features").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("OKAY"), "got: {body}");
        assert!(
            body.contains("cmd,stat_v2,fixed_push_mkdir,apex"),
            "features payload missing, got: {body}"
        );
    }

    #[tokio::test]
    async fn host_serial_features_is_per_device() {
        // The bug-fix end to end: the server advertises shell_v2 + sync_v2 (its
        // backend can bridge both), but `host-serial:<serial>:features` must
        // reflect EACH device's banner — so the feature-less device is NOT told
        // shell_v2 (native `adb -s ... shell` then picks v1 and avoids the CLSE),
        // while the full device still is.
        let caps = ServerCapabilities::default()
            .with_shell_v2()
            .with_feature("sync_v2");
        let full = DeviceEntry::new("full:5555").with_capabilities(DeviceFeatureSet {
            shell_v2: true,
            stat_v2: true,
            ..DeviceFeatureSet::default()
        });
        let stripped = DeviceEntry::new("stripped:6665").with_capabilities(DeviceFeatureSet {
            shell_v2: false,
            stat_v2: false,
            ..DeviceFeatureSet::default()
        });
        let f = Arc::new(frontend_with_caps_and_devices(caps, vec![full, stripped]));

        let full_body =
            String::from_utf8(round_trip(f.clone(), "host-serial:full:5555:features").await)
                .unwrap();
        assert!(
            full_body.contains("shell_v2"),
            "full device must still be offered shell_v2: {full_body}"
        );
        assert!(
            full_body.contains("sync_v2"),
            "full device must still be offered sync_v2: {full_body}"
        );

        let stripped_body =
            String::from_utf8(round_trip(f, "host-serial:stripped:6665:features").await).unwrap();
        assert!(
            !stripped_body.contains("shell_v2"),
            "feature-less device must NOT be offered shell_v2 (the bug): {stripped_body}"
        );
        assert!(
            !stripped_body.contains("sync_v2"),
            "feature-less device must NOT be offered sync_v2: {stripped_body}"
        );
        // The always-safe defaults are still present for the stripped device.
        assert!(
            stripped_body.contains("cmd") && stripped_body.contains("apex"),
            "always-safe defaults must remain for the stripped device: {stripped_body}"
        );
    }

    #[tokio::test]
    async fn host_serial_get_state_with_tcp_ip_serial_routes_correctly() {
        let f = Arc::new(frontend_with(vec![DeviceEntry {
            serial: "172.20.1.45:5555".to_string(),
            state: DeviceState::Device,
            product: None,
            model: None,
            device: None,
            capabilities: None,
            kind: None,
        }]));
        let resp = round_trip(f, "host-serial:172.20.1.45:5555:get-state").await;
        assert_eq!(resp, b"OKAY0006device");
    }

    #[tokio::test]
    async fn host_serial_forward_with_tcp_ip_serial_routes_correctly() {
        // The sub-service itself carries multiple colons
        // (`forward:tcp:0;tcp:7777`); neither the serial nor the forward
        // sub-arguments may be truncated.
        let f = Arc::new(frontend_with(vec![DeviceEntry {
            serial: "172.20.1.45:5555".to_string(),
            state: DeviceState::Device,
            product: None,
            model: None,
            device: None,
            capabilities: None,
            kind: None,
        }]));
        let resp = round_trip(f, "host-serial:172.20.1.45:5555:forward:tcp:0;tcp:7777").await;
        let body = String::from_utf8(resp).unwrap();
        // A bound forward replies OKAY (not the "unknown sub-service" / "device
        // not found" failures the parsing bug would produce).
        assert!(body.starts_with("OKAY"), "got: {body}");
    }

    #[tokio::test]
    async fn host_kill_default_policy_rejects() {
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:kill").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"));
        assert!(body.contains("kill not permitted"));
    }

    #[tokio::test]
    async fn unknown_service_fails() {
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:bogus").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"));
    }

    #[tokio::test]
    async fn host_connect_success_replies_okay_plus_status() {
        // `adb connect <addr>` → `host:connect:<addr>`; the arm routes to the
        // backend and frames its status string as OKAY + %04x + body.
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:connect:10.0.0.1:5555").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("OKAY"), "got: {body}");
        assert!(body.contains("connected to 10.0.0.1:5555"), "got: {body}");
    }

    #[tokio::test]
    async fn host_connect_failure_replies_fail() {
        // A backend connect error becomes a single FAIL with the reason.
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:connect:1.2.3.4:5555").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("failed to connect"), "got: {body}");
    }

    #[tokio::test]
    async fn host_disconnect_known_device_replies_okay_plus_status() {
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:disconnect:10.0.0.1:5555").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("OKAY"), "got: {body}");
        assert!(body.contains("disconnected 10.0.0.1:5555"), "got: {body}");
    }

    #[tokio::test]
    async fn host_disconnect_all_empty_addr_replies_okay() {
        // `adb disconnect` (no addr) → `host:disconnect:` with an empty target.
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:disconnect:").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("OKAY"), "got: {body}");
        assert!(body.contains("disconnected everything"), "got: {body}");
    }

    #[tokio::test]
    async fn host_disconnect_unknown_device_fails() {
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:disconnect:9.9.9.9:5555").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no such device"), "got: {body}");
    }

    #[tokio::test]
    async fn host_reconnect_offline_replies_okay() {
        // We model no offline devices, so reconnect-offline is a success no-op.
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:reconnect-offline").await;
        assert_eq!(resp, b"OKAY");
    }

    #[tokio::test]
    async fn host_wait_for_device_returns_okay_when_present() {
        // `adb wait-for-device` (→ host:wait-for-any-device) returns immediately
        // with TWO OKAYs (R1: accept + satisfied) when a device is already present.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f, "host:wait-for-any-device").await;
        assert_eq!(resp, b"OKAYOKAY");
    }

    #[tokio::test]
    async fn host_wait_for_usb_device_token_is_accepted() {
        // The transport token (usb/local/any) now filters by kind. An untagged
        // device (kind: None) matches any token, so a `usb` wait still resolves.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f, "host:wait-for-usb-device").await;
        assert_eq!(resp, b"OKAYOKAY");
    }

    #[tokio::test]
    async fn host_wait_for_local_device_with_only_usb_times_out_quickly() {
        // A tagged USB-only set must NOT satisfy `wait-for-local-device`. We can't
        // wait the full 60s in a unit test, so assert the kind predicate directly:
        // a USB device does not match a local request.
        assert!(!kind_matches(
            Some(TransportKind::Local),
            Some(TransportKind::Usb)
        ));
    }

    #[tokio::test]
    async fn host_wait_for_non_device_state_fails_fast() {
        // recovery/sideload/bootloader are unobservable by this backend → FAIL
        // immediately rather than hang.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f, "host:wait-for-usb-recovery").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("not supported"), "got: {body}");
    }

    #[tokio::test]
    async fn host_wait_for_bare_state_form_is_accepted() {
        // The forgiving bare-state form (no transport token) still works for the
        // `device` state when a device is present.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f, "host:wait-for-device").await;
        assert_eq!(resp, b"OKAYOKAY");
    }

    #[tokio::test]
    async fn host_wait_for_unknown_state_fails() {
        // An unrecognized state is rejected (not an observable state).
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f, "host:wait-for-any-bogus").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("not supported"), "got: {body}");
    }

    #[tokio::test]
    async fn host_transport_id_routes_to_dispatch_host_serial() {
        // R5: `host-transport-id:<N>:<sub>` resolves N → serial (1-based over the
        // sorted serial set, so id 1 = "aaa") and funnels into dispatch_host_serial,
        // exactly like host-usb:/host-local:. Verify via the get-state sub-service.
        let f = Arc::new(frontend_with(vec![
            DeviceEntry::new("aaa"),
            DeviceEntry::new("zzz"),
        ]));
        let resp = round_trip(f, "host-transport-id:1:get-state").await;
        assert_eq!(resp, b"OKAY0006device");
    }

    #[tokio::test]
    async fn host_transport_id_invalid_id_fails() {
        // A non-numeric N is rejected with the same wording as
        // `select_transport_by_id` ("invalid transport id").
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("aaa")]));
        let resp = round_trip(f, "host-transport-id:x:get-state").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("invalid transport id"), "got: {body}");
    }

    #[tokio::test]
    async fn host_transport_id_out_of_range_fails() {
        // An N with no matching device → "no device for transport id" (matches
        // `select_transport_by_id`).
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("aaa")]));
        let resp = round_trip(f, "host-transport-id:9:get-state").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no device for transport id"), "got: {body}");
    }

    #[tokio::test]
    async fn host_wait_for_disconnect_with_no_devices_returns_okay_immediately() {
        // Disconnect state, entry-check (primary) path: with no matching device
        // present (pinned_serial = None, empty list), the target is already
        // not-alive on entry, so TWO OKAYs (R1) are returned immediately without
        // waiting the 10s fallback. This is the state the `adb root` reconnect
        // handshake blocks on.
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:wait-for-any-disconnect").await;
        assert_eq!(resp, b"OKAYOKAY");
    }

    #[test]
    fn is_host_serial_sub_recognizes_wait_for_family() {
        // Prerequisite: `wait-for-*` must anchor the host-serial split so a TCP/IP
        // `ip:port` serial followed by a wait-for sub splits correctly.
        assert!(is_host_serial_sub("wait-for-any-disconnect"));
        assert!(is_host_serial_sub("wait-for-usb-device"));
        let serial = "172.20.1.45:5555";
        let rest = format!("{serial}:wait-for-any-disconnect");
        assert_eq!(
            split_host_serial(&rest),
            Some((serial, "wait-for-any-disconnect")),
            "tcp/ip serial + wait-for sub must split on the known-sub anchor"
        );
    }

    #[test]
    fn format_devices_short_and_long() {
        let devices = vec![DeviceEntry {
            serial: "s1".to_string(),
            state: DeviceState::Device,
            product: Some("prod".to_string()),
            model: Some("mod".to_string()),
            device: Some("dev".to_string()),
            capabilities: None,
            kind: None,
        }];
        assert_eq!(format_devices(&devices, false), "s1\tdevice");
        assert_eq!(
            format_devices(&devices, true),
            "s1\tdevice product:prod model:mod device:dev transport_id:1"
        );
    }

    #[tokio::test]
    async fn forward_no_device_fails() {
        // `host:forward:` with no device resolves transport-any → AOSP
        // "no devices/emulators found".
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:forward:tcp:0;tcp:5555").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no devices/emulators found"), "got: {body}");
    }

    #[tokio::test]
    async fn forward_auto_assign_replies_two_okays_plus_port_and_lists() {
        // Single device → forward binds a real OS port (tcp:0) and replies
        // OKAY OKAY + %04x + decimal port. The rule then shows in list-forward.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f.clone(), "host:forward:tcp:0;tcp:5555").await;
        assert_eq!(&resp[..8], b"OKAYOKAY", "forward success is two bare OKAYs");
        // Remainder is %04x + ASCII decimal resolved port.
        let len = usize::from_str_radix(std::str::from_utf8(&resp[8..12]).unwrap(), 16).unwrap();
        let port_str = std::str::from_utf8(&resp[12..12 + len]).unwrap();
        let resolved: u16 = port_str.parse().expect("decimal port");
        assert_ne!(resolved, 0, "tcp:0 must resolve to a real OS-assigned port");

        // list-forward reflects the rule: single OKAY + framed body.
        let listing = round_trip(f, "host:list-forward").await;
        assert_eq!(&listing[..4], b"OKAY");
        let body = String::from_utf8(listing[8..].to_vec()).unwrap();
        assert!(
            body.contains(&format!("solo tcp:{resolved} tcp:5555")),
            "list-forward body must contain the rule, got: {body}"
        );
    }

    #[tokio::test]
    async fn killforward_unknown_rule_fails() {
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f, "host:killforward:tcp:9999").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("not found"), "got: {body}");
    }

    #[tokio::test]
    async fn killforward_all_always_okays_twice() {
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        let resp = round_trip(f, "host:killforward-all").await;
        assert_eq!(resp, b"OKAYOKAY", "killforward-all success is two OKAYs");
    }

    #[tokio::test]
    async fn forward_norebind_conflict_fails() {
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("solo")]));
        // First forward binds a real port; capture it.
        let resp = round_trip(f.clone(), "host:forward:tcp:0;tcp:5555").await;
        let len = usize::from_str_radix(std::str::from_utf8(&resp[8..12]).unwrap(), 16).unwrap();
        let resolved: u16 = std::str::from_utf8(&resp[12..12 + len])
            .unwrap()
            .parse()
            .unwrap();

        // norebind on the SAME local port must fail with the AOSP reason.
        let req = format!("host:forward:norebind:tcp:{resolved};tcp:6000");
        let resp2 = round_trip(f, &req).await;
        let body = String::from_utf8(resp2).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(
            body.contains("cannot rebind existing socket"),
            "got: {body}"
        );
    }

    #[tokio::test]
    async fn forward_via_host_serial_form() {
        // `host-serial:<serial>:forward:...` — explicit serial path.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("dev1")]));
        let resp = round_trip(f, "host-serial:dev1:forward:tcp:0;tcp:7777").await;
        assert_eq!(
            &resp[..8],
            b"OKAYOKAY",
            "host-serial forward replies two OKAYs"
        );
    }

    #[tokio::test]
    async fn forward_via_host_serial_unknown_device_fails() {
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("dev1")]));
        let resp = round_trip(f, "host-serial:ghost:forward:tcp:0;tcp:7777").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("device not found"), "got: {body}");
    }

    /// Build a frontend whose advertised capabilities are explicitly set (the
    /// builder path; `serve()` would normally negotiate these from the backend).
    fn frontend_with_caps(caps: ServerCapabilities) -> AdbServerFrontend<MockBackend> {
        AdbServerFrontend::builder(Arc::new(MockBackend { devices: vec![] }))
            .capabilities(caps)
            .build()
    }

    /// Build a frontend with explicit advertised capabilities AND a device list
    /// (each device carrying its banner capabilities), for end-to-end per-device
    /// `host:features` assertions.
    fn frontend_with_caps_and_devices(
        caps: ServerCapabilities,
        devices: Vec<DeviceEntry>,
    ) -> AdbServerFrontend<MockBackend> {
        AdbServerFrontend::builder(Arc::new(MockBackend { devices }))
            .capabilities(caps)
            .build()
    }

    /// A device whose banner advertises everything (`shell_v2` + the `stat_v2`
    /// that marks sync-v2 support) — used to isolate the *server-feature* axis of
    /// the gate from the *device* axis in the pure `map_local_service` tests.
    fn full_device_caps() -> DeviceFeatureSet {
        DeviceFeatureSet {
            shell_v2: true,
            stat_v2: true,
            ..DeviceFeatureSet::default()
        }
    }

    #[test]
    fn map_local_service_shell_v1_and_tcp_always_ok() {
        let f = frontend_with_caps(ServerCapabilities::default());
        let dev = full_device_caps();
        // v1 shell / tcp do not consult device caps, but pass them anyway to
        // prove they are accepted regardless.
        assert!(matches!(
            f.map_local_service("shell:ls", Some(&dev)).unwrap(),
            ADBLocalCommand::ShellCommand(c, args) if c == "ls" && args.is_empty()
        ));
        assert!(matches!(
            f.map_local_service("tcp:5555", Some(&dev)).unwrap(),
            ADBLocalCommand::TcpConnect(5555)
        ));
        assert!(f.map_local_service("tcp:notaport", Some(&dev)).is_err());
    }

    #[test]
    fn map_local_service_sync_gated_on_sync_v2_feature() {
        let dev = full_device_caps();
        // Default caps do NOT advertise sync_v2 → sync: must be rejected even for
        // a fully-capable device (the server-feature axis).
        let f = frontend_with_caps(ServerCapabilities::default());
        assert!(
            f.map_local_service("sync:", Some(&dev)).is_err(),
            "sync: must FAIL when sync_v2 is not advertised (honest banner)"
        );

        // With sync_v2 advertised AND a device that supports it → bridged via Raw.
        let f = frontend_with_caps(ServerCapabilities::default().with_feature("sync_v2"));
        assert!(matches!(
            f.map_local_service("sync:", Some(&dev)).unwrap(),
            ADBLocalCommand::Raw(s) if s == "sync:"
        ));
    }

    #[test]
    fn map_local_service_shell_v2_gated_on_shell_v2_feature() {
        let dev = full_device_caps();
        // Default caps do NOT advertise shell_v2 → shell,v2 must be rejected.
        let f = frontend_with_caps(ServerCapabilities::default());
        assert!(
            f.map_local_service("shell,v2,raw:ls", Some(&dev)).is_err(),
            "shell,v2 must FAIL when shell_v2 is not advertised"
        );

        // With shell_v2 advertised AND a device that supports it → bridged
        // verbatim (modifiers preserved).
        let f = frontend_with_caps(ServerCapabilities::default().with_shell_v2());
        assert!(matches!(
            f.map_local_service("shell,v2,TERM=xterm,raw:ls", Some(&dev)).unwrap(),
            ADBLocalCommand::Raw(s) if s == "shell,v2,TERM=xterm,raw:ls"
        ));
    }

    #[test]
    fn map_local_service_shell_v2_denied_for_feature_less_device() {
        // The bug-report case: the server advertises shell_v2 (backend can bridge
        // it), but THIS device's banner lacks it (a stripped adbd). The gate must
        // FAIL the v2 OPEN rather than pass it through to be CLSE'd by the device.
        let f = frontend_with_caps(ServerCapabilities::default().with_shell_v2());
        let stripped = DeviceFeatureSet {
            shell_v2: false,
            stat_v2: false,
            ..DeviceFeatureSet::default()
        };
        assert!(
            f.map_local_service("shell,v2,raw:ls", Some(&stripped))
                .is_err(),
            "shell,v2 must FAIL for a device whose banner lacks shell_v2 (would CLSE)"
        );
        assert!(
            f.map_local_service("sync:", Some(&stripped)).is_err(),
            "sync: must FAIL for a device whose banner lacks sync-v2 (stat_v2)"
        );
        // But bare v1 shell still works on that same device.
        assert!(matches!(
            f.map_local_service("shell:ls", Some(&stripped)).unwrap(),
            ADBLocalCommand::ShellCommand(c, _) if c == "ls"
        ));
    }

    #[test]
    fn map_local_service_shell_v2_denied_for_unknown_device_caps() {
        // Capabilities unknown (device not handshaked) → conservative deny of the
        // framing services.
        let f = frontend_with_caps(
            ServerCapabilities::default()
                .with_shell_v2()
                .with_feature("sync_v2"),
        );
        assert!(
            f.map_local_service("shell,v2,raw:ls", None).is_err(),
            "unknown device caps must deny shell,v2 (conservative)"
        );
        assert!(
            f.map_local_service("sync:", None).is_err(),
            "unknown device caps must deny sync:"
        );
    }

    #[test]
    fn map_local_service_control_services_bridged_verbatim() {
        // Control services need no capability gate — default caps must accept
        // them, forwarded verbatim as Raw so the transparent bridge relays the
        // device's textual reply.
        let f = frontend_with_caps(ServerCapabilities::default());
        for svc in [
            "tcpip:5555",
            "usb:",
            "root:",
            "unroot:",
            "reboot:",
            "reboot:bootloader",
            "remount:",
            "enable-verity:",
            "disable-verity:",
        ] {
            // Control services need no device caps (every adbd supports them);
            // pass None to prove they are accepted regardless.
            match f.map_local_service(svc, None) {
                Ok(ADBLocalCommand::Raw(s)) => assert_eq!(s, svc, "forwarded verbatim"),
                Ok(_) => panic!("control service {svc} must map to Raw (got another command)"),
                Err(e) => panic!("control service {svc} must be accepted, got FAIL: {e}"),
            }
        }
    }

    #[test]
    fn is_control_service_matches_only_known_control_verbs() {
        for svc in [
            "tcpip:5555",
            "tcpip:0",
            "usb:",
            "root:",
            "unroot:",
            "reboot:",
            "reboot:recovery",
            "remount:",
            "enable-verity:",
            "disable-verity:",
        ] {
            assert!(is_control_service(svc), "{svc} should be a control service");
        }
        // Not control services: bare shell, sync, tcp connect, look-alikes.
        for svc in [
            "shell:ls",
            "sync:",
            "tcp:5555",
            "usbfoo:",
            "rebooting:",
            "tcpipx",
        ] {
            assert!(
                !is_control_service(svc),
                "{svc} should NOT be a control service"
            );
        }
    }

    #[test]
    fn map_local_service_jdwp_and_localabstract_unsupported() {
        let f = frontend_with_caps(
            ServerCapabilities::default()
                .with_feature("sync_v2")
                .with_shell_v2(),
        );
        // reverse: is routed by serve_reverse before map_local_service, so it is
        // not exercised here. jdwp/localabstract remain unbridged.
        assert!(f.map_local_service("jdwp:1234", None).is_err());
        assert!(f.map_local_service("localabstract:foo", None).is_err());
    }

    /// Select a transport then send one post-transport request, returning the
    /// server's reply bytes. Used for the reverse control services, which arrive
    /// after `host:transport:<serial>`.
    async fn post_transport_round_trip(
        frontend: Arc<AdbServerFrontend<MockBackend>>,
        serial: &str,
        post: &str,
    ) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = frontend.handle_client(stream).await;
        });

        let mut client = TcpStream::connect(addr).await.expect("connect");
        // 1) select transport (bare OKAY).
        let sel = format!("host:transport:{serial}");
        client
            .write_all(format!("{:04x}{sel}", sel.len()).as_bytes())
            .await
            .expect("write transport");
        client.flush().await.expect("flush");
        let mut okay = [0u8; 4];
        client.read_exact(&mut okay).await.expect("transport OKAY");
        assert_eq!(&okay, b"OKAY");
        // 2) the post-transport service.
        client
            .write_all(format!("{:04x}{post}", post.len()).as_bytes())
            .await
            .expect("write post");
        client.flush().await.expect("flush");
        let mut buf = Vec::new();
        let _ = client.read_to_end(&mut buf).await;
        server.await.expect("server task");
        buf
    }

    #[tokio::test]
    async fn reverse_forward_routes_to_backend_and_okays() {
        // MockBackend's open_reverse uses the trait default (unsupported) → FAIL,
        // proving the request REACHES the backend (not a generic frontend reject).
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("dev1")]));
        let resp = post_transport_round_trip(f, "dev1", "reverse:forward:tcp:5201;tcp:5201").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("reverse not supported"), "got: {body}");
    }

    #[tokio::test]
    async fn reverse_list_forward_frames_backend_body() {
        // MockBackend.list_reverse returns a canned (reverse) line; the frontend
        // must reply OKAY + framed body.
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("dev1")]));
        let resp = post_transport_round_trip(f, "dev1", "reverse:list-forward").await;
        assert_eq!(&resp[..4], b"OKAY", "list-forward is OKAY + framed body");
        let body = String::from_utf8(resp[8..].to_vec()).unwrap();
        assert!(body.contains("(reverse) tcp:5201 tcp:5201"), "got: {body}");
    }

    #[tokio::test]
    async fn reverse_killforward_all_okays() {
        // MockBackend.reverse_remove_all returns Ok → two OKAYs (AOSP framing).
        let f = Arc::new(frontend_with(vec![DeviceEntry::new("dev1")]));
        let resp = post_transport_round_trip(f, "dev1", "reverse:killforward-all").await;
        assert_eq!(resp, b"OKAYOKAY", "killforward-all success is two OKAYs");
    }

    // ----- Bug 2: event-driven wait-for-disconnect -----------------------------

    /// A backend whose `transport_alive` and `subscribe_lifecycle` are controllable
    /// so the event-driven `wait-for-disconnect` path can be exercised without USB
    /// hardware. `alive` starts true; a `TransportReset`/`Disconnected` is pushed
    /// via the returned `broadcast` sender.
    struct WaitDisconnectBackend {
        serial: String,
        alive: Arc<std::sync::atomic::AtomicBool>,
        lifecycle: tokio::sync::broadcast::Sender<LifecycleEvent>,
    }

    impl WaitDisconnectBackend {
        fn new(serial: &str, alive: bool) -> Self {
            let (lifecycle, _rx) = tokio::sync::broadcast::channel(8);
            Self {
                serial: serial.to_owned(),
                alive: Arc::new(std::sync::atomic::AtomicBool::new(alive)),
                lifecycle,
            }
        }
    }

    impl DeviceBackend for WaitDisconnectBackend {
        async fn list_devices(&self) -> Vec<DeviceEntry> {
            // The serial stays enumerated even after the connection dies — exactly
            // the MTK-adbd-restart shape the presence poll could not handle.
            vec![DeviceEntry::new(self.serial.clone())]
        }
        async fn subscribe_changes(&self) -> mpsc::Receiver<Vec<DeviceEntry>> {
            let (_tx, rx) = mpsc::channel(1);
            rx
        }
        async fn open_local_service(
            &self,
            _serial: &str,
            _cmd: &ADBLocalCommand,
        ) -> Result<crate::usb::MultiplexedSession> {
            unimplemented!("not exercised")
        }
        async fn transport_alive(&self, serial: &str) -> bool {
            serial == self.serial && self.alive.load(std::sync::atomic::Ordering::Acquire)
        }
        async fn subscribe_lifecycle(&self) -> mpsc::Receiver<LifecycleEvent> {
            let mut bcast = self.lifecycle.subscribe();
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                while let Ok(ev) = bcast.recv().await {
                    if tx.send(ev).await.is_err() {
                        break;
                    }
                }
            });
            rx
        }
    }

    /// Drive `host-serial:<serial>:wait-for-any-disconnect` against a
    /// `WaitDisconnectBackend`, returning the bytes the server wrote. Generic over
    /// the backend so it can carry the controllable disconnect backend.
    async fn round_trip_disconnect(backend: Arc<WaitDisconnectBackend>, serial: &str) -> Vec<u8> {
        let frontend = Arc::new(AdbServerFrontend::builder(backend).build());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = frontend.handle_client(stream).await;
        });
        let request = format!("host-serial:{serial}:wait-for-any-disconnect");
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

    /// Entry-check (primary) path: the transport is already dead on entry (the
    /// common case — PR0 showed the connection usually dies before the
    /// `wait-for-disconnect` request even arrives). The wait returns TWO OKAYs
    /// immediately, with no lifecycle event and no fallback wait.
    #[tokio::test]
    async fn wait_for_disconnect_entry_check_dead_returns_two_okays_immediately() {
        let backend = Arc::new(WaitDisconnectBackend::new("DEADDEV", false));
        let resp = round_trip_disconnect(backend, "DEADDEV").await;
        assert_eq!(
            resp, b"OKAYOKAY",
            "a transport already dead on entry returns two OKAYs at once"
        );
    }

    /// Event (secondary) path: the transport is alive on entry, then a
    /// `TransportReset` for the pinned serial arrives — the wait must unblock with
    /// TWO OKAYs promptly (NOT after the 10s fallback, and NOT after the old 60s
    /// presence ceiling).
    #[tokio::test]
    async fn wait_for_disconnect_unblocks_on_transport_reset_event() {
        let backend = Arc::new(WaitDisconnectBackend::new("LIVEDEV", true));
        let lifecycle = backend.lifecycle.clone();
        // Fire the reset shortly after the wait subscribes + passes the entry check.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = lifecycle.send(LifecycleEvent::TransportReset("LIVEDEV".to_string()));
        });
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            round_trip_disconnect(backend, "LIVEDEV"),
        )
        .await
        .expect("must unblock on the event well before the 10s fallback");
        assert_eq!(
            resp, b"OKAYOKAY",
            "a TransportReset for the pinned serial satisfies wait-for-disconnect"
        );
    }

    /// A real `Disconnected` (permanent unplug) also satisfies a disconnect wait.
    #[tokio::test]
    async fn wait_for_disconnect_unblocks_on_disconnected_event() {
        let backend = Arc::new(WaitDisconnectBackend::new("UNPLUG", true));
        let lifecycle = backend.lifecycle.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = lifecycle.send(LifecycleEvent::Disconnected("UNPLUG".to_string()));
        });
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            round_trip_disconnect(backend, "UNPLUG"),
        )
        .await
        .expect("must unblock on the disconnect event");
        assert_eq!(resp, b"OKAYOKAY");
    }

    /// Bounded-fallback path: the transport stays alive and no event ever arrives.
    /// Using paused time, the wait must fire the 10s fallback (NOT the old 60s)
    /// and still return TWO OKAYs (D1: assume-disconnected, clean return).
    #[tokio::test(start_paused = true)]
    async fn wait_for_disconnect_fallback_fires_at_10s_with_two_okays() {
        let backend = Arc::new(WaitDisconnectBackend::new("STUCK", true));
        let handle = tokio::spawn(round_trip_disconnect(backend, "STUCK"));
        // Advance just past the 10s bounded fallback; with paused time this does
        // not actually sleep. (If the bound were still 60s this would hang.)
        tokio::time::advance(std::time::Duration::from_secs(11)).await;
        let resp = handle.await.expect("task");
        assert_eq!(
            resp, b"OKAYOKAY",
            "the bounded fallback returns two OKAYs (assume disconnected), not a FAIL"
        );
    }

    /// `handle_disconnects` must IGNORE `TransportReset` (an adbd restart is not a
    /// permanent disconnect — forward/reverse rules must survive) while still
    /// releasing on a later `Disconnected`. A `while let Some(Disconnected(..))`
    /// loop would instead terminate on the `TransportReset` and silently disable
    /// all subsequent cleanup — this locks the `match`-and-continue fix.
    #[tokio::test]
    async fn handle_disconnects_ignores_transport_reset_but_still_releases_on_disconnected() {
        let (frontend, reverse_log) =
            disconnect_fixture("RESETDEV", OnDisconnect::ReleaseAll).await;
        let handle = frontend.handle();
        assert!(frontend.forwards.contains(7000).await, "precondition");

        let (tx, rx) = mpsc::channel(4);
        let driver = tokio::spawn(handle_disconnects(rx, handle, OnDisconnect::ReleaseAll));
        // A TransportReset must NOT release and must NOT end the loop.
        tx.send(LifecycleEvent::TransportReset("RESETDEV".to_string()))
            .await
            .expect("send reset");
        // A subsequent Disconnected on the SAME loop must still release.
        tx.send(LifecycleEvent::Disconnected("RESETDEV".to_string()))
            .await
            .expect("send disconnect");
        drop(tx);
        driver.await.expect("handler task");

        assert!(
            !frontend.forwards.contains(7000).await,
            "Disconnected after a TransportReset must still release (loop did not terminate early)"
        );
        assert_eq!(
            reverse_log.lock().expect("test lock").as_slice(),
            ["RESETDEV"],
            "exactly one release (the Disconnected); the TransportReset released nothing"
        );
    }

    /// The default `transport_alive` trait impl falls back to presence — a serial
    /// in `list_devices` reads as alive, an absent one as not. This keeps
    /// unadapted backends non-breaking (they degrade to the bounded fallback, not
    /// a 60s hang).
    #[tokio::test]
    async fn transport_alive_default_impl_falls_back_to_presence() {
        let b = MockBackend {
            devices: vec![DeviceEntry::new("present")],
        };
        assert!(b.transport_alive("present").await, "listed serial is alive");
        assert!(
            !b.transport_alive("absent").await,
            "unlisted serial is not alive (default presence fallback)"
        );
    }
}
