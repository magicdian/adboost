use rcgen::{CertificateParams, KeyPair, PKCS_RSA_SHA256};
use rustls::{
    ClientConfig, KeyLogFile, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, pem::PemObject},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::{
    Result, RustADBError,
    adb_transport::ADBTransport,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_transport_message::{
            ADBTransportMessage, ADBTransportMessageHeader, MAX_PAYLOAD, payload_len_within_bound,
        },
        message_commands::MessageCommand,
    },
};
use std::{
    fs::read_to_string,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// Either a plain TCP stream or a TLS stream layered on top of one.
///
/// Both arms implement [`tokio::io::AsyncRead`] / [`tokio::io::AsyncWrite`], so
/// the read/write helpers below are written once over `&mut CurrentConnection`.
/// Unlike the previous synchronous `rustls::StreamOwned`, the TLS upgrade does
/// not clone / swap the underlying socket: the async handshake consumes the
/// `TcpStream` and produces a `TlsStream<TcpStream>` in place (see
/// [`TcpTransport::upgrade_connection`]).
#[derive(Debug)]
enum CurrentConnection {
    Tcp(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl CurrentConnection {
    /// Read exactly `buf.len()` bytes, failing with an `io::ErrorKind::TimedOut`
    /// error if `timeout` elapses first.
    async fn read_exact_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> Result<()> {
        let fut = async {
            match self {
                Self::Tcp(s) => s.read_exact(buf).await,
                Self::Tls(s) => s.read_exact(buf).await,
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(res) => {
                res?;
                Ok(())
            }
            Err(_elapsed) => Err(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "TCP read timed out",
            ))),
        }
    }

    async fn write_all_timeout(&mut self, data: &[u8], timeout: Duration) -> Result<()> {
        let fut = async {
            match self {
                Self::Tcp(s) => {
                    s.write_all(data).await?;
                    s.flush().await
                }
                Self::Tls(s) => {
                    s.write_all(data).await?;
                    s.flush().await
                }
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(res) => {
                res?;
                Ok(())
            }
            Err(_elapsed) => Err(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "TCP write timed out",
            ))),
        }
    }
}

/// Transport running on USB
#[derive(Clone, Debug)]
pub struct TcpTransport {
    address: SocketAddr,
    /// The live connection. Wrapped in `Option<_>` *inside* the async mutex so
    /// the plain `TcpStream` can be moved out for the TLS handshake and the
    /// upgraded `TlsStream` put back, without needing to clone the socket.
    current_connection: Option<Arc<Mutex<Option<CurrentConnection>>>>,
    private_key_path: PathBuf,
}

fn certificate_from_pk(key_pair: &KeyPair) -> Result<Vec<CertificateDer<'static>>> {
    let certificate_params = CertificateParams::default();
    let certificate = certificate_params.self_signed(key_pair)?;
    Ok(vec![certificate.der().to_owned()])
}

impl TcpTransport {
    /// Instantiate a new [`TcpTransport`] using a given private key
    pub fn new<A: Into<SocketAddr>, P: AsRef<Path>>(address: A, private_key_path: P) -> Self {
        Self {
            address: address.into(),
            current_connection: None,
            private_key_path: private_key_path.as_ref().to_path_buf(),
        }
    }

    fn get_current_connection(&self) -> Result<Arc<Mutex<Option<CurrentConnection>>>> {
        self.current_connection
            .as_ref()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not connected",
            )))
            .cloned()
    }
}

/// Pull the live connection out of the guard, mapping the "no connection" case
/// to an `io::ErrorKind::NotConnected` error.
fn connection_mut(conn: &mut Option<CurrentConnection>) -> Result<&mut CurrentConnection> {
    conn.as_mut()
        .ok_or(RustADBError::IOError(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "not connected",
        )))
}

impl ADBTransport for TcpTransport {
    async fn connect(&mut self) -> Result<()> {
        let stream = TcpStream::connect(self.address).await?;
        self.current_connection = Some(Arc::new(Mutex::new(Some(CurrentConnection::Tcp(stream)))));
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        tracing::debug!("disconnecting...");
        if let Some(current_connection) = &self.current_connection {
            let mut lock = current_connection.lock().await;
            match lock.as_mut() {
                Some(CurrentConnection::Tcp(tcp_stream)) => {
                    let _ = tcp_stream.shutdown().await;
                }
                Some(CurrentConnection::Tls(tls_conn)) => {
                    let _ = tls_conn.shutdown().await;
                }
                None => {}
            }
        }

        Ok(())
    }
}

