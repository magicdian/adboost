//! A Read + Write adapter over an ADB session (WRTE/OKAY message protocol).

use std::io::{self, Read, Write};
use std::time::Duration;

use crate::message_devices::adb_message_transport::ADBMessageTransport;
use crate::message_devices::adb_session::ADBSession;
use crate::message_devices::adb_transport_message::ADBTransportMessage;
use crate::message_devices::message_commands::MessageCommand;

/// Bidirectional byte stream over an ADB session.
///
/// Wraps `ADBSession<T>` and implements `std::io::Read` + `std::io::Write`
/// by handling WRTE/OKAY flow control transparently.
#[derive(Debug)]
pub struct ADBSessionStream<T: ADBMessageTransport> {
    session: ADBSession<T>,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl<T: ADBMessageTransport> ADBSessionStream<T> {
    /// Create a new stream from an existing session.
    pub fn new(session: ADBSession<T>) -> Self {
        Self {
            session,
            read_buf: Vec::new(),
            read_pos: 0,
        }
    }

    /// Consume this stream and return the inner session.
    pub fn into_inner(self) -> ADBSession<T> {
        self.session
    }
}

impl<T: ADBMessageTransport> Read for ADBSessionStream<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // If we have buffered data, return it first
        if self.read_pos < self.read_buf.len() {
            let available = &self.read_buf[self.read_pos..];
            let to_copy = available.len().min(buf.len());
            buf[..to_copy].copy_from_slice(&available[..to_copy]);
            self.read_pos += to_copy;
            if self.read_pos >= self.read_buf.len() {
                self.read_buf.clear();
                self.read_pos = 0;
            }
            return Ok(to_copy);
        }

        // Read next message from transport
        let message = self
            .session
            .get_transport_mut()
            .read_message()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        match message.header().command() {
            MessageCommand::Write => {
                // Send OKAY acknowledgement
                let okay = ADBTransportMessage::try_new(
                    MessageCommand::Okay,
                    self.session.local_id(),
                    self.session.remote_id(),
                    &[],
                )
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                self.session
                    .get_transport_mut()
                    .write_message(okay)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

                let payload = message.into_payload();
                if payload.is_empty() {
                    return Ok(0);
                }

                let to_copy = payload.len().min(buf.len());
                buf[..to_copy].copy_from_slice(&payload[..to_copy]);
                if to_copy < payload.len() {
                    self.read_buf = payload;
                    self.read_pos = to_copy;
                }
                Ok(to_copy)
            }
            MessageCommand::Clse => {
                // Connection closed
                Ok(0)
            }
            MessageCommand::Okay => {
                // Ignore stray OKAYs and try reading again
                self.read(buf)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unexpected command: {}",
                    message.header().command()
                ),
            )),
        }
    }
}

impl<T: ADBMessageTransport> Write for ADBSessionStream<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // ADB messages have a max payload size; use 64KB chunks
        let chunk_size = buf.len().min(65536);
        let chunk = &buf[..chunk_size];

        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.session.local_id(),
            self.session.remote_id(),
            chunk,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        self.session
            .get_transport_mut()
            .write_message(message)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        // Wait for OKAY acknowledgement
        let response = self
            .session
            .get_transport_mut()
            .read_message_with_timeout(Duration::from_secs(10))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        if response.header().command() != MessageCommand::Okay {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "expected OKAY after WRTE, got {}",
                    response.header().command()
                ),
            ));
        }

        Ok(chunk_size)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
