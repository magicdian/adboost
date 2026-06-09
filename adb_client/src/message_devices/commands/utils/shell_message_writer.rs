use crate::Result;
use crate::message_devices::{
    adb_message_transport::ADBMessageTransport, adb_transport_message::ADBTransportMessage,
    message_commands::MessageCommand,
};

/// Async writer hiding the underlying ADB protocol write logic for shell commands.
///
/// Replaces the previous `std::io::Write` impl (which could not call the now-async
/// transport). Callers drive an `AsyncRead` -> `write` copy loop explicitly.
pub struct ShellMessageWriter<T: ADBMessageTransport> {
    transport: T,
    local_id: u32,
    remote_id: u32,
}

impl<T: ADBMessageTransport> ShellMessageWriter<T> {
    pub const fn new(transport: T, local_id: u32, remote_id: u32) -> Self {
        Self {
            transport,
            local_id,
            remote_id,
        }
    }

    /// Write a single buffer to the device as a `WRTE` message.
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id,
            self.remote_id,
            buf,
        )?;
        self.transport.write_message(message).await?;
        Ok(buf.len())
    }
}
