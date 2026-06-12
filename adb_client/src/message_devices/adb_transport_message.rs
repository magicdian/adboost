use byteorder::{ByteOrder, LittleEndian};

use crate::{
    BinaryDecodable, Result, RustADBError,
    message_devices::{message_commands::MessageCommand, utils::BinaryEncodable},
};

pub const AUTH_TOKEN: u32 = 1;
pub const AUTH_SIGNATURE: u32 = 2;
pub const AUTH_RSAPUBLICKEY: u32 = 3;

/// Maximum bytes in a single ADB payload (AOSP `MAX_PAYLOAD`, 1 MiB).
///
/// This is a protocol constant, not a USB-specific value: AOSP
/// `transport.cpp::check_header` rejects any frame whose `data_length` exceeds
/// it BEFORE reading the payload. It lives here (always compiled) so both the
/// USB and the always-compiled TCP read paths can bound `data_length` against
/// it; the `usb/flow_control.rs` chunk clamp re-exports this single definition.
pub const MAX_PAYLOAD: usize = 1024 * 1024;

/// Whether a wire `data_length` is within the allowed payload bound.
///
/// Pure helper shared by both transport read paths (USB + TCP) to reject an
/// oversize/hostile `data_length` (an attacker- or corruption-controlled `u32`,
/// up to ~4 GiB) BEFORE allocating the payload buffer — mirroring AOSP
/// `check_header`'s `data_length <= MAX_PAYLOAD` clause.
#[must_use]
pub const fn payload_len_within_bound(data_length: u32) -> bool {
    data_length as usize <= MAX_PAYLOAD
}

#[derive(Debug, Clone)]
pub struct ADBTransportMessage {
    header: ADBTransportMessageHeader,
    payload: Vec<u8>,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct ADBTransportMessageHeader {
    command: MessageCommand, /* command identifier constant      */
    arg0: u32,               /* first argument                   */
    arg1: u32,               /* second argument                  */
    data_length: u32,        /* length of payload (0 is allowed) */
    data_crc32: u32,         /* crc32 of data payload            */
    magic: u32,              /* command ^ 0xffffffff             */
}

impl ADBTransportMessageHeader {
    pub fn try_new(command: MessageCommand, arg0: u32, arg1: u32, data: &[u8]) -> Result<Self> {
        Ok(Self {
            command,
            arg0,
            arg1,
            data_length: u32::try_from(data.len())?,
            data_crc32: Self::compute_crc32(data),
            magic: Self::compute_magic(command),
        })
    }

    #[must_use]
    pub const fn command(&self) -> MessageCommand {
        self.command
    }

    #[must_use]
    pub const fn arg0(&self) -> u32 {
        self.arg0
    }

    #[must_use]
    pub const fn arg1(&self) -> u32 {
        self.arg1
    }

    #[must_use]
    pub const fn data_length(&self) -> u32 {
        self.data_length
    }

    #[must_use]
    pub const fn data_crc32(&self) -> u32 {
        self.data_crc32
    }

    #[must_use]
    pub const fn magic(&self) -> u32 {
        self.magic
    }

    pub(crate) fn compute_crc32(data: &[u8]) -> u32 {
        data.iter().map(|&x| u32::from(x)).sum()
    }

    #[must_use]
    pub const fn compute_magic(command: MessageCommand) -> u32 {
        let command_u32 = command as u32;
        command_u32 ^ 0xFFFF_FFFF
    }

    #[must_use]
    pub fn as_bytes(&self) -> Vec<u8> {
        self.encode()
    }
}

impl BinaryEncodable for ADBTransportMessageHeader {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.command.encode());
        bytes.extend_from_slice(&self.arg0.to_le_bytes());
        bytes.extend_from_slice(&self.arg1.to_le_bytes());
        bytes.extend_from_slice(&self.data_length.to_le_bytes());
        bytes.extend_from_slice(&self.data_crc32.to_le_bytes());
        bytes.extend_from_slice(&self.magic.to_le_bytes());
        bytes
    }
}

