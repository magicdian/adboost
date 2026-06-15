use crate::Result;
use crate::message_devices::{
    adb_message_transport::ADBMessageTransport, adb_session::ADBSession,
    adb_transport_message::ADBTransportMessage, message_commands::MessageCommand,
};

/// Async writer hiding the underlying ADB protocol write logic.
///
/// Reads received responses to check that the message has been correctly
/// received. Replaces the previous `std::io::Write` impl (which could not call
/// the now-async session); callers drive an `AsyncRead` -> `write` copy loop.
pub struct MessageWriter<'session, T: ADBMessageTransport> {
    session: &'session mut ADBSession<T>,
}

impl<'session, T: ADBMessageTransport> MessageWriter<'session, T> {
    pub const fn new(session: &'session mut ADBSession<T>) -> Self {
        Self { session }
    }

    /// Write a single buffer to the device as a `WRTE` message and expect `OKAY`.
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.session.local_id(),
            self.session.remote_id(),
            buf,
        )?;

        self.session.send_and_expect_okay(message).await?;

        Ok(buf.len())
    }
}
