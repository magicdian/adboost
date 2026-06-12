use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Set adb daemon to tcp/ip mode
    pub async fn tcpip(&mut self, port: u16) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(&ADBCommand::Local(ADBLocalCommand::TcpIp(port)), false)
            .await
            .map(|_| ())
    }
}