impl BinaryDecodable for ADBTransportMessageHeader {
    fn decode(data: &[u8]) -> Result<Self>
    where
        Self: Sized,
    {
        if data.len() != std::mem::size_of::<Self>() {
            return Err(RustADBError::ConversionError);
        }

        Ok(Self {
            command: MessageCommand::try_from(LittleEndian::read_u32(&data[0..4]))
                .map_err(|_| RustADBError::ConversionError)?,
            arg0: LittleEndian::read_u32(&data[4..8]),
            arg1: LittleEndian::read_u32(&data[8..12]),
            data_length: LittleEndian::read_u32(&data[12..16]),
            data_crc32: LittleEndian::read_u32(&data[16..20]),
            magic: LittleEndian::read_u32(&data[20..24]),
        })
    }
}

impl ADBTransportMessage {
    pub fn try_new(command: MessageCommand, arg0: u32, arg1: u32, data: &[u8]) -> Result<Self> {
        Ok(Self {
            header: ADBTransportMessageHeader::try_new(command, arg0, arg1, data)?,
            payload: data.to_vec(),
        })
    }

    #[must_use]
    pub const fn from_header_and_payload(
        header: ADBTransportMessageHeader,
        payload: Vec<u8>,
    ) -> Self {
        Self { header, payload }
    }

    /// Validate the integrity of a received message.
    ///
    /// Only the `magic` field (`command ^ 0xffffffff`) is verified. AOSP `adb`
    /// never validates the apacket `data_check` (crc) field on receive in any
    /// protocol version — it is vestigial and is sent as `0` once the negotiated
    /// version is `>= A_VERSION_SKIP_CHECKSUM` (0x01000001), so comparing it would
    /// reject every payload-bearing frame from a skip-checksum peer. `magic` is the
    /// version-independent integrity field; the underlying USB (hardware CRC16) and
    /// TCP transports already guarantee payload integrity.
    #[must_use]
    pub fn check_message_integrity(&self) -> bool {
        ADBTransportMessageHeader::compute_magic(self.header.command) == self.header.magic
    }

    pub fn assert_command(&self, expected_command: MessageCommand) -> Result<()> {
        let our_command = self.header().command();
        if expected_command == our_command {
            return Ok(());
        }

        Err(RustADBError::WrongResponseReceived(
            our_command.to_string(),
            expected_command.to_string(),
        ))
    }

    #[must_use]
    pub const fn header(&self) -> &ADBTransportMessageHeader {
        &self.header
    }

    #[must_use]
    pub const fn payload(&self) -> &Vec<u8> {
        &self.payload
    }

    #[must_use]
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

impl TryFrom<[u8; 24]> for ADBTransportMessageHeader {
    type Error = RustADBError;

