use crate::{
    Result, RustADBError,
    models::{ADBCommand, ADBLocalCommand, AdbRequestStatus, SyncCommand},
    server_device::ADBServerDevice,
};
use std::{
    convert::TryInto,
    str::{self, FromStr},
    time::SystemTime,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const BUFFER_SIZE: usize = 65535;

impl ADBServerDevice {
    /// Send stream to path on the device.
    pub async fn push<R: AsyncRead + Unpin, A: AsRef<str>>(
        &mut self,
        stream: R,
        path: A,
    ) -> Result<()> {
        log::info!("Sending data to {}", path.as_ref());
        self.set_serial_transport().await?;

        // Set device in SYNC mode
        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::Sync))
            .await?;

        // Send a send command
        self.transport.send_sync_request(&SyncCommand::Send).await?;

        self.handle_send_command(stream, path).await
    }

    async fn handle_send_command<R: AsyncRead + Unpin, S: AsRef<str>>(
        &mut self,
        mut input: R,
        to: S,
    ) -> Result<()> {
        // Append the permission flags to the filename
        let to = to.as_ref().to_string() + ",0777";

        // The name of the command is already sent by send_sync_request
        let to_as_bytes = to.as_bytes();
        let mut buffer = Vec::with_capacity(4 + to_as_bytes.len());
        buffer.extend_from_slice(&(u32::try_from(to.len())?).to_le_bytes());
        buffer.extend_from_slice(to_as_bytes);
        self.transport
            .get_raw_connection()?
            .write_all(&buffer)
            .await?;

        // Stream the input, framing it into "DATA" chunks.
        let mut chunk = vec![0u8; BUFFER_SIZE].into_boxed_slice();
        loop {
            let size = input.read(&mut chunk).await?;
            if size == 0 {
                break;
            }
            let chunk_len = u32::try_from(size)?;
            // 8 = "DATA".len() + sizeof(u32)
            let mut framed = Vec::with_capacity(8 + size);
            framed.extend_from_slice(b"DATA");
            framed.extend_from_slice(&chunk_len.to_le_bytes());
            framed.extend_from_slice(&chunk[..size]);
            self.transport
                .get_raw_connection()?
                .write_all(&framed)
                .await?;
        }

        // Copy is finished, we can now notify as finished
        // Have to send DONE + file mtime
        let Ok(last_modified) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) else {
            return Err(RustADBError::ADBRequestFailed(
                "SystemTime before UNIX EPOCH!".into(),
            ));
        };

        let mut done_buffer = Vec::with_capacity(8);
        done_buffer.extend_from_slice(b"DONE");
        done_buffer.extend_from_slice(&last_modified.as_secs().to_le_bytes());
        self.transport
            .get_raw_connection()?
            .write_all(&done_buffer)
            .await?;

        // We expect 'OKAY' response from this
        let mut request_status = [0; 4];
        self.transport
            .get_raw_connection()?
            .read_exact(&mut request_status)
            .await?;

        match AdbRequestStatus::from_str(str::from_utf8(&request_status)?)? {
            AdbRequestStatus::Fail => {
                // We can keep reading to get further details
                let length = self.transport.get_body_length().await?;

                let mut body = vec![
                    0;
                    length
                        .try_into()
                        .map_err(|_| RustADBError::ConversionError)?
                ];
                if length > 0 {
                    self.transport
                        .get_raw_connection()?
                        .read_exact(&mut body)
                        .await?;
                }

                Err(RustADBError::ADBRequestFailed(String::from_utf8(body)?))
            }
            AdbRequestStatus::Okay => Ok(()),
        }
    }
}
