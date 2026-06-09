use crate::{
    Result,
    models::{ADBCommand, ADBHostCommand},
    server::ADBServer,
};

impl ADBServer {
    /// Asks the ADB server to quit immediately.
    pub async fn kill(&mut self) -> Result<()> {
        self.connect()
            .await?
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::Kill), false)
            .await
            .map(|_| ())
    }
}
