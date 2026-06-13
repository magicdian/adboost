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

use super::backend::{DeviceBackend, DeviceEntry};
use super::capabilities::{KillPolicy, ServerCapabilities};
use super::forward::{ForwardRegistry, parse_forward, parse_killforward};
use super::protocol;
use crate::models::ADBLocalCommand;

/// Builder for [`AdbServerFrontend`].
pub struct AdbServerFrontendBuilder<B: DeviceBackend> {
    backend: Arc<B>,
    addr: SocketAddr,
    caps: ServerCapabilities,
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

    /// Finish building the frontend.
    #[must_use]
    pub fn build(self) -> AdbServerFrontend<B> {
        AdbServerFrontend {
            backend: self.backend,
            addr: self.addr,
            caps: self.caps,
            forwards: Arc::new(ForwardRegistry::default()),
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
}

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
        }
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
        let shared = Arc::new(self);
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    continue;
                }
            };
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
    async fn handle_client(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let Some(service) = read_request(&mut stream).await? else {
            return Ok(()); // clean EOF before any request
        };
        match self.dispatch_host_service(&mut stream, &service).await? {
            HostOutcome::Close => Ok(()),
            HostOutcome::TransportSelected(serial) => {
                let Some(local) = read_request(&mut stream).await? else {
                    return Ok(()); // client selected a transport then hung up
                };
                // After selecting a transport, a client may issue a forward-family
                // *host* request (this is how our own `ADBProxyDevice::forward`
                // works: `host:transport:<serial>` then `host:forward:...`). Route
                // those to the forward handler against the already-chosen serial
                // rather than treating them as a local service to bridge.
                if let Some(svc) = local.strip_prefix("host:")
                    && is_forward_family(svc)
                {
                    return match svc {
                        "list-forward" => self.serve_list_forward(&mut stream).await,
                        "killforward-all" => self.serve_killforward_all(&mut stream).await,
                        _ => self.serve_forward_family(&mut stream, svc, &serial).await,
                    };
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
        // `host-serial:<serial>:<sub>` — a single-device query carrying its own
        // serial. Strip the prefix and dispatch the sub-service against it.
        if let Some(rest) = service.strip_prefix("host-serial:") {
            if let Some((serial, sub)) = rest.split_once(':') {
                return self.dispatch_host_serial(stream, serial, sub).await;
            }
            stream
                .write_all(&protocol::fail("malformed host-serial request"))
                .await?;
            return Ok(HostOutcome::Close);
        }

        let Some(svc) = service.strip_prefix("host:") else {
            stream
                .write_all(&protocol::fail(&format!("unknown service: {service}")))
                .await?;
            return Ok(HostOutcome::Close);
        };

        match svc {
            "version" => {
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(
                        self.caps.version_hex(),
                    )))
                    .await?;
                Ok(HostOutcome::Close)
            }
            "features" => {
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(
                        &self.caps.features_csv(),
                    )))
                    .await?;
                Ok(HostOutcome::Close)
            }
            "devices" => {
                let listing = format_devices(&self.backend.list_devices().await, false);
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(&listing)))
                    .await?;
                Ok(HostOutcome::Close)
            }
            "devices-l" => {
                let listing = format_devices(&self.backend.list_devices().await, true);
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(&listing)))
                    .await?;
                Ok(HostOutcome::Close)
            }
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
            other => {
                stream
                    .write_all(&protocol::fail(&format!("unknown host service: {other}")))
                    .await?;
                Ok(HostOutcome::Close)
            }
        }
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
                stream
                    .write_all(&reply_or_overflow(protocol::okay_data(
                        &self.caps.features_csv(),
                    )))
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

    /// Resolve the single connected device's serial (transport-any semantics),
    /// or an AOSP-style failure reason when there are zero or multiple devices.
    async fn resolve_single_serial(&self) -> Result<String, String> {
        let devices = self.backend.list_devices().await;
        match devices.as_slice() {
            [] => Err("no devices".to_string()),
            [one] => Ok(one.serial.clone()),
            _ => Err("more than one device".to_string()),
        }
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
    /// or more than one).
    async fn select_transport_any(&self, stream: &mut TcpStream) -> std::io::Result<HostOutcome> {
        let devices = self.backend.list_devices().await;
        match devices.as_slice() {
            [] => {
                stream.write_all(&protocol::fail("no devices")).await?;
                Ok(HostOutcome::Close)
            }
            [one] => {
                stream.write_all(&protocol::okay()).await?;
                Ok(HostOutcome::TransportSelected(one.serial.clone()))
            }
            _ => {
                stream
                    .write_all(&protocol::fail("more than one device"))
                    .await?;
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

        let chosen = if rest.is_empty() || rest == "any" || rest == "-any" {
            match devices.as_slice() {
                [one] => Some(one.serial.clone()),
                _ => None,
            }
        } else if let Some(id_str) = rest
            .strip_prefix("id:")
            .or_else(|| rest.strip_prefix("-id:"))
        {
            id_str
                .parse::<u64>()
                .ok()
                .and_then(|id| protocol::transport_id_for_index(id, &serials))
        } else {
            // `tport:serial:<serial>` or `tport:<serial>`
            let serial = rest.strip_prefix("serial:").unwrap_or(rest);
            devices
                .iter()
                .find(|d| d.serial == serial)
                .map(|d| d.serial.clone())
        };

        if let Some(serial) = chosen {
            let id = protocol::transport_id_for(&serial, &serials).unwrap_or(0);
            stream.write_all(&protocol::okay_tport(id)).await?;
            Ok(HostOutcome::TransportSelected(serial))
        } else {
            stream
                .write_all(&protocol::fail("device not found"))
                .await?;
            Ok(HostOutcome::Close)
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

        let cmd = match self.map_local_service(service) {
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
        bridge_session(stream, session).await;
        Ok(())
    }

    /// Pure mapping from a post-transport service string to the
    /// [`ADBLocalCommand`] to open, or an AOSP-style FAIL reason. Capability
    /// gating (sync/shell-v2) is consulted here so an un-advertised service is
    /// rejected before any device session is opened.
    fn map_local_service(&self, service: &str) -> Result<ADBLocalCommand, String> {
        // `sync:` — bridged verbatim, only when `sync_v2` was advertised.
        if service == "sync:" {
            return if self.caps.has_feature("sync_v2") {
                Ok(ADBLocalCommand::Raw(service.to_string()))
            } else {
                Err(format!("service not supported: {service}"))
            };
        }
        // `shell,...` (shell-v2 and its modifiers) — verbatim, only when
        // `shell_v2` was advertised. Bare `shell:` (v1) is handled below.
        if service.starts_with("shell,") {
            return if self.caps.has_feature("shell_v2") {
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
                .map_err(|_| format!("invalid tcp port: {port_str}"));
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

/// Whether a (prefix-stripped) service is a member of the forward family:
/// `forward:` / `killforward:` (per-rule) or `list-forward` / `killforward-all`
/// (device-independent).
fn is_forward_family(svc: &str) -> bool {
    svc == "list-forward"
        || svc == "killforward-all"
        || svc.starts_with("forward:")
        || svc.starts_with("killforward:")
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
        let backend = Arc::clone(&backend);
        let serial = serial.clone();
        tokio::spawn(async move {
            let cmd = ADBLocalCommand::TcpConnect(remote_port);
            match backend.open_local_service(&serial, &cmd).await {
                Ok(session) => bridge_session(client, session).await,
                Err(e) => {
                    tracing::debug!("forward {peer}→tcp:{remote_port} open failed: {e}");
                }
            }
        });
    }
}

/// Bridge an arbitrary host TCP stream to a device session — the reverse data
/// path's host-dial side reuses the same bidirectional copy as the forward/local
/// bridge. `pub(super)` so [`super::reverse`] can call it.
pub(super) async fn bridge_session_public(
    host: TcpStream,
    session: crate::usb::MultiplexedSession,
) {
    bridge_session(host, session).await;
}

/// Bridge a client TCP socket to a device [`MultiplexedSession`] bidirectionally.
///
/// Both halves are `AsyncRead`/`AsyncWrite`. Each direction is copied
/// independently; when one direction reaches EOF, the *write* half of the other
/// peer is shut down (propagating the half-close as EOF) rather than aborting
/// the opposite copy. This is essential for request/response and
/// `echo … | nc`-style flows over `reverse`/`forward`: the client may close its
/// send side after the request while still expecting the reply to flow back.
/// The bridge ends only once BOTH directions complete.
async fn bridge_session(client: TcpStream, session: crate::usb::MultiplexedSession) {
    use tokio::io::AsyncWriteExt as _;

    let local_id = session.local_id();
    let (mut usb_read, mut usb_write) = session.into_split();
    let (mut client_read, mut client_write) = client.into_split();

    // client → device, then signal EOF to the device by shutting its write half.
    let c2u = tokio::spawn(async move {
        let n = tokio::io::copy(&mut client_read, &mut usb_write).await;
        tracing::trace!("bridge c2u (host→device) ended local_id={local_id}: {n:?}");
        let _ = usb_write.shutdown().await;
    });
    // device → client, then signal EOF to the client.
    let u2c = tokio::spawn(async move {
        let n = tokio::io::copy(&mut usb_read, &mut client_write).await;
        tracing::trace!("bridge u2c (device→host) ended local_id={local_id}: {n:?}");
        let _ = client_write.shutdown().await;
    });

    // Wait for BOTH directions to finish so a late reply is not truncated.
    let _ = tokio::join!(c2u, u2c);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;
    use crate::server::backend::DeviceState;
    use tokio::sync::mpsc;

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
        // Reverse routing tests use these hardware-free stubs: list returns a
        // canned body; the kill/forward arms just succeed so the frontend's
        // reply framing can be asserted.
        async fn list_reverse(&self, _serial: &str) -> Result<String> {
            Ok("(reverse) tcp:5201 tcp:5201\n".to_string())
        }
        async fn reverse_remove_all(&self, _serial: &str) -> Result<()> {
            Ok(())
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
        assert!(body.contains("no devices"));
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
    async fn host_serial_get_state_reports_device_or_offline() {
        let f = Arc::new(frontend_with(vec![DeviceEntry {
            serial: "dev1".to_string(),
            state: DeviceState::Device,
            product: None,
            model: None,
            device: None,
        }]));
        let resp = round_trip(f.clone(), "host-serial:dev1:get-state").await;
        assert_eq!(resp, b"OKAY0006device");

        let resp = round_trip(f, "host-serial:ghost:get-state").await;
        assert_eq!(resp, b"OKAY0007offline");
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

    #[test]
    fn format_devices_short_and_long() {
        let devices = vec![DeviceEntry {
            serial: "s1".to_string(),
            state: DeviceState::Device,
            product: Some("prod".to_string()),
            model: Some("mod".to_string()),
            device: Some("dev".to_string()),
        }];
        assert_eq!(format_devices(&devices, false), "s1\tdevice");
        assert_eq!(
            format_devices(&devices, true),
            "s1\tdevice product:prod model:mod device:dev transport_id:1"
        );
    }

    #[tokio::test]
    async fn forward_no_device_fails() {
        // `host:forward:` with no device resolves transport-any → "no devices".
        let f = Arc::new(frontend_with(vec![]));
        let resp = round_trip(f, "host:forward:tcp:0;tcp:5555").await;
        let body = String::from_utf8(resp).unwrap();
        assert!(body.starts_with("FAIL"), "got: {body}");
        assert!(body.contains("no devices"), "got: {body}");
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

    #[test]
    fn map_local_service_shell_v1_and_tcp_always_ok() {
        let f = frontend_with_caps(ServerCapabilities::default());
        assert!(matches!(
            f.map_local_service("shell:ls").unwrap(),
            ADBLocalCommand::ShellCommand(c, args) if c == "ls" && args.is_empty()
        ));
        assert!(matches!(
            f.map_local_service("tcp:5555").unwrap(),
            ADBLocalCommand::TcpConnect(5555)
        ));
        assert!(f.map_local_service("tcp:notaport").is_err());
    }

    #[test]
    fn map_local_service_sync_gated_on_sync_v2_feature() {
        // Default caps do NOT advertise sync_v2 → sync: must be rejected.
        let f = frontend_with_caps(ServerCapabilities::default());
        assert!(
            f.map_local_service("sync:").is_err(),
            "sync: must FAIL when sync_v2 is not advertised (honest banner)"
        );

        // With sync_v2 advertised → sync: is bridged verbatim via Raw.
        let f = frontend_with_caps(ServerCapabilities::default().with_feature("sync_v2"));
        assert!(matches!(
            f.map_local_service("sync:").unwrap(),
            ADBLocalCommand::Raw(s) if s == "sync:"
        ));
    }

    #[test]
    fn map_local_service_shell_v2_gated_on_shell_v2_feature() {
        // Default caps do NOT advertise shell_v2 → shell,v2 must be rejected.
        let f = frontend_with_caps(ServerCapabilities::default());
        assert!(
            f.map_local_service("shell,v2,raw:ls").is_err(),
            "shell,v2 must FAIL when shell_v2 is not advertised"
        );

        // With shell_v2 advertised → bridged verbatim (modifiers preserved).
        let f = frontend_with_caps(ServerCapabilities::default().with_shell_v2());
        assert!(matches!(
            f.map_local_service("shell,v2,TERM=xterm,raw:ls").unwrap(),
            ADBLocalCommand::Raw(s) if s == "shell,v2,TERM=xterm,raw:ls"
        ));
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
        assert!(f.map_local_service("jdwp:1234").is_err());
        assert!(f.map_local_service("localabstract:foo").is_err());
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
}
