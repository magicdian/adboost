use crate::{
    Result,
    message_devices::{
        adb_message_device::ADBMessageDevice, adb_message_transport::ADBMessageTransport,
        message_commands::MessageCommand,
    },
    models::ADBLocalCommand,
};

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    /// Restart adbd in TCP/IP mode on `port`, returning the device's textual ack.
    ///
    /// The `tcpip:<port>` service writes a single status line (e.g.
    /// `restarting in TCP mode port: 5555`) as one WRTE, then closes. We collect
    /// the WRTE payload(s) until the device CLSEs and return the trimmed text.
    pub(crate) async fn tcpip(&mut self, port: u16) -> Result<String> {
        let mut session = self.open_session(&ADBLocalCommand::TcpIp(port)).await?;

        let mut ack = Vec::new();
        loop {
            let message = session.recv_and_reply_okay().await?;
            match message.header().command() {
                MessageCommand::Clse => break,
                MessageCommand::Write => ack.extend_from_slice(&message.into_payload()),
                // Any other command on a one-shot control service is unexpected;
                // ignore non-WRTE frames and keep waiting for CLSE.
                _ => {}
            }
        }

        Ok(decode_tcpip_ack(&ack))
    }

    /// Restart adbd in USB mode (`usb:`), undoing a previous `tcpip`. The service
    /// replies OKAY at session open; adbd then restarts.
    pub(crate) async fn usb(&mut self) -> Result<()> {
        // `open_session` already asserts the OKAY handshake; that is the device's
        // acknowledgement for the `usb:` control service.
        self.open_session(&ADBLocalCommand::Usb).await?;
        Ok(())
    }
}

/// Decode the `tcpip:` control service's status payload (concatenated WRTE
/// bytes) into a trimmed UTF-8 string. adbd writes a single human-readable line
/// such as `restarting in TCP mode port: 5555\n`; we strip the trailing newline
/// so callers get a clean message. Pure (sans-io) so it is unit-tested without a
/// device.
fn decode_tcpip_ack(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::decode_tcpip_ack;

    #[test]
    fn decodes_and_trims_trailing_newline() {
        assert_eq!(
            decode_tcpip_ack(b"restarting in TCP mode port: 5555\n"),
            "restarting in TCP mode port: 5555"
        );
    }

    #[test]
    fn handles_empty_and_whitespace_only_payload() {
        assert_eq!(decode_tcpip_ack(b""), "");
        assert_eq!(decode_tcpip_ack(b"  \r\n"), "");
    }

    #[test]
    fn lossy_decodes_non_utf8_without_panicking() {
        // A corrupted byte must not panic — it is replaced with U+FFFD.
        let decoded = decode_tcpip_ack(&[b'o', b'k', 0xff]);
        assert!(decoded.starts_with("ok"));
    }
}
