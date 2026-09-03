use crate::{
    Result,
    models::{ADBCommand, ADBHostCommand, HostFeatures},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Lists the features of the transport this device selects: a
    /// **post-transport** `host:features` (the per-transport AOSP query behind
    /// `adb features`), so the reply is the *device's* feature set — the
    /// server's features intersected with the device's CNXN banner. NOT the
    /// server-level `host:host-features` set. [`Self::shell_command`] gates
    /// shell v1/v2 on exactly this reply.
    pub async fn host_features(&mut self) -> Result<Vec<HostFeatures>> {
        self.set_serial_transport().await?;

        let features = self
            .transport
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::HostFeatures), true)
            .await?;

        Ok(features
            .split(|x| x.eq(&b','))
            .filter_map(|v| HostFeatures::try_from(v).ok())
            .collect())
    }
}
