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
        }
    }
}

/// An ADB server frontend: listens for native `adb`/`scrcpy` clients and
/// bridges their local services onto a [`DeviceBackend`].
pub struct AdbServerFrontend<B: DeviceBackend> {
    backend: Arc<B>,
    addr: SocketAddr,
    caps: ServerCapabilities,
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
    pub async fn serve(self) -> std::io::Result<()> {
        let listener = TcpListener::bind(self.addr).await?;
        let actual = listener.local_addr().unwrap_or(self.addr);
        tracing::info!("adb server frontend listening on {actual}");
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
            _ if svc.starts_with("tport:") => self.select_tport(stream, &svc["tport:".len()..]).await,
            _ if svc.starts_with("forward:") || svc.starts_with("killforward") => {
                // Real port-forward management (host-side listener + per-conn
                // bridge, double-OKAY framing, port-0 alloc) is a self-contained
                // follow-up — see task. Fail cleanly rather than half-implement.
                stream
                    .write_all(&protocol::fail("forward not supported yet"))
                    .await?;
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
            _ if sub.starts_with("transport") || sub == "tport" => {
                // `host-serial:<serial>:transport` selects that device.
                return self.select_transport_by_serial(stream, serial.to_string()).await;
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

    /// `host:transport-any` — select the single connected device (error if none
    /// or more than one).
    async fn select_transport_any(
        &self,
        stream: &mut TcpStream,
    ) -> std::io::Result<HostOutcome> {
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
        } else if let Some(id_str) = rest.strip_prefix("id:").or_else(|| rest.strip_prefix("-id:")) {
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
    async fn serve_local_service(
        &self,
        mut stream: TcpStream,
        service: &str,
        serial: &str,
    ) -> std::io::Result<()> {
        // Known-unsupported services get a stable FAIL BEFORE opening anything.
        let unsupported = service.starts_with("sync:")
            || service.starts_with("shell,") // shell,v2
            || service.starts_with("reverse:")
            || service.starts_with("jdwp")
            || service.starts_with("localabstract:");
        if unsupported {
            stream
                .write_all(&protocol::fail(&format!(
                    "service not supported: {service}"
                )))
                .await?;
            return Ok(());
        }

        // Map smartsocket service → ADBLocalCommand. Bare `shell:` is v1
        // (ShellCommand with empty args), NOT v2.
        let cmd = if let Some(shell_cmd) = service.strip_prefix("shell:") {
            ADBLocalCommand::ShellCommand(shell_cmd.to_string(), vec![])
        } else if let Some(port_str) = service.strip_prefix("tcp:") {
            if let Ok(port) = port_str.parse::<u16>() {
                ADBLocalCommand::TcpConnect(port)
            } else {
                stream
                    .write_all(&protocol::fail(&format!("invalid tcp port: {port_str}")))
                    .await?;
                return Ok(());
            }
        } else {
            stream
                .write_all(&protocol::fail(&format!(
                    "service not supported: {service}"
                )))
                .await?;
            return Ok(());
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
    String::from_utf8(body)
        .map(Some)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF8 service string"))
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

/// Bridge a client TCP socket to a device [`MultiplexedSession`] bidirectionally.
///
/// Both halves are `AsyncRead`/`AsyncWrite`; when either direction ends, the
/// other is aborted so the bridge tears down promptly.
async fn bridge_session(client: TcpStream, session: crate::usb::MultiplexedSession) {
    let (mut usb_read, mut usb_write) = session.into_split();
    let (mut client_read, mut client_write) = client.into_split();

    let mut c2u =
        tokio::spawn(async move { tokio::io::copy(&mut client_read, &mut usb_write).await });
    let mut u2c =
        tokio::spawn(async move { tokio::io::copy(&mut usb_read, &mut client_write).await });

    tokio::select! {
        _ = &mut c2u => u2c.abort(),
        _ = &mut u2c => c2u.abort(),
    }
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
        client.write_all(framed.as_bytes()).await.expect("write req");
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
        assert!(!body.contains("shell_v2"), "default must not advertise shell_v2");
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
        client.read_exact(&mut resp).await.expect("read 12-byte tport reply");
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
}
