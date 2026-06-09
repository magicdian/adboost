use crate::{
    Result, RustADBError,
    models::{ADBCommand, ADBLocalCommand, SyncCommand},
    server_device::ADBServerDevice,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

const BUFFER_SIZE: usize = 65535;

impl ADBServerDevice {
    /// Receives path to stream from the device.
    pub async fn pull(
        &mut self,
        path: &(dyn AsRef<str> + Sync),
        stream: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<()> {
        self.set_serial_transport().await?;

        // Set device in SYNC mode
        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::Sync))
            .await?;

        // Send a recv command
        self.transport.send_sync_request(&SyncCommand::Recv).await?;

        self.handle_recv_command(path, stream).await
    }

    async fn handle_recv_command<S: AsRef<str>>(
        &mut self,
        from: S,
        output: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<()> {
        let from_as_bytes = from.as_ref().as_bytes();
        let mut buffer = Vec::with_capacity(4 + from_as_bytes.len());
        buffer.extend_from_slice(&(u32::try_from(from.as_ref().len())?).to_le_bytes());
        buffer.extend_from_slice(from_as_bytes);
        self.transport
            .get_raw_connection()?
            .write_all(&buffer)
            .await?;

        let mut chunk = vec![0u8; BUFFER_SIZE].into_boxed_slice();
        loop {
            let connection = self.transport.get_raw_connection()?;
            let mut header = [0_u8; 4];
            connection.read_exact(&mut header).await?;

            match &header[..] {
                b"DATA" => {
                    let mut remaining = connection.read_u32_le().await? as usize;
                    while remaining > 0 {
                        let to_read = std::cmp::min(remaining, chunk.len());
                        connection.read_exact(&mut chunk[..to_read]).await?;
                        output.write_all(&chunk[..to_read]).await?;
                        remaining -= to_read;
                    }
                }
                b"DONE" => break,
                b"FAIL" => {
                    let length = connection.read_u32_le().await? as usize;
                    let mut error_msg = vec![0; length];
                    connection.read_exact(&mut error_msg).await?;

                    return Err(RustADBError::ADBRequestFailed(
                        String::from_utf8_lossy(&error_msg).to_string(),
                    ));
                }
                _ => {
                    return Err(RustADBError::UnknownResponseType(format!(
                        "Unknown response from device {header:#?}"
                    )));
                }
            }
        }

        output.flush().await?;

        // Connection should've been left in SYNC mode by now
        Ok(())
    }
}
