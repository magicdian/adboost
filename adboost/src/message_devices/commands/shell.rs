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

        // Cooperative shutdown signal: the input side fires this once it sees
        // EOF/error so the reader exits at a SAFE point (between reads) instead
        // of being `abort`ed mid-`write_all`/`flush`. Aborting in the middle of
        // a write would drop device output we have already protocol-ACKed,
        // silently truncating interactive shell output (H2).
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // Reading task, reads response from adbd and writes it into `writer`.
        let reader_task = tokio::spawn(async move {
            loop {
                // Race the next read against the shutdown signal. `read_message`
                // is the cancel-safe atomic unit (one ADB frame), so cancelling
                // it here loses nothing; and because the signal is only observed
                // between reads, any in-flight write below always runs to
                // completion before the loop can exit.
                let message = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break Ok::<(), RustADBError>(()),
                    read = transport.read_message() => match read {
                        Ok(message) => message,
                        Err(e) => break Err(e),
                    },
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
                        // Not inside the `select!`: once a frame is read and
                        // ACKed, it MUST be fully flushed to `writer` before we
                        // can observe the shutdown signal and exit.
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

        // Input side is done. Signal the reader to stop at its next safe point
        // (between reads) rather than aborting it mid-write, then wait for it to
        // finish so any frame it had already read+ACKed is fully flushed to the
        // writer before we return. This mirrors the synchronous original's
        // intent (let the reader complete its current output) without leaking a
        // task: the cooperative signal guarantees the reader unblocks even when
        // the device is silent (the read loop has no inner timeout).
        let _ = shutdown_tx.send(());
        match reader_task.await {
            Ok(Ok(())) => {}
            // The reader commonly ends with a disconnect/CLSE error once the
            // stream tears down; the synchronous original ran it on a detached
            // thread and never observed this. Preserve that interactive
            // semantic (input EOF drives the return value) but make the reader
            // outcome observable instead of silently discarded.
            Ok(Err(e)) => tracing::debug!("shell reader task ended: {e}"),
            Err(join_err) => tracing::warn!("shell reader task did not join cleanly: {join_err}"),
        }

        copy_result
    }
}
