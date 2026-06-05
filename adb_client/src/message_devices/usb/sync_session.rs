//! SYNC v1 file transfer multiplexed over a persistent USB connection.
//!
//! [`SyncSession`] wraps a [`MultiplexedSession`] opened with the `sync:`
//! service. Because the persistent connection multiplexes every stream over a
//! single authenticated USB connection (demuxed by the shared reader loop),
//! `adb push`/`adb pull` can run on the SAME connection as shell/tcp sessions
//! — there is no need to open a second, exclusive `ADBUSBDevice` (which would
//! double-claim the USB interface and conflict with the persistent
//! connection's exclusive claim).
//!
//! ## Layering
//!
//! The SYNC v1 sub-protocol frames (`id` + LE `length` + payload) ride inside
//! the WRTE/OKAY byte stream of the underlying [`MultiplexedSession`], which
//! stays byte-transparent: it knows nothing about SYNC. This is the same
//! layering used by `ShellV2Session` — both sit on top of an untouched
//! `MultiplexedSession`.
//!
//! Only SYNC **v1** is implemented (STAT/LIST/SEND/RECV/DATA/DONE/OKAY/FAIL).
//! SYNC v2 + compression (brotli/lz4/zstd) is intentionally out of scope.

use std::io::{Read, Write};

use crate::Result;
use crate::RustADBError;
use crate::message_devices::message_commands::MessageSubcommand;
use crate::message_devices::usb::persistent::MultiplexedSession;

/// Hard cap on a single SYNC `DATA` chunk payload (AOSP SYNC v1 limit).
const SYNC_DATA_MAX: usize = 65536;

/// Size of a SYNC frame header: a 4-byte ASCII opcode id + a 4-byte LE length.
const SYNC_HEADER_LEN: usize = 8;

/// SYNC success reply opcode (`"OKAY"`, little-endian). The shared
/// [`MessageSubcommand`] enum (used by the non-persistent SYNC path) only
/// defines the request opcodes, not this reply, so we mirror its wire value
/// here rather than mutate that enum. Identical to the connection-level
/// `A_OKAY` value.
const SYNC_OKAY: u32 = 0x5941_4B4F;

/// A SYNC v1 session multiplexed over a persistent USB connection.
///
/// Built by [`crate::message_devices::usb::PersistentUsbConnection::open_sync_session`].
/// It owns the underlying [`MultiplexedSession`] (one `local_id`, demuxed by the
/// shared reader loop like any other session) and speaks the SYNC v1 framing on
/// top of its byte stream.
pub struct SyncSession {
    inner: MultiplexedSession,
}

impl SyncSession {
    /// Wrap an already-opened `sync:` [`MultiplexedSession`].
    #[must_use]
    pub(crate) fn new(inner: MultiplexedSession) -> Self {
        Self { inner }
    }

    /// Push the contents of `reader` to `remote_path` on the device with the
    /// given unix `mode` (e.g. `0o644`), via the SYNC `SEND` request.
    ///
    /// Frames: `SEND <path>,<mode>`, then one or more `DATA <chunk>` (each
    /// `<= 65536` bytes), then `DONE <mtime>`; the device replies `OKAY` on
    /// success or `FAIL <reason>`.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::IOError`] on a transport error,
    /// [`RustADBError::ADBRequestFailed`] if the device replies `FAIL` or an
    /// unexpected opcode.
    pub fn push<R: Read>(&mut self, mut reader: R, remote_path: &str, mode: u32) -> Result<()> {
        let path_header = format!("{remote_path},{mode}");
        self.write_request(MessageSubcommand::Send, path_header.as_bytes())?;

        let mut buffer = vec![0u8; SYNC_DATA_MAX];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            self.write_request(MessageSubcommand::Data, &buffer[..read])?;
        }

        // DONE carries the file mtime in its length field. We do not forward an
        // mtime (mirrors the non-persistent `push_file`); send 0.
        self.write_frame_header(MessageSubcommand::Done, 0)?;
        self.inner.flush()?;

