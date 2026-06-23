use std::{io::ErrorKind, path::Path, pin::Pin};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    ADBDeviceExt, ADBListItemType, Result, RustADBError,
    models::{ADBCommand, ADBLocalCommand, AdbStatResponse, HostFeatures, RemountInfo},
};

use super::ADBProxyDevice;

const BUFFER_SIZE: usize = 65535;

#[derive(Eq, PartialEq)]
enum ShellChannel {
    Stdout,
    Stderr,
    ExitStatus,
}

impl TryFrom<u8> for ShellChannel {
    type Error = std::io::Error;

    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Stdout),
            2 => Ok(Self::Stderr),
            3 => Ok(Self::ExitStatus),
            _ => Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "Invalid channel",
            )),
        }
    }
}

impl ADBDeviceExt for ADBProxyDevice {
    async fn shell_command(
        &mut self,
        command: &(dyn AsRef<str> + Sync),
        stdout: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
        stderr: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
    ) -> Result<Option<u8>> {
        let supported_features = self.host_features().await;
        let use_shell_v2 = supported_features.is_ok_and(|features| {
            features.contains(&HostFeatures::ShellV2) || features.contains(&HostFeatures::Cmd)
        });

        self.set_serial_transport().await?;

        if use_shell_v2 {
            self.shell_command_v2(command, stdout, stderr).await
        } else {
            self.shell_command_v1(command, stdout).await
        }
    }

    async fn stat(&mut self, remote_path: &(dyn AsRef<str> + Sync)) -> Result<AdbStatResponse> {
        self.stat(remote_path.as_ref()).await
    }

    async fn exec(
        &mut self,
        command: &str,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        self.bidirectional_session(
            &ADBCommand::Local(ADBLocalCommand::Exec(command.to_owned())),
            reader,
            writer,
        )
        .await
    }

    async fn shell(
        &mut self,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        self.bidirectional_session(&ADBCommand::Local(ADBLocalCommand::Shell), reader, writer)
            .await
    }

    async fn pull(
        &mut self,
        source: &(dyn AsRef<str> + Sync),
        output: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<()> {
        self.pull(source, output).await
    }

    async fn reboot(&mut self, reboot_type: crate::RebootType) -> Result<()> {
        self.reboot(reboot_type).await
    }

    async fn root(&mut self) -> Result<()> {
        self.root().await
    }

    async fn unroot(&mut self) -> Result<()> {
        self.unroot().await
    }

    async fn push(
        &mut self,
        stream: &mut (dyn AsyncRead + Unpin + Send),
        path: &(dyn AsRef<str> + Sync),
    ) -> Result<()> {
        self.push(stream, path.as_ref()).await
    }

    async fn install(
        &mut self,
        apk_path: &(dyn AsRef<Path> + Sync),
        user: Option<&str>,
    ) -> Result<()> {
        self.install(apk_path, user).await
    }

    async fn uninstall(
        &mut self,
        package: &(dyn AsRef<str> + Sync),
        user: Option<&str>,
    ) -> Result<()> {
        self.uninstall(package.as_ref(), user).await
    }

    #[cfg(feature = "framebuffer")]
    async fn framebuffer_inner(&mut self) -> Result<image::ImageBuffer<image::Rgba<u8>, Vec<u8>>> {
        self.framebuffer_inner().await
    }

    async fn list(&mut self, path: &(dyn AsRef<str> + Sync)) -> Result<Vec<ADBListItemType>> {
        self.list(path.as_ref()).await
    }

    async fn remount(&mut self) -> Result<Vec<RemountInfo>> {
        self.remount().await
    }

    async fn enable_verity(&mut self) -> Result<()> {
        self.enable_verity().await
    }

    async fn disable_verity(&mut self) -> Result<()> {
        self.disable_verity().await
    }

    async fn tcpip(&mut self, port: u16) -> Result<String> {
        self.tcpip(port).await
    }

    async fn usb(&mut self) -> Result<()> {
        self.usb().await
    }
}

impl ADBProxyDevice {
    /// Shell v1: simple shell without protocol (for older ADB versions)
    async fn shell_command_v1(
        &mut self,
        command: &(dyn AsRef<str> + Sync),
        mut stdout: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
    ) -> Result<Option<u8>> {
        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::ShellCommand(
                command.as_ref().to_string(),
                vec![],
            )))
            .await?;

        let input = self.transport.get_raw_connection()?;
        let mut buffer = vec![0; BUFFER_SIZE].into_boxed_slice();

