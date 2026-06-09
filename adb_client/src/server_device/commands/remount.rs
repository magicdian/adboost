use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand, RemountInfo},
    server_device::ADBServerDevice,
};
use tokio::io::AsyncReadExt;

impl ADBServerDevice {
    /// Remounts the device filesystem as read-write
    pub async fn remount(&mut self) -> Result<Vec<RemountInfo>> {
        self.set_serial_transport().await?;

        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::Remount))
            .await?;

        let mut data = [0; 1024];
        let read_amount = self.transport.get_raw_connection()?.read(&mut data).await?;

        let response = String::from_utf8_lossy(&data[0..read_amount]);
        RemountInfo::from_str_response(&response)
    }
}