        // Expect OKAY (success) or FAIL (with reason payload).
        let (id, len) = self.read_frame_header()?;
        match SyncResponse::classify(id) {
            SyncResponse::Okay => Ok(()),
            SyncResponse::Fail => {
                let reason = self.read_exact_string(len)?;
                Err(RustADBError::ADBRequestFailed(format!(
                    "sync push failed: {reason}"
                )))
            }
            SyncResponse::Data | SyncResponse::Done | SyncResponse::Other => {
                Err(RustADBError::ADBRequestFailed(format!(
                    "sync push: unexpected response id {:#010x}",
                    u32::from_le_bytes(id)
                )))
            }
        }
    }

    /// Pull `remote_path` from the device into `writer`, via the SYNC `RECV`
    /// request.
    ///
    /// Sends `RECV <path>`, then reads `DATA <chunk>` frames until a `DONE`
    /// frame; a `FAIL <reason>` frame aborts with an error.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::IOError`] on a transport error or
    /// [`RustADBError::ADBRequestFailed`] if the device replies `FAIL` or an
    /// unexpected opcode.
    pub fn pull<W: Write>(&mut self, remote_path: &str, mut writer: W) -> Result<()> {
        self.write_request(MessageSubcommand::Recv, remote_path.as_bytes())?;
        self.inner.flush()?;

        let mut chunk = vec![0u8; SYNC_DATA_MAX];
        loop {
            let (id, len) = self.read_frame_header()?;
            match SyncResponse::classify(id) {
                SyncResponse::Done => return Ok(()),
                SyncResponse::Fail => {
                    let reason = self.read_exact_string(len)?;
                    return Err(RustADBError::ADBRequestFailed(format!(
                        "sync pull failed: {reason}"
                    )));
                }
                SyncResponse::Data => {
                    self.copy_payload(len, &mut chunk, &mut writer)?;
                }
                SyncResponse::Okay | SyncResponse::Other => {
                    return Err(RustADBError::ADBRequestFailed(format!(
                        "sync pull: unexpected response id {:#010x}",
                        u32::from_le_bytes(id)
                    )));
                }
            }
        }
    }

    /// Write a SYNC request frame: header (`id` + LE length) followed by the
    /// `payload` bytes (the length field is the payload length).
    fn write_request(&mut self, sub: MessageSubcommand, payload: &[u8]) -> Result<()> {
        self.write_frame_header(sub, u32::try_from(payload.len())?)?;
        if !payload.is_empty() {
            self.inner.write_all(payload)?;
        }
        log::trace!(
            "PersistentUsb: sync wrote frame id={sub:?} len={}",
            payload.len()
        );
        Ok(())
    }

    /// Write just an 8-byte SYNC frame header (`id` + LE `len`).
    fn write_frame_header(&mut self, sub: MessageSubcommand, len: u32) -> Result<()> {
        let header = encode_sync_header(sub, len);
        self.inner.write_all(&header)?;
        Ok(())
    }

    /// Read exactly an 8-byte SYNC frame header, returning the raw 4-byte id and
    /// the decoded LE length.
    fn read_frame_header(&mut self) -> Result<([u8; 4], u32)> {
        let mut header = [0u8; SYNC_HEADER_LEN];
        self.read_exact(&mut header)?;
        let id: [u8; 4] = header[0..4].try_into()?;
        let len = u32::from_le_bytes(header[4..8].try_into()?);
        Ok((id, len))
    }

    /// Copy `len` bytes of DATA payload from the inner stream into `writer`,
    /// reusing `scratch` as the transfer buffer.
    fn copy_payload<W: Write>(
        &mut self,
        len: u32,
        scratch: &mut [u8],
        writer: &mut W,
    ) -> Result<()> {
        let mut remaining = len as usize;
        while remaining > 0 {
            let want = remaining.min(scratch.len());
            self.read_exact(&mut scratch[..want])?;
            writer.write_all(&scratch[..want])?;
            remaining -= want;
        }
        Ok(())
    }

    /// Read exactly `len` payload bytes and decode them lossily as a UTF-8
    /// string (used for `FAIL` reason text).
    fn read_exact_string(&mut self, len: u32) -> Result<String> {
        let mut buf = vec![0u8; len as usize];
        self.read_exact(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Read exactly `buf.len()` bytes from the inner byte-transparent session.
    ///
    /// `MultiplexedSession::read` returns whatever a single WRTE delivered, so a
    /// SYNC frame may span several reads — loop until `buf` is full or EOF.
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.inner.read(&mut buf[filled..])?;
            if n == 0 {
                return Err(RustADBError::IOError(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "sync session closed before frame completed",
                )));
            }
            filled += n;
        }
        Ok(())
    }
}

