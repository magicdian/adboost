use std::{sync::Arc, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};

use crate::{
    Result, RustADBError,
    adb_transport::ADBTransport,
    message_devices::{
        adb_message_transport::ADBMessageTransport, adb_transport_message::ADBTransportMessage,
        framed_stream::FramedStream,
    },
};

/// A full-duplex byte stream usable as an ADB transport.
///
/// One helper trait (rather than `Box<dyn AsyncRead + AsyncWrite>`) is needed
/// because a trait object may name only one non-auto trait. The blanket impl
/// covers every `AsyncRead + AsyncWrite` type, so callers never implement it.
pub trait AsyncStream: AsyncRead + AsyncWrite {}

impl<T: AsyncRead + AsyncWrite + ?Sized> AsyncStream for T {}

/// The type-erased stream a [`StreamTransport`] owns.
pub type StreamConnection = Box<dyn AsyncStream + Unpin + Send + 'static>;

/// An ADB transport over an **already-connected** arbitrary byte stream.
///
/// Use it to run the proven [`PersistentConnection`] state machine (CNXN, AUTH,
/// OPEN/WRTE/CLSE, shell v1/v2, SYNC) over a stream that is not a `TcpStream` —
/// e.g. the raw pipe of another ADB device's local service, a
/// [`tokio::io::DuplexStream`], a Unix socket, or a TLS tunnel established by
/// the caller.
///
/// Framing, cancel-safety, independent read/write locks, and write poisoning are
/// the shared `FramedStream` driver used by [`TcpTransport`](crate::TcpTransport);
/// this type only owns the stream and its diagnostic label.
///
/// [`PersistentConnection`]: crate::usb::PersistentConnection
pub struct StreamTransport {
    stream: FramedStream<StreamConnection>,
}

impl StreamTransport {
    /// Wrap an already-connected stream.
    ///
    /// This does **not** perform any I/O: `ADBTransport::connect()` is a no-op.
    /// The caller is responsible for establishing (and re-establishing) the
    /// stream. `label` is used only in diagnostics, e.g. `"tbox@YTG..."`.
    pub fn new<S>(stream: S, label: impl Into<String>) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let stream: StreamConnection = Box::new(stream);
        Self {
            stream: FramedStream::new(stream, Arc::from(label.into())),
        }
    }

    /// Wrap an already-connected TCP socket, disabling Nagle.
    ///
    /// ADB multiplexing is small-packet and interactive, so `TCP_NODELAY` matters
    /// for shell latency (same rationale as
    /// [`TcpTransport`](crate::TcpTransport)); a socket-option failure is surfaced
    /// rather than silently ignored.
    ///
    /// # Errors
    ///
    /// Propagates the `setsockopt` failure from [`TcpStream::set_nodelay`].
    pub fn from_tcp_stream(stream: TcpStream, label: impl Into<String>) -> Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self::new(stream, label))
    }

    /// The diagnostic label supplied at construction.
    #[must_use]
    pub fn label(&self) -> &str {
        self.stream.peer()
    }
}

impl Clone for StreamTransport {
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
        }
    }
}

impl std::fmt::Debug for StreamTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamTransport")
            .field("label", &self.label())
            .finish_non_exhaustive()
    }
}

impl ADBTransport for StreamTransport {
    /// No-op: [`StreamTransport::new`] means the stream is already connected.
    fn connect(&mut self) -> impl std::future::Future<Output = Result<()>> + Send {
        std::future::ready(Ok(()))
    }

    /// Shut down the write half. The read half is the caller's to close.
    async fn disconnect(&mut self) -> Result<()> {
        self.stream.shutdown().await;
        Ok(())
    }
}

impl ADBMessageTransport for StreamTransport {
    async fn read_message_with_timeout(
        &mut self,
        read_timeout: Duration,
    ) -> Result<ADBTransportMessage> {
        self.stream.read_message(read_timeout).await
    }

