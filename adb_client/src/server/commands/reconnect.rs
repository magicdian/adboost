use crate::{
    Result,
    models::{ADBCommand, ADBHostCommand},
    server::ADBServer,
};

impl ADBServer {
    /// Reconnect the device
    pub async fn reconnect_offline(&mut self) -> Result<()> {
        self.connect()
            .await?
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::ReconnectOffline), false)
            .await
            .map(|_| ())
    }
}
