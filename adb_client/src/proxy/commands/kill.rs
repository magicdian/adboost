use crate::{
    Result,
    models::{ADBCommand, ADBHostCommand},
    proxy::ADBProxyServer,
};

impl ADBProxyServer {
    /// Asks the ADB server to quit immediately.
    pub async fn kill(&mut self) -> Result<()> {
        self.connect()
            .await?
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::Kill), false)
            .await
            .map(|_| ())
    }
}
