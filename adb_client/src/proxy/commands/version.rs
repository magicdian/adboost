use crate::{
    Result,
    models::{ADBCommand, ADBHostCommand},
    proxy::{ADBProxyServer, AdbVersion},
};

impl ADBProxyServer {
    /// Gets server's internal version number.
    pub async fn version(&mut self) -> Result<AdbVersion> {
        let version = self
            .connect()
            .await?
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::Version), true)
            .await?;

        AdbVersion::try_from(version)
    }
}
