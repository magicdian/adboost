use std::path::Path;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::{
    Result, RustADBError,
    models::{
        ADBCommand, ADBLocalCommand, InstallControl, InstallOptions, InstallOutcome,
        InstallProgress,
    },
    proxy::ADBProxyDevice,
    utils::check_extension_is_apk,
};

use super::response::{self, ReplyKind};

const INSTALL_BUFFER_SIZE: usize = 65535;

impl ADBProxyDevice {
    /// Install an APK on device.
    ///
    /// Compatibility wrapper over [`Self::install_with_options`]: no replacement,
    /// no permission grant, no progress and no cancellation. For upgrade installs,
    /// progress or cancellation, use the options-taking form.
    pub async fn install<P: AsRef<Path>>(&mut self, apk_path: P, user: Option<&str>) -> Result<()> {
        let options = InstallOptions {
            user: user.map(ToString::to_string),
            ..InstallOptions::default()
        };
        self.install_with_options(apk_path, &options, InstallControl::default())
            .await
            .map(|_| ())
    }

    /// Install an APK with typed flags, optional progress, and optional
    /// cancellation.
    ///
    /// The service string is
    /// `exec:cmd package 'install' [-r] [-g] [-t] [-d] [--user X] -S <size>`.
    /// The APK is streamed to the device, then the package manager's reply is
    /// read to EOF and classified:
    ///
    /// - `Success` → [`InstallOutcome`];
    /// - `Failure [REASON]` → [`RustADBError::InstallFailed`] with the parsed
    ///   reason;
    /// - anything else → [`RustADBError::InstallFailed`] with
    ///   `reason = "unknown response"`.
    ///
    /// # Cancellation
    ///
    /// Triggering `control.cancel` stops the local transfer and closes the
    /// connection, so the device-side install observes EOF and abandons the
    /// session. This is **best-effort**: the device may already have completed the
    /// installation before the abort lands. The connection is unusable afterwards;
    /// the next operation reconnects through the transport switch.
    ///
    /// # Errors
    ///
    /// Propagates I/O and transport errors, [`RustADBError::InstallFailed`], and
    /// [`RustADBError::OperationCancelled`].
    pub async fn install_with_options<P: AsRef<Path>>(
        &mut self,
        apk_path: P,
        options: &InstallOptions,
        control: InstallControl,
    ) -> Result<InstallOutcome> {
        let mut apk_file = tokio::fs::File::open(&apk_path).await?;
        check_extension_is_apk(&apk_path)?;
        let total_bytes = apk_file.metadata().await?.len();

        self.set_serial_transport().await?;
        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::InstallWithOptions(
                total_bytes,
                options.clone(),
            )))
            .await?;

        let stream_result = {
            let connection = self.transport.get_raw_connection()?;
            stream_apk(&mut apk_file, connection, total_bytes, &control).await
        };
        if let Err(error) = stream_result {
            if matches!(error, RustADBError::OperationCancelled) {
                // Best-effort abort: closing the socket makes the device's
                // `cmd package install` observe EOF and abandon the session.
                self.close_connection().await;
                return Err(RustADBError::OperationCancelled);
            }
            // The device may reject the payload early and send the reason before
            // closing. Recover it only when the error proves the peer is gone;
            // reading a live, half-open connection could hang.
            if is_peer_closed(&error)
                && let Some(failure) = self.read_aborted_install_reply().await
            {
                return Err(failure);
            }
            return Err(error);
        }

        let reply_result = {
            let connection = self.transport.get_raw_connection()?;
            response::read_reply(connection, control.cancel.as_ref()).await
        };
        let raw = match reply_result {
            Ok(raw) => raw,
            Err(RustADBError::OperationCancelled) => {
                self.close_connection().await;
                return Err(RustADBError::OperationCancelled);
            }
            Err(error) => return Err(error),
        };

        match response::parse_success_or_failure(&raw) {
            ReplyKind::Success => {
                tracing::info!(
                    "APK file {} successfully installed",
                    apk_path.as_ref().display()
                );
                Ok(InstallOutcome {
                    success: true,
                    package: None,
                    raw,
                })
            }
            ReplyKind::Failure(reason) => Err(RustADBError::InstallFailed { reason, raw }),
            ReplyKind::Unknown => Err(RustADBError::InstallFailed {
                reason: "unknown response".to_string(),
                raw,
            }),
        }
    }

    /// Recover a device-reported install failure after the upload aborted.
    ///
    /// Only called when the write error proves the peer closed, so the socket is
    /// guaranteed to be at (or reaching) EOF and the read cannot block.
    async fn read_aborted_install_reply(&mut self) -> Option<RustADBError> {
        let connection = self.transport.get_raw_connection().ok()?;
        let raw = response::read_reply(connection, None).await.ok()?;
        match response::parse_success_or_failure(&raw) {
            ReplyKind::Failure(reason) => Some(RustADBError::InstallFailed { reason, raw }),
            ReplyKind::Unknown if !raw.trim().is_empty() => Some(RustADBError::InstallFailed {
                reason: "unknown response".to_string(),
                raw,
            }),
            ReplyKind::Success | ReplyKind::Unknown => None,
        }
    }
}

/// Whether an I/O error proves the device side closed the connection.
fn is_peer_closed(error: &RustADBError) -> bool {
    const PEER_CLOSED: [std::io::ErrorKind; 4] = [
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::ConnectionAborted,
        std::io::ErrorKind::UnexpectedEof,
    ];
    matches!(error, RustADBError::IOError(io) if PEER_CLOSED.contains(&io.kind()))
}

