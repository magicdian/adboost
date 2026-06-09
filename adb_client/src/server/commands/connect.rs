use crate::{
    Result, RustADBError,
    models::{ADBCommand, ADBHostCommand},
    server::ADBServer,
};
use std::net::SocketAddrV4;

impl ADBServer {
    /// Connect device over tcp with address and port
    pub async fn connect_device(&mut self, address: SocketAddrV4) -> Result<()> {
        let response = self
            .connect()
            .await?
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::Connect(address)), true)
            .await?;

        match String::from_utf8(response) {
            Ok(s) if s.starts_with("connected to") => Ok(()),
            Ok(s) if s.starts_with("already connected to") => Ok(()),
            Ok(s) => Err(RustADBError::ADBRequestFailed(s)),
            Err(e) => Err(e.into()),
        }
    }
}
