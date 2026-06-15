use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Set adb daemon to tcp/ip mode, returning the device's textual ack
    /// (e.g. `restarting in TCP mode port: 5555`).
    pub async fn tcpip(&mut self, port: u16) -> Result<String> {
        self.set_serial_transport().await?;

        // `tcpip:<port>` replies OKAY (consumed by `send_adb_request`, which turns
        // a FAIL into an error) and then streams a single status line with no
        // length prefix before closing — read that raw tail.
        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::TcpIp(port)))
            .await?;
        self.transport.read_raw_to_end().await
    }
}