/// Stream `reader` into `writer`, emitting progress and honoring cancellation.
///
/// Generic over the byte streams so progress monotonicity and the cancel path are
/// unit-testable with `tokio::io::duplex` (no socket, no hardware).
async fn stream_apk<R, W>(
    reader: &mut R,
    writer: &mut W,
    total_bytes: u64,
    control: &InstallControl,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let interval = control.progress_interval_bytes.max(1);
    let mut buffer = vec![0u8; INSTALL_BUFFER_SIZE];
    let mut bytes_sent: u64 = 0;
    let mut next_report_at = interval;
    let mut last_reported: Option<u64> = None;

    loop {
        let read = read_chunk(reader, &mut buffer, control.cancel.as_ref()).await?;
        if read == 0 {
            break;
        }

        write_chunk(writer, &buffer[..read], control.cancel.as_ref()).await?;
        bytes_sent += read as u64;

        if bytes_sent >= next_report_at || bytes_sent == total_bytes {
            emit_progress(control, bytes_sent, total_bytes);
            last_reported = Some(bytes_sent);
            next_report_at = next_report_at.saturating_add(interval);
        }
    }

    // Guarantee the receiver observes completion even for an empty file or when
    // the last chunk did not cross an interval boundary.
    if last_reported != Some(bytes_sent) {
        emit_progress(control, bytes_sent, total_bytes);
    }

    Ok(())
}

async fn read_chunk<R>(
    reader: &mut R,
    buffer: &mut [u8],
    cancel: Option<&CancellationToken>,
) -> Result<usize>
where
    R: AsyncRead + Unpin,
{
    if let Some(cancel) = cancel {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(RustADBError::OperationCancelled),
            result = reader.read(buffer) => Ok(result?),
        }
    } else {
        Ok(reader.read(buffer).await?)
    }
}

/// Write one chunk, aborting on cancellation.
///
/// `write_all` is not cancel-safe, but cancellation tears the connection down
/// immediately afterwards, so a partially written chunk is irrelevant.
async fn write_chunk<W>(
    writer: &mut W,
    chunk: &[u8],
    cancel: Option<&CancellationToken>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if let Some(cancel) = cancel {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(RustADBError::OperationCancelled),
            result = writer.write_all(chunk) => Ok(result?),
        }
    } else {
        Ok(writer.write_all(chunk).await?)
    }
}

/// Send a progress sample without ever blocking the transfer.
///
/// A full channel drops the sample: progress is an observation surface, not a
/// pacing mechanism.
fn emit_progress(control: &InstallControl, bytes_sent: u64, total_bytes: u64) {
    let Some(progress) = control.progress.as_ref() else {
        return;
    };
    let _ = progress.try_send(InstallProgress {
        bytes_sent,
        total_bytes,
    });
}

#[cfg(test)]
mod tests {
    use super::stream_apk;
    use crate::RustADBError;
    use crate::models::{InstallControl, InstallProgress};
    use std::time::Duration;
    use tokio::io::{AsyncWriteExt, duplex, sink};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn progress_is_monotonic_and_ends_at_total() {
        let data = vec![7u8; 2 * 1024 * 1024 + 13];
        let total = data.len() as u64;
        let (mut reader, mut writer) = duplex(4096);
        let write_task = tokio::spawn(async move {
            writer
                .write_all(&data)
                .await
                .expect("duplex write cannot fail");
        });

        let (progress_tx, mut progress_rx) = mpsc::channel(16);
        let control = InstallControl {
            progress: Some(progress_tx),
            cancel: None,
            progress_interval_bytes: 1024 * 1024,
        };

        let mut out = sink();
        stream_apk(&mut reader, &mut out, total, &control)
            .await
            .expect("streaming must succeed");
        drop(control);
        write_task.await.expect("writer task must finish");

        let mut samples = Vec::new();
        while let Some(sample) = progress_rx.recv().await {
            samples.push(sample);
        }

        assert!(samples.len() >= 2, "expected interval + final samples");
        assert!(
            samples
                .windows(2)
                .all(|pair| pair[0].bytes_sent <= pair[1].bytes_sent),
            "progress must be monotonic: {samples:?}"
        );
        assert_eq!(
            samples.last(),
            Some(&InstallProgress {
                bytes_sent: total,
                total_bytes: total
            }),
            "the final sample must report completion"
        );
    }

    #[tokio::test]
    async fn empty_file_reports_zero_progress() {
        let (mut reader, writer) = duplex(16);
        drop(writer);

        let (progress_tx, mut progress_rx) = mpsc::channel(4);
        let control = InstallControl {
            progress: Some(progress_tx),
            ..InstallControl::default()
        };
        let mut out = sink();
        stream_apk(&mut reader, &mut out, 0, &control)
            .await
            .expect("an empty file is a valid install payload");
        drop(control);

        assert_eq!(
            progress_rx.recv().await,
            Some(InstallProgress {
                bytes_sent: 0,
                total_bytes: 0
            }),
            "an empty payload must still report completion"
        );
    }

    #[tokio::test]
    async fn cancellation_mid_stream_returns_operation_cancelled() {
        let (mut reader, mut writer) = duplex(128);
        writer
            .write_all(&[0u8; 100])
            .await
            .expect("duplex write cannot fail");

        let token = CancellationToken::new();
        let control = InstallControl {
            progress: None,
            cancel: Some(token.clone()),
            progress_interval_bytes: 1,
        };

        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        });

        let mut out = sink();
        let error = stream_apk(&mut reader, &mut out, 1_000_000, &control)
            .await
            .expect_err("a blocked stream must observe cancellation");
        assert!(
            matches!(error, RustADBError::OperationCancelled),
            "cancellation must surface as OperationCancelled: {error:?}"
        );

        drop(writer);
        cancel_task.await.expect("cancel task must finish");
    }
}
