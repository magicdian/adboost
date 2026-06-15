use crate::{
    ADBTransport, Result,
    models::{ADBCommand, ADBHostCommand},
    proxy::TCPProxyTransport,
};
use std::net::SocketAddrV4;

/// Represents a device connected to the ADB server.
#[derive(Debug)]
pub struct ADBProxyDevice {
    /// Unique device identifier.
    pub identifier: Option<String>,
    /// Optional transport id assigned by the ADB server.
    ///
    /// When set, this takes precedence over `identifier` for transport selection.
    /// Required to disambiguate devices that report the same serial number.
    ///
    /// Note: transport ids are volatile. They are reassigned by the ADB server on
    /// device reconnect or server restart, so callers must re-query `devices_long`
    /// each time rather than caching the id across operations.
    pub transport_id: Option<u32>,
    /// Internal [`TCPProxyTransport`]
    pub(crate) transport: TCPProxyTransport,
}

impl ADBProxyDevice {
    /// Instantiates a new [`ADBProxyDevice`], knowing its ADB identifier (as returned by `adb devices` command).
    #[must_use]
    pub fn new(identifier: String, server_addr: Option<SocketAddrV4>) -> Self {
        let transport = TCPProxyTransport::new_or_default(server_addr);

        Self {
            identifier: Some(identifier),
            transport_id: None,
            transport,
        }
    }

    /// Instantiates a new [`ADBProxyDevice`] selected by its transport id (as returned by `adb devices -l`).
    ///
    /// Use this when multiple devices share the same serial number, since transport id is
    /// always unique within a running ADB server. The id is volatile: do not cache it across
    /// device reconnects or server restarts — re-query via [`crate::proxy::ADBProxyServer::devices_long`].
    #[must_use]
    pub fn new_with_transport_id(transport_id: u32, server_addr: Option<SocketAddrV4>) -> Self {
        let transport = TCPProxyTransport::new_or_default(server_addr);

        Self {
            identifier: None,
            transport_id: Some(transport_id),
            transport,
        }
    }

    /// Instantiates a new [`ADBProxyDevice`], assuming only one is currently connected.
    #[must_use]
    pub fn autodetect(server_addr: Option<SocketAddrV4>) -> Self {
        let transport = TCPProxyTransport::new_or_default(server_addr);

        Self {
            identifier: None,
            transport_id: None,
            transport,
        }
    }

    /// Connect to underlying transport
    pub(crate) async fn connect(&mut self) -> Result<&mut TCPProxyTransport> {
        self.transport.connect().await?;

        Ok(&mut self.transport)
    }

    /// Set device connection to use the configured transport.
    ///
    /// Prefers `transport_id` when set (unique even with duplicate serials), then falls back
    /// to `identifier` (serial), and finally to `transport-any` when neither is configured.
    pub(crate) async fn set_serial_transport(&mut self) -> Result<()> {
        let cmd = if let Some(id) = self.transport_id {
            ADBHostCommand::TransportId(id)
        } else if let Some(serial) = self.identifier.clone() {
            ADBHostCommand::TransportSerial(serial)
        } else {
            ADBHostCommand::TransportAny
        };
        self.connect()
            .await?
            .send_adb_request(&ADBCommand::Host(cmd))
            .await
    }
}

// NOTE (async teardown, P0-②): `disconnect` is now an async transport method and
// cannot be awaited in a stable-Rust `Drop`. The underlying `tokio::net::TcpStream`
// closes its socket when dropped, so teardown is handled structurally. Callers
// wanting a graceful, awaited shutdown should call `disconnect` explicitly.