    async fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        write_timeout: Duration,
    ) -> Result<()> {
        self.stream.write_message(message, write_timeout).await
    }

    /// A wrapped stream cannot be re-established as TLS in place, so refuse STLS
    /// explicitly instead of answering the handshake and continuing in plaintext.
    /// A caller that needs an encrypted link must establish it before wrapping.
    fn upgrade_connection(&mut self) -> impl std::future::Future<Output = Result<()>> + Send {
        let label = self.label().to_owned();
        std::future::ready(Err(RustADBError::UpgradeError(format!(
            "STLS is not supported over stream transport \"{label}\""
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::StreamTransport;
    use crate::RustADBError;
    use crate::message_devices::adb_message_transport::ADBMessageTransport;
    use crate::message_devices::adb_transport_message::{
        ADBTransportMessage, ADBTransportMessageHeader, HEADER_LENGTH, MAX_PAYLOAD,
    };
    use crate::message_devices::message_commands::MessageCommand;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

    const LEGACY_VERSION: u32 = 0x0100_0000;
    const DEVICE_LOCAL_ID: u32 = 0x5151_5151;

    /// Serialize one frame to its exact on-wire bytes.
    fn wire_frame(command: MessageCommand, arg0: u32, arg1: u32, payload: &[u8]) -> Vec<u8> {
        let message =
            ADBTransportMessage::try_new(command, arg0, arg1, payload).expect("build frame");
        let mut bytes = message.header().as_bytes();
        bytes.extend_from_slice(message.payload());
        bytes
    }

    /// Build a raw 24-byte header with explicit `data_length` and `magic`, so a
    /// test can forge an oversize / bad-magic / unknown-command header.
    fn raw_header(command_word: u32, data_length: u32, magic: u32) -> [u8; HEADER_LENGTH] {
        let mut buf = [0_u8; HEADER_LENGTH];
        buf[0..4].copy_from_slice(&command_word.to_le_bytes());
        buf[12..16].copy_from_slice(&data_length.to_le_bytes());
        buf[20..24].copy_from_slice(&magic.to_le_bytes());
        buf
    }

    /// The far end of a duplex pair, speaking raw ADB device frames.
    struct FakeAdbd {
        io: DuplexStream,
    }

    impl FakeAdbd {
        fn new(io: DuplexStream) -> Self {
            Self { io }
        }

        async fn read_frame(&mut self) -> crate::Result<ADBTransportMessage> {
            let mut header_bytes = [0_u8; HEADER_LENGTH];
            self.io.read_exact(&mut header_bytes).await?;
            let header = ADBTransportMessageHeader::try_from(header_bytes)?;
            let mut payload = vec![0_u8; header.data_length() as usize];
            self.io.read_exact(&mut payload).await?;
            Ok(ADBTransportMessage::from_header_and_payload(
                header, payload,
            ))
        }

        /// Read until the next frame with `command`, skipping unrelated ones (the
        /// host interleaves OKAY readiness/acks with OPENs).
        async fn next_command(
            &mut self,
            command: MessageCommand,
        ) -> crate::Result<ADBTransportMessage> {
            loop {
                let message = self.read_frame().await?;
                if message.header().command() == command {
                    return Ok(message);
                }
            }
        }

        async fn write_frame(
            &mut self,
            command: MessageCommand,
            arg0: u32,
            arg1: u32,
            payload: &[u8],
        ) -> crate::Result<()> {
            let message = ADBTransportMessage::try_new(command, arg0, arg1, payload)?;
            let mut bytes = message.header().as_bytes();
            bytes.extend_from_slice(message.payload());
            self.io.write_all(&bytes).await?;
            self.io.flush().await?;
            Ok(())
        }

        async fn send_legacy_banner(&mut self) -> crate::Result<()> {
            self.write_frame(
                MessageCommand::Cnxn,
                LEGACY_VERSION,
                1_048_576,
                b"device::ro.product.name=tbox;ro.product.model=TBox",
            )
            .await
        }

        /// Keep the duplex end alive until the host closes it.
        ///
        /// Dropping a `DuplexStream` immediately discards anything still buffered
        /// in the pipe, so the fake device must NOT exit right after writing its
        /// reply frames — the host has not necessarily read them yet.
        async fn drain_until_eof(&mut self) {
            let mut buf = [0_u8; 1024];
            loop {
                match self.io.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        }
    }

    fn nonexistent_key_path() -> std::path::PathBuf {
        // `read_adb_private_key` returns None on NotFound, so the connection
        // generates an in-memory key and the test stays hermetic (no HOME/lookup).
        std::path::PathBuf::from("/nonexistent/adboost-stream-test/adbkey")
    }

    /// §6.1: a legacy (features-less) adbd over `tokio::io::duplex` completes
    /// CNXN, opens `shell:`, echoes a payload, and negotiates *classic* flow
    /// control because the peer advertises nothing.
    #[cfg(feature = "usb")]
    #[tokio::test]
    async fn from_stream_completes_legacy_handshake_and_shell_echo() {
        use crate::message_devices::usb::persistent::PersistentConnection;
        use crate::models::ADBLocalCommand;

        let (host, device) = tokio::io::duplex(64 * 1024);
        let device_task = tokio::spawn(async move {
            let mut adbd = FakeAdbd::new(device);
            adbd.next_command(MessageCommand::Cnxn)
                .await
                .expect("CNXN from host");
            adbd.send_legacy_banner().await.expect("CNXN banner");
            let open = adbd
                .next_command(MessageCommand::Open)
                .await
                .expect("OPEN from host");
            assert!(
                open.payload().starts_with(b"shell:echo hi"),
                "the OPEN service must be the shell command (got {:?})",
                String::from_utf8_lossy(open.payload())
            );
            let host_id = open.header().arg0();
            adbd.write_frame(MessageCommand::Okay, DEVICE_LOCAL_ID, host_id, &[])
                .await
                .expect("OKAY");
            adbd.write_frame(MessageCommand::Write, DEVICE_LOCAL_ID, host_id, b"hi\n")
                .await
                .expect("WRTE");
            adbd.write_frame(MessageCommand::Clse, DEVICE_LOCAL_ID, host_id, &[])
                .await
                .expect("CLSE");
            adbd.drain_until_eof().await;
        });

        // The timeout is a hang guard only: the connection generates a fresh
        // 2048-bit RSA key, which is CPU-bound and can be slow in a debug build
        // under a heavily loaded parallel test run.
        let conn = tokio::time::timeout(
            Duration::from_secs(60),
            PersistentConnection::from_stream(host, Some(nonexistent_key_path())),
        )
        .await
        .expect("handshake must not hang")
        .expect("CNXN over the duplex stream");

        assert!(
            !conn.peer_features().delayed_ack,
            "a legacy banner advertises no delayed_ack"
        );
        assert!(
            !conn.peer_features().shell_v2,
            "a legacy banner advertises no shell_v2"
        );
        assert!(
            !conn.delayed_ack_negotiated(),
            "a legacy peer must fall back to classic stop-and-wait"
        );

        let mut session = conn
            .open_session(&ADBLocalCommand::ShellCommand("echo hi".into()))
            .await
            .expect("open shell session");
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), session.read_to_end(&mut out))
            .await
            .expect("session read must not hang")
            .expect("read echo payload");
        assert_eq!(
            out.as_slice(),
            b"hi\n",
            "the echoed payload must arrive intact"
        );

        // Closing the connection lets the fake device observe EOF and finish.
        drop(session);
        drop(conn);
        tokio::time::timeout(Duration::from_secs(5), device_task)
            .await
            .expect("device task must finish")
            .expect("device task must not panic");
    }

    /// §6.2: a payload split across a read-timeout boundary must be reassembled
    /// intact and keep the stream aligned for the next frame.
    #[tokio::test]
    async fn partial_payload_across_read_timeout_stays_aligned() {
        let (host, mut peer) = tokio::io::duplex(64 * 1024);
        let mut transport = StreamTransport::new(host, "test");
        let frame = wire_frame(MessageCommand::Write, 7, 8, b"split payload");
        let split_at = HEADER_LENGTH + 4;
        peer.write_all(&frame[..split_at])
            .await
            .expect("send header + partial payload");
        peer.flush().await.expect("flush");

        let timed_out = transport
            .read_message_with_timeout(Duration::from_millis(50))
            .await;
        assert!(
            matches!(timed_out, Err(RustADBError::ReadTimeout)),
            "a mid-frame read must surface ReadTimeout (got {timed_out:?})"
        );

        peer.write_all(&frame[split_at..])
            .await
            .expect("send rest of payload");
        peer.flush().await.expect("flush");

        let message = tokio::time::timeout(
            Duration::from_secs(2),
            transport.read_message_with_timeout(Duration::from_secs(1)),
        )
        .await
        .expect("read must not hang")
        .expect("the frame must complete");
        assert_eq!(
            message.header().command(),
            MessageCommand::Write,
            "the buffered bytes must still decode as the original command"
        );
        assert_eq!(
            message.payload().as_slice(),
            b"split payload",
            "the payload must be reassembled across the timeout"
        );
    }

    /// §6.2: the same guarantee for a partial *header*.
    #[tokio::test]
    async fn partial_header_across_read_timeout_stays_aligned() {
        let (host, mut peer) = tokio::io::duplex(64 * 1024);
        let mut transport = StreamTransport::new(host, "test");
        let frame = wire_frame(MessageCommand::Okay, 1, 2, b"");
        peer.write_all(&frame[..10])
            .await
            .expect("send partial header");
        peer.flush().await.expect("flush");

        let timed_out = transport
            .read_message_with_timeout(Duration::from_millis(50))
            .await;
        assert!(
            matches!(timed_out, Err(RustADBError::ReadTimeout)),
            "a partial header must surface ReadTimeout (got {timed_out:?})"
        );

        peer.write_all(&frame[10..]).await.expect("send rest");
        peer.flush().await.expect("flush");

        let message = tokio::time::timeout(
            Duration::from_secs(2),
            transport.read_message_with_timeout(Duration::from_secs(1)),
        )
        .await
        .expect("read must not hang")
        .expect("the frame must complete");
        assert_eq!(
            message.header().command(),
            MessageCommand::Okay,
            "the ten buffered header bytes must not be lost"
        );
    }

    /// §6.3: a blocked read must not stall a concurrent write (independent halves).
    #[tokio::test]
    async fn read_does_not_block_concurrent_write() {
        let (host, _peer) = tokio::io::duplex(64 * 1024);
        let mut transport = StreamTransport::new(host, "test");
        let mut reader_transport = transport.clone();
        let reader = tokio::spawn(async move {
            let _ = reader_transport
                .read_message_with_timeout(Duration::from_secs(1))
                .await;
        });

        // Let the reader acquire the read half and park in the stream read.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let message =
            ADBTransportMessage::try_new(MessageCommand::Cnxn, 0, 0, b"test").expect("build CNXN");
        let write = transport.write_message_with_timeout(message, Duration::from_secs(2));
        let outcome = tokio::time::timeout(Duration::from_millis(200), write).await;
        assert!(
            matches!(outcome, Ok(Ok(()))),
            "write must not wait behind the reader's idle read (got {outcome:?})"
        );

        reader.abort();
    }

    /// §6.4: a write that makes no progress before committing any byte is the
    /// recoverable `WriteTimeout`; the transport stays usable afterwards.
    #[tokio::test]
    async fn write_start_gate_timeout_keeps_transport_usable() {
        let accept = Arc::new(AtomicUsize::new(0));
        let mut transport = StreamTransport::new(
            StallStream {
                accept: Arc::clone(&accept),
                written: 0,
            },
            "mock",
        );
        let message =
            ADBTransportMessage::try_new(MessageCommand::Okay, 0, 0, b"").expect("build frame");

        let first = transport
            .write_message_with_timeout(message.clone(), Duration::from_millis(50))
            .await;
        assert!(
            matches!(first, Err(RustADBError::WriteTimeout)),
            "an uncommitted stall must be WriteTimeout (got {first:?})"
        );

        accept.store(1024, Ordering::SeqCst);
        let second = transport
            .write_message_with_timeout(message, Duration::from_secs(1))
            .await;
        assert!(
            second.is_ok(),
            "the write half must NOT be poisoned by a start-gate timeout (got {second:?})"
        );
    }

    /// §6.4: a write that fails after committing part of the frame poisons the
    /// write half, so a truncated frame is never followed by another frame.
    #[tokio::test]
    async fn mid_frame_write_error_poisons_the_write_half() {
        let mut transport = StreamTransport::new(FailingStream { written: 0 }, "mock");
        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            1,
            2,
            b"a payload long enough to require several writes",
        )
        .expect("build frame");

        let first = transport
            .write_message_with_timeout(message.clone(), Duration::from_secs(1))
            .await;
        assert!(
            matches!(first, Err(RustADBError::IOError(ref e)) if e.kind() == std::io::ErrorKind::BrokenPipe),
            "a mid-frame IO error must be fatal (got {first:?})"
        );

        let second = transport
            .write_message_with_timeout(message, Duration::from_secs(1))
            .await;
        assert!(
            matches!(second, Err(RustADBError::IOError(ref e)) if e.kind() == std::io::ErrorKind::NotConnected),
            "a fatal mid-frame write must poison the write half (got {second:?})"
        );
    }

    /// §6.5: two concurrent sessions are demultiplexed by local id over one
    /// `StreamTransport`.
    #[cfg(feature = "usb")]
    #[tokio::test]
    async fn two_sessions_interleave_by_local_id() {
        use crate::message_devices::usb::persistent::PersistentConnection;
        use crate::models::ADBLocalCommand;

        let (host, device) = tokio::io::duplex(64 * 1024);
        let device_task = tokio::spawn(async move {
            let mut adbd = FakeAdbd::new(device);
            adbd.next_command(MessageCommand::Cnxn).await.expect("CNXN");
            adbd.send_legacy_banner().await.expect("banner");

            for index in 0..2 {
                let open = adbd.next_command(MessageCommand::Open).await.expect("OPEN");
                let host_id = open.header().arg0();
                let remote_id = 0x1000 + index;
                let mut service = open.payload().clone();
                while service.last() == Some(&0) {
                    service.pop();
                }
                adbd.write_frame(MessageCommand::Okay, remote_id, host_id, &[])
                    .await
                    .expect("OKAY");
                adbd.write_frame(MessageCommand::Write, remote_id, host_id, &service)
                    .await
                    .expect("WRTE");
                adbd.write_frame(MessageCommand::Clse, remote_id, host_id, &[])
                    .await
                    .expect("CLSE");
            }
            adbd.drain_until_eof().await;
        });

        // The timeout is a hang guard only: the connection generates a fresh
        // 2048-bit RSA key, which is CPU-bound and can be slow in a debug build
        // under a heavily loaded parallel test run.
        let conn = tokio::time::timeout(
            Duration::from_secs(60),
            PersistentConnection::from_stream(host, Some(nonexistent_key_path())),
        )
        .await
        .expect("handshake must not hang")
        .expect("CNXN over the duplex stream");

        let mut first = conn
            .open_session(&ADBLocalCommand::ShellCommand("first".into()))
            .await
            .expect("open first session");
        let mut second = conn
            .open_session(&ADBLocalCommand::ShellCommand("second".into()))
            .await
            .expect("open second session");

        let mut first_out = Vec::new();
        let mut second_out = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), first.read_to_end(&mut first_out))
            .await
            .expect("first read must not hang")
            .expect("read first session");
        tokio::time::timeout(Duration::from_secs(5), second.read_to_end(&mut second_out))
            .await
            .expect("second read must not hang")
            .expect("read second session");

        assert_eq!(
            first_out.as_slice(),
            b"shell:first",
            "session 1 must receive only its own stream"
        );
        assert_eq!(
            second_out.as_slice(),
            b"shell:second",
            "session 2 must receive only its own stream"
        );

        // Closing the connection lets the fake device observe EOF and finish.
        drop(first);
        drop(second);
        drop(conn);
        tokio::time::timeout(Duration::from_secs(5), device_task)
            .await
            .expect("device task must finish")
            .expect("device task must not panic");
    }

    /// §6.6: an oversize `data_length` is rejected before any payload wait, with a
    /// structured error (not a desync).
    #[tokio::test]
    async fn oversize_data_length_is_a_structured_error() {
        let (host, mut peer) = tokio::io::duplex(64 * 1024);
        let mut transport = StreamTransport::new(host, "test");
        let oversize = u32::try_from(MAX_PAYLOAD).expect("MAX_PAYLOAD fits u32") + 1;
        let command = MessageCommand::Write as u32;
        peer.write_all(&raw_header(command, oversize, !command))
            .await
            .expect("send oversize header");
        peer.flush().await.expect("flush");

        let result = transport
            .read_message_with_timeout(Duration::from_millis(200))
            .await;
        assert!(
            matches!(result, Err(RustADBError::ADBRequestFailed(ref msg)) if msg.contains("MAX_PAYLOAD")),
            "oversize data_length must be an ADBRequestFailed bound error (got {result:?})"
        );
    }

    /// §6.6: an unknown command word is a `ConversionError`, never a silent
    /// resynchronization.
    #[tokio::test]
    async fn unknown_command_is_a_conversion_error() {
        let (host, mut peer) = tokio::io::duplex(64 * 1024);
        let mut transport = StreamTransport::new(host, "test");
        peer.write_all(&raw_header(0xDEAD_BEEF, 0, !0xDEAD_BEEF))
            .await
            .expect("send unknown command header");
        peer.flush().await.expect("flush");

        let result = transport
            .read_message_with_timeout(Duration::from_millis(200))
            .await;
        assert!(
            matches!(result, Err(RustADBError::ConversionError)),
            "an unknown command must decode to ConversionError (got {result:?})"
        );
    }

    /// §6.6: a correct command with a corrupted magic is an integrity error.
    #[tokio::test]
    async fn bad_magic_is_an_integrity_error() {
        let (host, mut peer) = tokio::io::duplex(64 * 1024);
        let mut transport = StreamTransport::new(host, "test");
        let command = MessageCommand::Write as u32;
        peer.write_all(&raw_header(command, 0, 0))
            .await
            .expect("send bad-magic header");
        peer.flush().await.expect("flush");

        let result = transport
            .read_message_with_timeout(Duration::from_millis(200))
            .await;
        assert!(
            matches!(result, Err(RustADBError::InvalidIntegrity(..))),
            "a wrong magic must be an InvalidIntegrity error (got {result:?})"
        );
    }

    /// §6.7: a device-requested STLS upgrade is refused explicitly, naming the
    /// stream label (never silently continued in plaintext).
    #[cfg(feature = "usb")]
    #[tokio::test]
    async fn stls_upgrade_is_refused_with_the_stream_label() {
        use crate::message_devices::usb::persistent::PersistentConnection;

        let (host, device) = tokio::io::duplex(64 * 1024);
        let device_task = tokio::spawn(async move {
            let mut adbd = FakeAdbd::new(device);
            adbd.next_command(MessageCommand::Cnxn).await.expect("CNXN");
            adbd.write_frame(MessageCommand::Stls, 0, 0, &[])
                .await
                .expect("STLS");
            // Consume the host's STLS acknowledgement; the connection then fails.
            let _ = adbd.read_frame().await;
        });

        let transport = StreamTransport::new(host, "tbox@YTG");
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            PersistentConnection::new(transport, Some(nonexistent_key_path())),
        )
        .await
        .expect("handshake must not hang");
        match result {
            Ok(_) => panic!("an STLS request over a wrapped stream must not succeed"),
            Err(RustADBError::UpgradeError(message)) => assert!(
                message.contains("tbox@YTG"),
                "the upgrade error must name the stream label (got {message:?})"
            ),
            Err(other) => panic!("expected UpgradeError, got {other:?}"),
        }

        let _ = tokio::time::timeout(Duration::from_secs(2), device_task).await;
    }

    // -- mock streams (write semantics) ------------------------------------

    /// Never produces readable bytes; accepts `accept` bytes per write, or stalls
    /// while `accept` is zero.
    struct StallStream {
        accept: Arc<AtomicUsize>,
        written: usize,
    }

    impl AsyncRead for StallStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for StallStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let available = self.accept.load(Ordering::SeqCst);
            if available == 0 {
                return Poll::Pending;
            }
            let n = buf.len().min(available);
            self.accept.fetch_sub(n, Ordering::SeqCst);
            self.written += n;
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Accepts exactly eight bytes of the first write, then errors — modeling a
    /// peer that fails after a frame is already partially on the wire.
    struct FailingStream {
        written: usize,
    }

    impl AsyncRead for FailingStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for FailingStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.written >= 8 {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "mock mid-frame failure",
                )));
            }
            let n = buf.len().min(8 - self.written);
            self.written += n;
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}
