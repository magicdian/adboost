use std::{
    io::{Error, ErrorKind},
    net::SocketAddrV4,
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

use crate::{
    Result, RustADBError, adb_transport::ADBTransport, emulator::models::ADBEmulatorCommand,
};

/// Return authentication token stored in `$HOME/.emulator_console_auth_token`
async fn get_authentication_token() -> Result<String> {
    let Some(home) = std::env::home_dir() else {
        return Err(RustADBError::NoHomeDirectory);
    };

    let token = tokio::fs::read_to_string(home.join(".emulator_console_auth_token")).await?;

    Ok(token)
}

/// Emulator transport running on top on TCP.
#[derive(Debug)]
pub struct TCPEmulatorTransport {
    socket_addr: SocketAddrV4,
    tcp_stream: Option<TcpStream>,
}

impl TCPEmulatorTransport {
    /// Instantiates a new instance of [`TCPEmulatorTransport`]
    #[must_use]
    pub const fn new(socket_addr: SocketAddrV4) -> Self {
        Self {
            socket_addr,
            tcp_stream: None,
        }
    }

    pub(crate) fn get_raw_connection(&mut self) -> Result<&mut TcpStream> {
        self.tcp_stream
            .as_mut()
            .ok_or(RustADBError::IOError(Error::new(
                ErrorKind::NotConnected,
                "not connected",
            )))
    }

    /// Send an authenticate request to this emulator
    pub async fn authenticate(&mut self) -> Result<()> {
        let token = get_authentication_token().await?;
        let _ = self
            .send_command(&ADBEmulatorCommand::Authenticate(token))
            .await?;
        Ok(())
    }

    /// Send an [`ADBEmulatorCommand`] to this emulator
    pub(crate) async fn send_command(&mut self, command: &ADBEmulatorCommand) -> Result<String> {
        let command_bytes = command.to_string();
        {
            let connection = self.get_raw_connection()?;
            // Send command
            connection.write_all(command_bytes.as_bytes()).await?;
        }

        // Read response lines while checking for "OK" or "KO: " errors
        self.read_response().await
    }

    async fn read_response(&mut self) -> Result<String> {
        let mut reader = BufReader::new(self.get_raw_connection()?);
        let mut response = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).await?;
            if line.starts_with("KO:") {
                return Err(RustADBError::ADBRequestFailed(line));
            }
            if line.trim() == "OK" {
                break;
            }
            response.push_str(&line);
        }

        Ok(response)
    }
}

impl ADBTransport for TCPEmulatorTransport {
    async fn disconnect(&mut self) -> Result<()> {
        if let Some(conn) = &mut self.tcp_stream {
            let peer = conn.peer_addr()?;
            conn.shutdown().await?;
            tracing::trace!("Disconnected from {peer}");
        }

        Ok(())
    }

    /// Connect to current emulator and authenticate
    async fn connect(&mut self) -> Result<()> {
        if self.tcp_stream.is_none() {
            let stream = TcpStream::connect(self.socket_addr).await?;

            tracing::trace!("Successfully connected to {}", self.socket_addr);

            self.tcp_stream = Some(stream);

            // Android Console: Authentication required
            // Android Console: type 'auth <auth_token>' to authenticate
            // Android Console: you can find your <auth_token> in
            // '/home/xxx/.emulator_console_auth_token'
            {
                let mut reader = BufReader::new(self.get_raw_connection()?);
                for _ in 0..=4 {
                    let mut line = String::new();
                    reader.read_line(&mut line).await?;
                }
            }

            self.authenticate().await?;

            tracing::trace!("Authentication successful");
        }

        Ok(())
    }
}