        loop {
            match input.read(&mut buffer).await {
                Ok(0) => break,
                Ok(size) => {
                    if let Some(stdout) = stdout.as_mut() {
                        stdout.write_all(&buffer[..size]).await?;
                    }
                }
                Err(e) => match e.kind() {
                    ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe => break,
                    _ => return Err(RustADBError::IOError(e)),
                },
            }
        }

        Ok(None)
    }

    /// Shell v2: with protocol packets (for newer ADB versions)
    async fn shell_command_v2(
        &mut self,
        command: &(dyn AsRef<str> + Sync),
        stdout: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
        stderr: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
    ) -> Result<Option<u8>> {
        let mut args = vec!["v2".to_string()];

        if let Ok(term) = std::env::var("TERM") {
            args.push(format!("TERM={term}"));
        }

        // Send the request
        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::ShellCommand(
                command.as_ref().to_string(),
                args,
            )))
            .await?;

        // Now decode the shell v2 protocol packets, reference:
        // https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/shell_protocol.h
        let input = self.transport.get_raw_connection()?;
        decode_shell_v2_stream(input, stdout, stderr).await
    }

    async fn bidirectional_session(
        &mut self,
        server_cmd: &ADBCommand,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        mut writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        // For bidirectional session, we still need shell features
        // But we can try without checking features first
        self.set_serial_transport().await?;
        self.transport.send_adb_request(server_cmd).await?;

        // Split the single connection into independent read/write halves so the
        // outbound (stdin -> socket) and inbound (socket -> writer) directions can
        // be driven concurrently without cloning the stream (no `try_clone`).
        let connection = self.transport.get_raw_connection()?;
        let (mut read_half, mut write_half) = tokio::io::split(connection);

        // Inbound: socket -> writer (runs concurrently with the outbound copy).
        let reader_fut = async {
            let mut buffer = vec![0u8; BUFFER_SIZE].into_boxed_slice();
            loop {
                match read_half.read(&mut buffer).await {
                    Ok(0) => return Ok::<(), RustADBError>(()),
                    Ok(size) => {
                        writer.write_all(&buffer[..size]).await?;
                        writer.flush().await?;
                    }
                    Err(e) => return Err(RustADBError::IOError(e)),
                }
            }
        };

        // Outbound: reader (e.g. stdin) -> socket.
        let writer_fut = async {
            let mut buffer = vec![0u8; BUFFER_SIZE].into_boxed_slice();
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => return Ok::<(), RustADBError>(()),
                    Ok(size) => {
                        if let Err(e) = write_half.write_all(&buffer[..size]).await {
                            match e.kind() {
                                ErrorKind::BrokenPipe => return Ok(()),
                                _ => return Err(RustADBError::IOError(e)),
                            }
                        }
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::BrokenPipe => return Ok(()),
                        _ => return Err(RustADBError::IOError(e)),
                    },
                }
            }
        };

        // Whichever direction finishes first ends the session.
        tokio::select! {
            res = reader_fut => res,
            res = writer_fut => res,
        }
    }
}

