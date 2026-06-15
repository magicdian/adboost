use std::io::{Cursor, Seek};

use byteorder::ReadBytesExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    AdbStatResponse, BinaryDecodable, Result, RustADBError,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_transport_message::ADBTransportMessage,
        message_commands::{MessageCommand, MessageSubcommand},
        utils::BinaryEncodable,
    },
};

const BUFFER_SIZE: usize = 65535;

/// Represent a session between an `ADBDevice` and remote `adbd`.
#[derive(Debug)]
pub struct ADBSession<T: ADBMessageTransport> {
    transport: T,
    local_id: u32,
    remote_id: u32,
}

impl<T: ADBMessageTransport> ADBSession<T> {
    /// Create a new session with the given transport and IDs.
    pub const fn new(transport: T, local_id: u32, remote_id: u32) -> Self {
        Self {
            transport,
            local_id,
            remote_id,
        }
    }

    /// Get a mutable reference to the underlying transport.
    pub const fn get_transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Get the local session ID.
    pub const fn local_id(&self) -> u32 {
        self.local_id
    }

    /// Get the remote session ID.
    pub const fn remote_id(&self) -> u32 {
        self.remote_id
    }

    /// Receive a message and acknowledge it by replying with an `OKAY` command
    pub(crate) async fn recv_and_reply_okay(&mut self) -> Result<ADBTransportMessage> {
        let message = self.transport.read_message().await?;
        self.transport
            .write_message(ADBTransportMessage::try_new(
                MessageCommand::Okay,
                self.local_id,
                self.remote_id,
                &[],
            )?)
            .await?;
        Ok(message)
    }

    /// Expect a message with an `OKAY` command after sending a message.
    pub(crate) async fn send_and_expect_okay(
        &mut self,
        message: ADBTransportMessage,
    ) -> Result<ADBTransportMessage> {
        self.transport.write_message(message).await?;

        let message = self.transport.read_message().await?;
        message.assert_command(MessageCommand::Okay)?;
        Ok(message)
    }

    pub(crate) async fn recv_file<W: AsyncWrite + Unpin>(
        &mut self,
        mut output: W,
    ) -> std::result::Result<(), RustADBError> {
        let mut len: Option<u64> = None;
        loop {
            let payload = self.recv_and_reply_okay().await?.into_payload();
            // The header parsing below operates on an in-memory cursor — pure
            // (sans-io) byte work, kept synchronous. Only the payload copy into
            // the async sink is awaited.
            let mut rdr = Cursor::new(&payload);
            while rdr.position() != payload.len() as u64 {
                match len.take() {
                    Some(0) | None => {
                        rdr.seek_relative(4)?;
                        len.replace(u64::from(
                            ReadBytesExt::read_u32::<byteorder::LittleEndian>(&mut rdr)?,
                        ));
                    }
                    Some(length) => {
                        let remaining_bytes = payload.len() as u64 - rdr.position();
                        let copy_len = length.min(remaining_bytes);
                        let start = usize::try_from(rdr.position())?;
                        let end = start + usize::try_from(copy_len)?;
                        output.write_all(&payload[start..end]).await?;
                        rdr.seek_relative(i64::try_from(copy_len)?)?;
                        if length < remaining_bytes {
                            // header for the next chunk follows in this payload
                        } else {
                            len.replace(length - remaining_bytes);
                            // this payload is now exhausted
                            break;
                        }
                    }
                }
            }
            if ReadBytesExt::read_u32::<byteorder::LittleEndian>(&mut Cursor::new(
                &payload[(payload.len() - 8)..(payload.len() - 4)],
            ))? == MessageSubcommand::Done as u32
            {
                break;
            }
        }
        Ok(())
    }

    pub(crate) async fn push_file<R: AsyncRead + Unpin>(&mut self, mut reader: R) -> Result<()> {
        let mut buffer = vec![0; BUFFER_SIZE].into_boxed_slice();
        let amount_read = reader.read(&mut buffer).await?;
        let subcommand_data = MessageSubcommand::Data.with_arg(u32::try_from(amount_read)?);

        let mut serialized_message = subcommand_data.encode();
        serialized_message.append(&mut buffer[..amount_read].to_vec());

        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id(),
            self.remote_id(),
            &serialized_message,
        )?;

        self.send_and_expect_okay(message).await?;

        loop {
            let mut buffer = vec![0; BUFFER_SIZE].into_boxed_slice();

            match reader.read(&mut buffer).await {
                Ok(0) => {
                    // Currently file mtime is not forwarded
                    let subcommand_data = MessageSubcommand::Done.with_arg(0);

                    let message = ADBTransportMessage::try_new(
                        MessageCommand::Write,
                        self.local_id(),
                        self.remote_id(),
                        &subcommand_data.encode(),
                    )?;

                    self.send_and_expect_okay(message).await?;

                    // Command should end with a Write => Okay
                    let received = self.transport.read_message().await?;
                    match received.header().command() {
                        MessageCommand::Write => return Ok(()),
                        c => {
                            return Err(RustADBError::ADBRequestFailed(format!(
                                "Wrong command received {c}"
                            )));
                        }
                    }
                }
                Ok(size) => {
                    let subcommand_data = MessageSubcommand::Data.with_arg(u32::try_from(size)?);

                    let mut serialized_message = subcommand_data.encode();
                    serialized_message.append(&mut buffer[..size].to_vec());

                    let message = ADBTransportMessage::try_new(
                        MessageCommand::Write,
                        self.local_id(),
                        self.remote_id(),
                        &serialized_message,
                    )?;

                    self.send_and_expect_okay(message).await?;
                }
                Err(e) => {
                    return Err(RustADBError::IOError(e));
                }
            }
        }
    }

    pub(crate) async fn stat_with_explicit_ids(
        &mut self,
        remote_path: &str,
    ) -> Result<AdbStatResponse> {
        let stat_buffer = MessageSubcommand::Stat.with_arg(u32::try_from(remote_path.len())?);
        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id(),
            self.remote_id(),
            &stat_buffer.encode(),
        )?;
        self.send_and_expect_okay(message).await?;
        self.send_and_expect_okay(ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id(),
            self.remote_id(),
            remote_path.as_bytes(),
        )?)
        .await?;

        let response = self.transport.read_message().await?;
        // Skip first 4 bytes as this is the literal "STAT".
        // Interesting part starts right after

        AdbStatResponse::decode(&response.into_payload()[4..])
    }
}

// NOTE (async teardown, P0-②): the previous synchronous `Drop` drained a
// trailing CLSE from the transport with a short blocking read. Under the async
// rewrite there is no async `Drop` in stable Rust and the transport read is now
// a future that cannot be awaited here. Session teardown is handled structurally
// by the transport layer (the persistent USB multiplexer enqueues a fire-and-
// forget CLSE on the writer task; the TCP transport closes the socket on drop),
// so the trailing-CLSE drain is dropped. Callers needing graceful close should
// use `end_transaction`, which reads the closing message explicitly.
