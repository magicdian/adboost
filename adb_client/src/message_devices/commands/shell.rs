use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::models::ADBLocalCommand;
use crate::{
    Result, RustADBError,
    message_devices::{
        adb_message_device::ADBMessageDevice, adb_message_transport::ADBMessageTransport,
        adb_transport_message::ADBTransportMessage, commands::utils::ShellMessageWriter,
        message_commands::MessageCommand,
    },
};

const SHELL_BUFFER_SIZE: usize = 1024;

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    /// Runs 'command' in a shell on the device, and write its output and error streams into output.
    pub(crate) async fn shell_command(
        &mut self,
        command: &(dyn AsRef<str> + Sync),
        mut stdout: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
        _stderr: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
    ) -> Result<Option<u8>> {
        let mut session = self
            .open_session(&ADBLocalCommand::ShellCommand(
                command.as_ref().to_string(),
                Vec::new(),
            ))
            .await?;

        loop {
            let message = session.recv_and_reply_okay().await?;
            if message.header().command() == MessageCommand::Clse {
                break;
            }
            // should this just write for ::Write messages?
            if let Some(ref mut stdout) = stdout {
                stdout.write_all(&message.into_payload()).await?;
            }
        }

        Ok(None)
    }

    /// Starts an interactive shell session on the device.
    /// Input data is read from [reader] and write to [writer].
    pub(crate) async fn shell(
        &mut self,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        self.bidirectional_session(&ADBLocalCommand::Shell, reader, writer)
            .await
    }

    /// Runs `command` on the device.
    /// Input data is read from [reader] and write to [writer].
    pub(crate) async fn exec(
        &mut self,
        command: &str,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        self.bidirectional_session(&ADBLocalCommand::Exec(command.to_string()), reader, writer)
            .await
    }

    /// Starts an bidirectional(interactive) session. This can be a shell or an exec session.
    async fn bidirectional_session(
        &mut self,
        local_command: &ADBLocalCommand,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        mut writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        let session = self.open_session(local_command).await?;

        let local_id = session.local_id();
        let remote_id = session.remote_id();

        let mut transport = self.get_transport_mut().clone();

        // Reading task, reads response from adbd and writes it into `writer`.
        let reader_task = tokio::spawn(async move {
            loop {
                let message = match transport.read_message().await {
                    Ok(message) => message,
                    Err(e) => break Err::<(), RustADBError>(e),
                };

                // Acknowledge for more data
                let response = match ADBTransportMessage::try_new(
                    MessageCommand::Okay,
                    local_id,
                    remote_id,
                    &[],
                ) {
                    Ok(response) => response,
                    Err(e) => break Err(e),
                };
                if let Err(e) = transport.write_message(response).await {
                    break Err(e);
                }

                match message.header().command() {
                    MessageCommand::Write => {
                        if let Err(e) = writer.write_all(&message.into_payload()).await {
                            break Err(RustADBError::IOError(e));
                        }
                        if let Err(e) = writer.flush().await {
                            break Err(RustADBError::IOError(e));
                        }
                    }
                    MessageCommand::Okay => {}
                    _ => break Err(RustADBError::ADBShellNotSupported),
                }
            }
        });

        let transport = self.get_transport_mut().clone();
        let mut shell_writer = ShellMessageWriter::new(transport, local_id, remote_id);

        // Read from given reader (that could be stdin e.g), and write content to device adbd
        let mut buffer = vec![0u8; SHELL_BUFFER_SIZE].into_boxed_slice();
        let copy_result = loop {
            match reader.read(&mut buffer).await {
                Ok(0) => break Ok(()),
                Ok(size) => {
                    if let Err(e) = shell_writer.write(&buffer[..size]).await {
                        break Err(e);
                    }
                }
                Err(e) => break Err(RustADBError::IOError(e)),
            }
        };

        // The reader task lives for the connection's lifetime; abort it once the
        // input side is done so the cloned transport is released.
        reader_task.abort();

        copy_result
    }
}
