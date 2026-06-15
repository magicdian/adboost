use rand::RngExt;
use std::{path::Path, time::Duration};

use crate::{
    Result, RustADBError,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_session::ADBSession,
        adb_transport_message::{
            ADBTransportMessage, AUTH_RSAPUBLICKEY, AUTH_SIGNATURE, AUTH_TOKEN,
        },
        message_commands::{MessageCommand, MessageSubcommand},
        models::{ADBRsaKey, read_adb_private_key},
        utils::BinaryEncodable,
    },
    models::ADBLocalCommand,
};

/// Generic structure representing an ADB device reachable over an [`ADBMessageTransport`].
/// Structure is totally agnostic over which transport is truly used.
#[derive(Debug)]
pub struct ADBMessageDevice<T: ADBMessageTransport> {
    transport: T,
}

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    /// Instantiate a new [`ADBMessageDevice`].
    ///
    /// The ADB handshake is network I/O, so the constructor is `async` (it
    /// drives `connect` to completion). Per the async rewrite there is no
    /// blocking constructor and no builder — callers `.await` this directly.
    pub async fn new<P: AsRef<Path>>(transport: T, adb_private_key_path: P) -> Result<Self> {
        let private_key = if let Some(private_key) = read_adb_private_key(&adb_private_key_path)? {
            private_key
        } else {
            tracing::warn!(
                "No private key found at path {}. Generating a new random.",
                adb_private_key_path.as_ref().display()
            );
            ADBRsaKey::new_random()?
        };

        let mut message_device = Self { transport };
        message_device.connect(&private_key).await?;

        Ok(message_device)
    }

    pub(crate) const fn get_transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Send initial connect
    async fn connect(&mut self, private_key: &ADBRsaKey) -> Result<()> {
        self.get_transport_mut().connect().await?;

        let message = ADBTransportMessage::try_new(
            MessageCommand::Cnxn,
            0x0100_0000,
            1_048_576,
            format!("host::{}\0", env!("CARGO_PKG_NAME")).as_bytes(),
        )?;

        self.get_transport_mut().write_message(message).await?;

        let message = self.get_transport_mut().read_message().await?;

        // Check if a client is requesting a secure connection and upgrade it if necessary
        match message.header().command() {
            MessageCommand::Stls => {
                self.get_transport_mut()
                    .write_message(ADBTransportMessage::try_new(
                        MessageCommand::Stls,
                        1,
                        0,
                        &[],
                    )?)
                    .await?;
                self.get_transport_mut().upgrade_connection().await?;
                tracing::debug!("Connection successfully upgraded from TCP to TLS");
                Ok(())
            }
            MessageCommand::Cnxn => {
                tracing::debug!("Unencrypted connection established");
                Ok(())
            }
            MessageCommand::Auth => {
                tracing::debug!("Authentication required");
                self.auth_handshake(message, private_key).await
            }
            _ => Err(crate::RustADBError::WrongResponseReceived(
                "Expected CNXN, STLS or AUTH command".to_string(),
                message.header().command().to_string(),
            )),
        }
    }

    async fn auth_handshake(
        &mut self,
        message: ADBTransportMessage,
        private_key: &ADBRsaKey,
    ) -> Result<()> {
        match message.header().command() {
            MessageCommand::Auth => {
                tracing::debug!("Authentication required");
            }
            _ => return Ok(()),
        }

        // At this point, we should have received an AUTH message with arg0 == 1
        let auth_message = match message.header().arg0() {
            AUTH_TOKEN => message,
            v => {
                return Err(RustADBError::ADBRequestFailed(format!(
                    "Received AUTH message with type != 1 ({v})"
                )));
            }
        };

        let sign = private_key.sign(auth_message.into_payload())?;

        let message = ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_SIGNATURE, 0, &sign)?;

        self.transport.write_message(message).await?;

        let received_response = self.transport.read_message().await?;

        if received_response.header().command() == MessageCommand::Cnxn {
            tracing::info!(
                "Authentication OK, device info {}",
                String::from_utf8(received_response.into_payload())?
            );
            return Ok(());
        }

        let mut pubkey = private_key.android_pubkey_encode()?.into_bytes();
        pubkey.push(b'\0');

        let message =
            ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_RSAPUBLICKEY, 0, &pubkey)?;

        self.transport.write_message(message).await?;

        let response = self
            .transport
            .read_message_with_timeout(Duration::from_secs(10))
            .await?;
        response.assert_command(MessageCommand::Cnxn)?;

        tracing::info!(
            "Authentication OK, device info {}",
            String::from_utf8(response.into_payload())?
        );
        Ok(())
    }

    pub(crate) async fn open_synchronization_session(&mut self) -> Result<ADBSession<T>> {
        self.open_session(&ADBLocalCommand::Sync).await
    }

    /// Open a new ADB session with the given local command.
    pub async fn open_session(&mut self, cmd: &ADBLocalCommand) -> Result<ADBSession<T>> {
        // Scope the (`!Send`) `ThreadRng` so it is dropped before any `.await`,
        // keeping the returned future `Send` for multi-threaded runtimes.
        let local_id: u32 = {
            let mut rng = rand::rng();
            rng.random()
        };

        let message = ADBTransportMessage::try_new(
            MessageCommand::Open,
            local_id, // Our 'local-id'
            0,
            cmd.to_string().as_bytes(),
        )?;
        self.transport.write_message(message).await?;

        let response = self.transport.read_message().await?;

        if response.header().command() != MessageCommand::Okay {
            return Err(RustADBError::ADBRequestFailed(format!(
                "Open session failed: got {} in response instead of OKAY",
                response.header().command()
            )));
        }

        if response.header().arg1() != local_id {
            return Err(RustADBError::ADBRequestFailed(format!(
                "Open session failed: responses used {} for our local_id instead of {local_id}",
                response.header().arg1()
            )));
        }

        Ok(ADBSession::new(
            self.transport.clone(),
            local_id,
            response.header().arg0(),
        ))
    }

    pub(crate) async fn end_transaction(&mut self, session: &mut ADBSession<T>) -> Result<()> {
        let quit_buffer = MessageSubcommand::Quit.with_arg(0u32);
        session
            .send_and_expect_okay(ADBTransportMessage::try_new(
                MessageCommand::Write,
                session.local_id(),
                session.remote_id(),
                &quit_buffer.encode(),
            )?)
            .await?;

        let _discard_close = self.transport.read_message().await?;
        Ok(())
    }
}

// NOTE (async teardown, P0-②): `disconnect` is now an async transport method
// and cannot be awaited in a stable-Rust `Drop`. Transport shutdown is handled
// structurally by the transport itself (the persistent USB multiplexer aborts
// its reader task and enqueues a fire-and-forget CLSE on drop; the TCP transport
// closes its socket). Consumers wanting a graceful, awaited teardown should call
// `disconnect` explicitly before dropping the device.