/// Classification of a SYNC response opcode (so callers match on intent rather
/// than the raw 4-byte id). Pure / I/O-free for unit testing.
#[derive(Debug, PartialEq, Eq)]
enum SyncResponse {
    Okay,
    Fail,
    Data,
    Done,
    Other,
}

impl SyncResponse {
    fn classify(id: [u8; 4]) -> Self {
        match u32::from_le_bytes(id) {
            SYNC_OKAY => Self::Okay,
            x if x == MessageSubcommand::Fail as u32 => Self::Fail,
            x if x == MessageSubcommand::Data as u32 => Self::Data,
            x if x == MessageSubcommand::Done as u32 => Self::Done,
            _ => Self::Other,
        }
    }
}

/// Encode an 8-byte SYNC frame header: 4-byte ASCII opcode id + 4-byte LE
/// length. Pure / I/O-free so it can be unit-tested without hardware.
fn encode_sync_header(sub: MessageSubcommand, len: u32) -> [u8; SYNC_HEADER_LEN] {
    let mut header = [0u8; SYNC_HEADER_LEN];
    header[0..4].copy_from_slice(&(sub as u32).to_le_bytes());
    header[4..8].copy_from_slice(&len.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_header_encodes_id_and_le_length() {
        let header = encode_sync_header(MessageSubcommand::Send, 9);
        assert_eq!(
            &header[0..4],
            &(MessageSubcommand::Send as u32).to_le_bytes(),
            "first 4 bytes must be the SEND opcode in little-endian"
        );
        assert_eq!(
            &header[4..8],
            &9u32.to_le_bytes(),
            "last 4 bytes must be the payload length in little-endian"
        );
    }

    #[test]
    fn sync_header_data_length_roundtrips() {
        // DATA chunk at the hard 65536 cap must encode/decode cleanly.
        let cap = u32::try_from(SYNC_DATA_MAX).expect("65536 fits in u32");
        let header = encode_sync_header(MessageSubcommand::Data, cap);
        let len = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes"));
        assert_eq!(
            len, cap,
            "DATA length field must roundtrip at the 65536 boundary"
        );
    }

    #[test]
    fn classify_data_done_fail_okay() {
        assert_eq!(
            SyncResponse::classify((MessageSubcommand::Data as u32).to_le_bytes()),
            SyncResponse::Data,
            "DATA opcode must classify as Data"
        );
        assert_eq!(
            SyncResponse::classify((MessageSubcommand::Done as u32).to_le_bytes()),
            SyncResponse::Done,
            "DONE opcode must classify as Done"
        );
        assert_eq!(
            SyncResponse::classify((MessageSubcommand::Fail as u32).to_le_bytes()),
            SyncResponse::Fail,
            "FAIL opcode must classify as Fail"
        );
        assert_eq!(
            SyncResponse::classify(SYNC_OKAY.to_le_bytes()),
            SyncResponse::Okay,
            "OKAY opcode must classify as Okay"
        );
    }

    #[test]
    fn classify_unknown_is_other() {
        assert_eq!(
            SyncResponse::classify(*b"ZZZZ"),
            SyncResponse::Other,
            "an unknown 4-byte id must classify as Other"
        );
    }
}
