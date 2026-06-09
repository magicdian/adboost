use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    server_device::ADBServerDevice,
};

impl ADBServerDevice {
    /// Forward socket connection
    pub async fn forward(&mut self, remote: String, local: String) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(
                &ADBCommand::Local(ADBLocalCommand::Forward(remote, local)),
                false,
            )
            .await
            .map(|_| ())
    }

    /// Remove a previously applied forward rule by its local endpoint.
    pub async fn forward_remove(&mut self, local: String) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(
                &ADBCommand::Local(ADBLocalCommand::ForwardRemove(local)),
                false,
            )
            .await
            .map(|_| ())
    }

    /// Remove all previously applied forward rules
    pub async fn forward_remove_all(&mut self) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(&ADBCommand::Local(ADBLocalCommand::ForwardRemoveAll), false)
            .await
            .map(|_| ())
    }
}
