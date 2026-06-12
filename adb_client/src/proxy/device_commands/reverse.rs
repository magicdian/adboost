use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Reverse socket connection
    pub async fn reverse(&mut self, remote: String, local: String) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(
                &ADBCommand::Local(ADBLocalCommand::Reverse(remote, local)),
                false,
            )
            .await
            .map(|_| ())
    }

    /// Remove a previously applied reverse rule by its remote endpoint.
    pub async fn reverse_remove(&mut self, remote: String) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(
                &ADBCommand::Local(ADBLocalCommand::ReverseRemove(remote)),
                false,
            )
            .await
            .map(|_| ())
    }

    /// Remove all reverse rules
    pub async fn reverse_remove_all(&mut self) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(&ADBCommand::Local(ADBLocalCommand::ReverseRemoveAll), false)
            .await
            .map(|_| ())
    }
}
