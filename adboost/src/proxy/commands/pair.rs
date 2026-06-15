use crate::{
    Result, RustADBError,
    models::{ADBCommand, ADBHostCommand},
    proxy::ADBProxyServer,
};
use std::net::SocketAddrV4;

impl ADBProxyServer {
    /// Pair device on a specific port with a generated 'code'
    pub async fn pair(&mut self, address: SocketAddrV4, code: String) -> Result<()> {
        let response = self
            .connect()
            .await?
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::Pair(address, code)), true)
            .await?;

        match String::from_utf8(response) {
            Ok(s) if s.starts_with("Successfully paired to") => Ok(()),
            Ok(s) => Err(RustADBError::ADBRequestFailed(s)),
            Err(e) => Err(e.into()),
        }
    }
}