impl ADBMessageTransport for TcpTransport {
    async fn read_message_with_timeout(
        &mut self,
        read_timeout: Duration,
    ) -> Result<ADBTransportMessage> {
        let raw_connection_lock = self.get_current_connection()?;
        let mut guard = raw_connection_lock.lock().await;
        let raw_connection = connection_mut(&mut guard)?;

        let mut data = [0; 24];
        raw_connection
            .read_exact_timeout(&mut data, read_timeout)
            .await?;

        let header = ADBTransportMessageHeader::try_from(data)?;

        // Bound the wire data_length BEFORE allocating (AOSP check_header clause:
        // reject data_length > MAX_PAYLOAD before reading the payload). A hostile or
        // corrupt 24-byte header could otherwise drive a ~4 GiB allocation.
        if !payload_len_within_bound(header.data_length()) {
            return Err(RustADBError::ADBRequestFailed(format!(
                "frame data_length {} exceeds MAX_PAYLOAD {MAX_PAYLOAD}",
                header.data_length()
            )));
        }

        let payload = if header.data_length() != 0 {
            let mut msg_data = vec![0_u8; header.data_length() as usize];
            raw_connection
                .read_exact_timeout(&mut msg_data, read_timeout)
                .await?;
            msg_data
        } else {
            vec![]
        };
        // raw_connection is not used anymore, let's drop the guard
        drop(guard);

        let message = ADBTransportMessage::from_header_and_payload(header, payload);

        // Check message integrity (magic-only; runs for every frame, AOSP-faithful)
        if !message.check_message_integrity() {
            return Err(RustADBError::InvalidIntegrity(
                ADBTransportMessageHeader::compute_magic(message.header().command()),
                message.header().magic(),
            ));
        }

        Ok(message)
    }

    async fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        write_timeout: Duration,
    ) -> Result<()> {
        let message_bytes = message.header().as_bytes();
        let raw_connection_lock = self.get_current_connection()?;
        let mut guard = raw_connection_lock.lock().await;
        let raw_connection = connection_mut(&mut guard)?;

        raw_connection
            .write_all_timeout(&message_bytes, write_timeout)
            .await?;

        let payload = message.into_payload();
        if !payload.is_empty() {
            raw_connection
                .write_all_timeout(&payload, write_timeout)
                .await?;
        }

        Ok(())
    }

    async fn upgrade_connection(&mut self) -> Result<()> {
        let Some(current_connection) = self.current_connection.clone() else {
            return Err(RustADBError::UpgradeError(
                "cannot upgrade a non-existing connection...".into(),
            ));
        };

        {
            let mut guard = current_connection.lock().await;

            // Take ownership of the live connection so we can move the
            // `TcpStream` into the TLS handshake; we put the upgraded stream
            // back before unlocking.
            let tcp_stream = match guard.take() {
                Some(CurrentConnection::Tcp(tcp_stream)) => tcp_stream,
                Some(other @ CurrentConnection::Tls(_)) => {
                    // Put it back; cannot upgrade an already-TLS connection.
                    *guard = Some(other);
                    return Err(RustADBError::UpgradeError(
                        "cannot upgrade a TLS connection...".into(),
                    ));
                }
                None => {
                    return Err(RustADBError::UpgradeError(
                        "cannot upgrade a non-existing connection...".into(),
                    ));
                }
            };

            // TODO: Check if we cannot be more precise
            let pk_content = read_to_string(&self.private_key_path)?;
            let key_pair = KeyPair::from_pkcs8_pem_and_sign_algo(&pk_content, &PKCS_RSA_SHA256)?;
            let certificate = certificate_from_pk(&key_pair)?;
            let private_key = PrivatePkcs8KeyDer::from_pem_file(&self.private_key_path)?;

            let mut client_config = ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoCertificateVerification {}))
                .with_client_auth_cert(certificate, private_key.into())?;
            client_config.key_log = Arc::new(KeyLogFile::new());

            let connector = TlsConnector::from(Arc::new(client_config));
            let server_name = self.address.ip().into();

            // Async TLS handshake; consumes the TcpStream, no ownership swap.
            let tls_stream = connector.connect(server_name, tcp_stream).await?;
            *guard = Some(CurrentConnection::Tls(Box::new(tls_stream)));
        }

        let message = self.read_message().await?;
        match message.header().command() {
            MessageCommand::Cnxn => {
                let device_infos = String::from_utf8(message.into_payload())?;
                tracing::debug!("received device info: {device_infos}");
                Ok(())
            }
            c => Err(RustADBError::ADBRequestFailed(format!(
                "Wrong command received {c}"
            ))),
        }
    }
}

#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}