    fn try_from(value: [u8; 24]) -> Result<Self> {
        Self::decode(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ADBTransportMessage, ADBTransportMessageHeader, MAX_PAYLOAD, payload_len_within_bound,
    };
    use crate::message_devices::message_commands::MessageCommand;

    #[test]
    fn payload_len_within_bound_rejects_oversize() {
        assert!(
            payload_len_within_bound(0),
            "zero-length payload is within bound"
        );
        assert!(
            payload_len_within_bound(u32::try_from(MAX_PAYLOAD).expect("MAX_PAYLOAD fits u32")),
            "exactly MAX_PAYLOAD is within bound"
        );
        assert!(
            !payload_len_within_bound(
                u32::try_from(MAX_PAYLOAD).expect("MAX_PAYLOAD fits u32") + 1
            ),
            "MAX_PAYLOAD + 1 is rejected"
        );
        assert!(
            !payload_len_within_bound(u32::MAX),
            "a hostile 4 GiB data_length is rejected before any allocation"
        );
    }

    #[test]
    fn oversize_header_buffer_decodes_but_fails_bound_check() {
        // A 24-byte header with a 4 GiB data_length still decodes into a header
        // (the bound is enforced by the read path, not the decoder), but the
        // shared bound helper rejects it before the read path allocates.
        let header = ADBTransportMessageHeader::try_from(build_header_buffer(
            MessageCommand::Write,
            u32::MAX,
            0,
            ADBTransportMessageHeader::compute_magic(MessageCommand::Write),
        ))
        .expect("hand-built header decodes");
        assert!(
            !payload_len_within_bound(header.data_length()),
            "oversize data_length must be rejected before allocating the payload"
        );
    }

    /// Build a raw 24-byte header buffer with explicit `data_crc32` and `magic`
    /// fields, mirroring the on-wire layout decoded by `TryFrom<[u8; 24]>`. This
    /// is the cleanest sans-io way to forge headers (the struct fields are
    /// private), e.g. to emulate a skip-checksum peer that sends crc as 0.
    fn build_header_buffer(
        command: MessageCommand,
        data_length: u32,
        data_crc32: u32,
        magic: u32,
    ) -> [u8; 24] {
        let mut buf = [0u8; 24];
        buf[0..4].copy_from_slice(&(command as u32).to_le_bytes());
        // arg0 / arg1 left as 0
        buf[12..16].copy_from_slice(&data_length.to_le_bytes());
        buf[16..20].copy_from_slice(&data_crc32.to_le_bytes());
        buf[20..24].copy_from_slice(&magic.to_le_bytes());
        buf
    }

    #[test]
    fn integrity_passes_with_zero_crc_skip_checksum_peer() {
        // Exact bug #2 regression-lock: a skip-checksum peer sends a non-empty
        // payload with data_crc32 = 0 and correct magic. It must pass.
        let payload = b"host::features=cmd,shell_v2,delayed_ack".to_vec();
        let header = ADBTransportMessageHeader::try_from(build_header_buffer(
            MessageCommand::Cnxn,
            u32::try_from(payload.len()).expect("payload length fits in u32"),
            0,
            ADBTransportMessageHeader::compute_magic(MessageCommand::Cnxn),
        ))
        .expect("hand-built header decodes");
        let message = ADBTransportMessage::from_header_and_payload(header, payload);
        assert!(
            message.check_message_integrity(),
            "non-empty payload with crc=0 and correct magic must pass (skip-checksum peer)"
        );
    }

    #[test]
    fn integrity_passes_with_zero_payload() {
        // Zero-payload control frame (e.g. OKAY) with correct magic passes.
        let header = ADBTransportMessageHeader::try_from(build_header_buffer(
            MessageCommand::Okay,
            0,
            0,
            ADBTransportMessageHeader::compute_magic(MessageCommand::Okay),
        ))
        .expect("hand-built header decodes");
        let message = ADBTransportMessage::from_header_and_payload(header, vec![]);
        assert!(
            message.check_message_integrity(),
            "zero-payload frame with correct magic must pass"
        );
    }

    #[test]
    fn integrity_fails_on_magic_mismatch() {
        // A corrupted magic must still be rejected (the only integrity field left).
        let payload = b"data".to_vec();
        let header = ADBTransportMessageHeader::try_from(build_header_buffer(
            MessageCommand::Write,
            u32::try_from(payload.len()).expect("payload length fits in u32"),
            ADBTransportMessageHeader::compute_crc32(&payload),
            ADBTransportMessageHeader::compute_magic(MessageCommand::Write) ^ 0x1,
        ))
        .expect("hand-built header decodes");
        let message = ADBTransportMessage::from_header_and_payload(header, payload);
        assert!(
            !message.check_message_integrity(),
            "wrong magic must fail integrity check"
        );
    }

    #[test]
    fn integrity_ignores_wrong_crc() {
        // Correct magic but a data_crc32 that does NOT match the byte-sum: still
        // passes, proving crc is no longer consulted on receive.
        let payload = b"some payload bytes".to_vec();
        let bogus_crc = ADBTransportMessageHeader::compute_crc32(&payload).wrapping_add(12345);
        let header = ADBTransportMessageHeader::try_from(build_header_buffer(
            MessageCommand::Write,
            u32::try_from(payload.len()).expect("payload length fits in u32"),
            bogus_crc,
            ADBTransportMessageHeader::compute_magic(MessageCommand::Write),
        ))
        .expect("hand-built header decodes");
        let message = ADBTransportMessage::from_header_and_payload(header, payload);
        assert!(
            message.check_message_integrity(),
            "wrong crc with correct magic must pass (crc no longer consulted)"
        );
    }
}
