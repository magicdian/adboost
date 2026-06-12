use crate::{
    Result,
    models::{ADBCommand, ADBHostCommand},
    proxy::{ADBProxyServer, models::ServerStatus},
};

impl ADBProxyServer {
    /// Check ADB server status
    pub async fn server_status(&mut self) -> Result<ServerStatus> {
        let status = self
            .connect()
            .await?
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::ServerStatus), true)
            .await?;

        ServerStatus::try_from(status)
    }
}