/// Decode a shell-v2 protocol byte stream (channel-tagged frames) from `input`,
/// routing stdout/stderr payloads to the given writers and returning the exit
/// code once the stream ends.
///
/// The exit-status frame is followed by the device tearing the stream down, so
/// the END OF STREAM is the normal terminator of a v2 shell command. Every EOF
/// site therefore returns the exit code captured SO FAR (`Ok(exit)`), never an
/// unconditional `Ok(None)` — a between-frames EOF after the exit-status frame
/// must not discard the code we already decoded. This mirrors
/// [`crate::message_devices::usb::shell_v2_session::ShellV2Session::execute`].
///
/// Extracted as a free function over `AsyncRead` so the EOF/exit-code behavior
/// is unit-testable from an in-memory cursor without a live device.
async fn decode_shell_v2_stream<R: AsyncRead + Unpin>(
    input: &mut R,
    mut stdout: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
    mut stderr: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
) -> Result<Option<u8>> {
    let mut exit = None;
    let mut buffer = vec![0; BUFFER_SIZE].into_boxed_slice();
    loop {
        // 1 byte of channel + 4 bytes of payload size.
        let mut pckt_metadata = [0u8; 5];
        if let Err(err) = input.read_exact(&mut pckt_metadata).await {
            match err.kind() {
                // Stream closed cleanly between frames — return whatever exit
                // code we already captured. Returning `Ok(None)` here would
                // discard a code decoded from a prior exit-status frame (the
                // through-server path always hits this trailing EOF).
                ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe => return Ok(exit),
                _ => return Err(RustADBError::IOError(err)),
            }
        }

        let (channel, payload_size) = {
            let channel = pckt_metadata[0];
            let payload_size = u32::from_le_bytes(pckt_metadata[1..5].try_into()?) as usize;
            (ShellChannel::try_from(channel)?, payload_size)
        };

        if payload_size == 0 {
            continue;
        }

        match channel {
            ShellChannel::Stdout | ShellChannel::Stderr => {
                let mut remainder = payload_size;
                while remainder > 0 {
                    let to_read = std::cmp::min(remainder, BUFFER_SIZE);
                    match input.read(&mut buffer[0..to_read]).await {
                        Ok(size) => {
                            if size == 0 {
                                return Ok(exit);
                            }

                            match channel {
                                ShellChannel::Stdout => {
                                    if let Some(stdout) = stdout.as_mut() {
                                        stdout.write_all(&buffer[..size]).await?;
                                    }
                                }
                                ShellChannel::Stderr => {
                                    // first stderr if existing, else a merged output into stdout
                                    if let Some(writer) = stderr.as_mut() {
                                        writer.write_all(&buffer[..size]).await?;
                                    } else if let Some(writer) = stdout.as_mut() {
                                        writer.write_all(&buffer[..size]).await?;
                                    }
                                }
                                ShellChannel::ExitStatus => {
                                    // unreachable
                                }
                            }

                            remainder -= size;
                        }
                        Err(e) => {
                            return Err(RustADBError::IOError(e));
                        }
                    }
                }
            }
            ShellChannel::ExitStatus => {
                if payload_size != 1 {
                    return Err(RustADBError::ADBShellV2ParseError(format!(
                        "Spurious exit status packet with size of {payload_size} (should be 1)"
                    )));
                }

                match input.read_u8().await {
                    Ok(status) => exit = Some(status),
                    Err(err) => match err.kind() {
                        // A truncated exit-status frame: `exit` is still None here,
                        // so this is equivalent to `Ok(None)` — kept as `Ok(exit)`
                        // for symmetry with the between-frames EOF above.
                        ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe => return Ok(exit),
                        _ => return Err(RustADBError::IOError(err)),
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a shell-v2 frame: 1-byte channel + 4-byte LE payload size + payload.
    fn frame(channel: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![channel];
        v.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// The regression: stdout frame, then an exit-status frame, then EOF. The
    /// trailing EOF on the next header read must NOT discard the captured code.
    #[tokio::test]
    async fn exit_code_survives_trailing_eof_after_exit_frame() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&frame(1, b"hi\n")); // stdout
        stream.extend_from_slice(&frame(3, &[1])); // exit status = 1
        // stream simply ends here (device tore the connection down)

        let mut out = Vec::new();
        let mut cursor = Cursor::new(stream);
        let code = decode_shell_v2_stream(
            &mut cursor,
            Some(&mut out as &mut (dyn AsyncWrite + Unpin + Send)),
            None,
        )
        .await
        .expect("decode must succeed");

        assert_eq!(code, Some(1), "exit code must survive the trailing EOF");
        assert_eq!(out, b"hi\n");
    }

    /// A non-zero exit code with no preceding output still decodes.
    #[tokio::test]
    async fn exit_code_only_stream() {
        let stream = frame(3, &[42]);
        let mut cursor = Cursor::new(stream);
        let code = decode_shell_v2_stream(&mut cursor, None, None)
            .await
            .expect("decode must succeed");
        assert_eq!(code, Some(42));
    }

    /// A stream that closes before any exit-status frame yields `None` (no code
    /// was ever sent) — not an error.
    #[tokio::test]
    async fn no_exit_frame_yields_none() {
        let stream = frame(1, b"output");
        let mut out = Vec::new();
        let mut cursor = Cursor::new(stream);
        let code = decode_shell_v2_stream(
            &mut cursor,
            Some(&mut out as &mut (dyn AsyncWrite + Unpin + Send)),
            None,
        )
        .await
        .expect("decode must succeed");
        assert_eq!(code, None, "no exit-status frame ⇒ None");
        assert_eq!(out, b"output");
    }

    /// stdout and stderr route to their respective writers.
    #[tokio::test]
    async fn stdout_stderr_split() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&frame(1, b"out"));
        stream.extend_from_slice(&frame(2, b"err"));
        stream.extend_from_slice(&frame(3, &[0]));

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut cursor = Cursor::new(stream);
        let code = decode_shell_v2_stream(
            &mut cursor,
            Some(&mut out as &mut (dyn AsyncWrite + Unpin + Send)),
            Some(&mut err as &mut (dyn AsyncWrite + Unpin + Send)),
        )
        .await
        .expect("decode must succeed");

        assert_eq!(code, Some(0));
        assert_eq!(out, b"out");
        assert_eq!(err, b"err");
    }
}
