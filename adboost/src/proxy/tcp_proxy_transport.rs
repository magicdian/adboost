use std::net::{Ipv4Addr, SocketAddrV4};

use byteorder::{ByteOrder, LittleEndian};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::ADBTransport;
use crate::models::{ADBCommand, SyncCommand};
use crate::proxy::host_service::{self, HostStatus};
use crate::{Result, RustADBError};

const DEFAULT_SERVER_IP: Ipv4Addr = Ipv4Addr::LOCALHOST;
const DEFAULT_SERVER_PORT: u16 = 5037;

/// Server transport running on top on TCP
#[derive(Debug)]
pub struct TCPProxyTransport {
    socket_addr: SocketAddrV4,
    tcp_stream: Option<TcpStream>,
}

impl Default for TCPProxyTransport {
    fn default() -> Self {
        Self::new(SocketAddrV4::new(DEFAULT_SERVER_IP, DEFAULT_SERVER_PORT))
    }
}

impl TCPProxyTransport {
    /// Instantiates a new instance of [`TCPProxyTransport`]
    #[must_use]
    pub const fn new(socket_addr: SocketAddrV4) -> Self {
        Self {
            socket_addr,
            tcp_stream: None,
        }
    }

    /// Instantiate a new instance of [`TCPProxyTransport`] using given address, or default if not specified.
    #[must_use]
    pub fn new_or_default(socket_addr: Option<SocketAddrV4>) -> Self {
        socket_addr.map_or_else(Self::default, Self::new)
    }

    /// Get underlying [`SocketAddrV4`]
    #[must_use]
    pub const fn get_socketaddr(&self) -> SocketAddrV4 {
        self.socket_addr
    }

    pub(crate) async fn proxy_connection(
        &mut self,
        adb_command: &ADBCommand,
        with_response: bool,
    ) -> Result<Vec<u8>> {
        self.send_adb_request(adb_command).await?;

        if with_response {
            let length = self.get_hex_body_length().await?;
            let mut body = vec![
                0;
                length
                    .try_into()
                    .map_err(|_| RustADBError::ConversionError)?
            ];
            if length > 0 {
                self.get_raw_connection()?.read_exact(&mut body).await?;
            }

            Ok(body)
        } else {
            Ok(vec![])
        }
    }

    /// Read the raw connection to EOF and return it as UTF-8 (lossy), trimmed.
    ///
    /// Some device control services (e.g. `tcpip:<port>`) reply OKAY and then
    /// stream a single human-readable status line with **no** length prefix,
    /// closing the socket afterwards. Those replies cannot go through
    /// [`Self::proxy_connection`] (which expects a 4-hex length), so callers read
    /// the streamed tail here after `send_adb_request` has consumed the OKAY.
    pub(crate) async fn read_raw_to_end(&mut self) -> Result<String> {
        let mut body = Vec::new();
        self.get_raw_connection()?.read_to_end(&mut body).await?;
        Ok(String::from_utf8_lossy(&body).trim().to_string())
    }

    pub(crate) fn get_raw_connection(&mut self) -> Result<&mut TcpStream> {
        self.tcp_stream
            .as_mut()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not connected",
            )))
    }

    /// Take ownership of the underlying [`TcpStream`], leaving the transport
    /// disconnected.
    ///
    /// Used to hand a now-dedicated socket (after a `shell,v2` request has been
    /// accepted) to an owning session, so dropping that session closes the
    /// socket and the device observes EOF. The transport must be reconnected
    /// before its next request.
    pub(crate) fn take_raw_connection(&mut self) -> Result<TcpStream> {
        self.tcp_stream
            .take()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not connected",
            )))
    }

    /// Gets the body length from hexadecimal value
    pub(crate) async fn get_hex_body_length(&mut self) -> Result<u32> {
        let length_buffer = self.read_body_length().await?;
        Ok(u32::from_str_radix(
            std::str::from_utf8(&length_buffer)?,
            16,
        )?)
    }

    /// Send the given [`SyncCommand`] to ADB server, and checks that the request has been taken in consideration.
    pub(crate) async fn send_sync_request(&mut self, command: &SyncCommand) -> Result<()> {
        // First 4 bytes are the name of the command we want to send
        // (e.g. "SEND", "RECV", "STAT", "LIST")
        Ok(self
            .get_raw_connection()?
            .write_all(command.to_string().as_bytes())
            .await?)
    }

    /// Gets the body length from a `LittleEndian` value
    pub(crate) async fn get_body_length(&mut self) -> Result<u32> {
        let length_buffer = self.read_body_length().await?;
        Ok(LittleEndian::read_u32(&length_buffer))
    }

    /// Read 4 bytes representing body length
    async fn read_body_length(&mut self) -> Result<[u8; 4]> {
        let mut length_buffer = [0; 4];
        self.get_raw_connection()?
            .read_exact(&mut length_buffer)
            .await?;

        Ok(length_buffer)
    }

    /// Send the given [`AdbCommand`] to ADB server, and checks that the request has been taken in consideration.
    /// If an error occurred, a [`RustADBError`] is returned with the response error string.
    pub(crate) async fn send_adb_request(&mut self, command: &ADBCommand) -> Result<()> {
        let adb_command_string = command.to_string();
        host_service::send_request(self.get_raw_connection()?, &adb_command_string).await?;
        self.read_adb_response().await
    }

    /// Read a response from ADB server
    pub(crate) async fn read_adb_response(&mut self) -> Result<()> {
        match host_service::read_status(self.get_raw_connection()?).await? {
            HostStatus::Okay => Ok(()),
            HostStatus::Fail(reason) => Err(RustADBError::ADBRequestFailed(reason)),
        }
    }
}

impl ADBTransport for TCPProxyTransport {
    async fn disconnect(&mut self) -> Result<()> {
        if let Some(conn) = &mut self.tcp_stream {
            let peer = conn.peer_addr()?;
            conn.shutdown().await?;
            tracing::trace!("Disconnected from {peer}");
        }

        Ok(())
    }

    async fn connect(&mut self) -> Result<()> {
        if let Some(previous) = &mut self.tcp_stream {
            // Ignoring underlying error, we will recreate a new connection
            let _ = previous.shutdown().await;
        }
        let tcp_stream = TcpStream::connect(self.socket_addr).await?;
        tcp_stream.set_nodelay(true)?;
        self.tcp_stream = Some(tcp_stream);
        tracing::trace!("Successfully connected to {}", self.socket_addr);

        Ok(())
    }
}
