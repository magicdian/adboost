//! Shared shell-v2 inner-frame codec — the single source of truth for the
//! `[id:u8][len:u32 LE][payload]` framing used by *both* transports.
//!
//! AOSP's shell protocol (`shell,v2`) multiplexes stdin/stdout/stderr/exit and a
//! couple of control channels onto one byte stream as a sequence of frames, each
//! a 1-byte channel id + a 4-byte little-endian payload length + the payload.
//! Reference:
//! <https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/shell_protocol.h>
//!
//! ## Why this lives here (not under `usb`)
//!
//! There were historically **two** copies of this framing: one in the USB
//! [`ShellV2Session`](crate::message_devices::usb) path and one in the
//! [`crate::proxy`] path (which only knew the device→host ids). They drifted —
//! the proxy copy was missing the host→device control ids entirely. The proxy
//! module is always compiled, while `usb` is `#[cfg(feature = "usb")]`, so the
//! shared codec must sit in a non-usb-gated location both can import:
//! `message_devices` (always built). This is the de-duplication mandated by the
//! code-reuse guide — encode/decode now have exactly one implementation.

use crate::Result;
use crate::RustADBError;

/// Size of a shell-v2 frame header: a 1-byte channel id + a 4-byte LE length.
pub const HEADER_LEN: usize = 5;

/// A decoded shell-v2 inner-frame channel id.
///
/// Mirrors the AOSP `ShellProtocol::Id` values. The host→device-only ids
/// ([`Self::Stdin`], [`Self::CloseStdin`]) and the interactive
/// [`Self::WindowSizeChange`] are part of the enum so a decoder can classify and
/// consume-and-ignore them on the device→host stream rather than erroring, and
/// so the writable session can *encode* them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellChannel {
    /// `id=0`: stdin (host→device).
    Stdin,
    /// `id=1`: stdout payload (device→host).
    Stdout,
    /// `id=2`: stderr payload (device→host).
    Stderr,
    /// `id=3`: exit status (device→host; payload is exactly one byte).
    ExitStatus,
    /// `id=4`: close-stdin (host→device; signals EOF on the device's stdin).
    CloseStdin,
    /// `id=5`: window-size change (8-byte payload; host→device, interactive).
    WindowSizeChange,
}

impl ShellChannel {
    /// The on-wire `id` byte for this channel.
    #[must_use]
    pub const fn id(self) -> u8 {
        match self {
            Self::Stdin => 0,
            Self::Stdout => 1,
            Self::Stderr => 2,
            Self::ExitStatus => 3,
            Self::CloseStdin => 4,
            Self::WindowSizeChange => 5,
        }
    }
}

impl TryFrom<u8> for ShellChannel {
    type Error = RustADBError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Stdin),
            1 => Ok(Self::Stdout),
            2 => Ok(Self::Stderr),
            3 => Ok(Self::ExitStatus),
            4 => Ok(Self::CloseStdin),
            5 => Ok(Self::WindowSizeChange),
            other => Err(RustADBError::ADBShellV2ParseError(format!(
                "invalid shell-v2 channel id {other}"
            ))),
        }
    }
}

/// A single decoded shell-v2 frame header (channel + payload length).
#[derive(Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// Which logical channel the following payload belongs to.
    pub channel: ShellChannel,
    /// Length in bytes of the payload that follows the header.
    pub payload_len: usize,
}

/// Decode a [`HEADER_LEN`]-byte shell-v2 frame header: `[id:u8][len:u32 LE]`.
///
/// I/O-free so it can be unit-tested by feeding synthetic byte sequences; the
/// transports read the 5 header bytes off their stream and hand them here.
///
/// # Errors
///
/// Returns [`RustADBError::ADBShellV2ParseError`] if the id byte is not a valid
/// [`ShellChannel`].
pub fn decode_header(header: [u8; HEADER_LEN]) -> Result<FrameHeader> {
    let channel = ShellChannel::try_from(header[0])?;
    let payload_len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    Ok(FrameHeader {
        channel,
        payload_len,
    })
}

/// Encode a shell-v2 frame: `[id:u8][len:u32 LE][payload]`.
///
/// Used by the host→device direction (writing stdin / close-stdin / window-size)
/// and by tests/simulators that need to *produce* device→host frames.
///
/// # Panics
///
/// Panics only if `payload.len()` exceeds `u32::MAX`, which cannot happen for any
/// real shell frame (payloads are bounded by the transport's max packet size).
#[must_use]
pub fn encode(channel: ShellChannel, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).expect("shell-v2 payload fits in u32");
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.push(channel.id());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_id_round_trips_through_try_from() {
        for ch in [
            ShellChannel::Stdin,
            ShellChannel::Stdout,
            ShellChannel::Stderr,
            ShellChannel::ExitStatus,
            ShellChannel::CloseStdin,
            ShellChannel::WindowSizeChange,
        ] {
            assert_eq!(
                ShellChannel::try_from(ch.id()).expect("valid id"),
                ch,
                "id() and TryFrom must be inverse for {ch:?}"
            );
        }
    }

    #[test]
    fn decode_stdout_header() {
        // id=1 (stdout), len=5.
        assert_eq!(
            decode_header([1, 5, 0, 0, 0]).expect("valid header"),
            FrameHeader {
                channel: ShellChannel::Stdout,
                payload_len: 5,
            },
            "stdout header must decode channel=Stdout and the LE length"
        );
    }

    #[test]
    fn decode_stderr_header_little_endian() {
        // id=2 (stderr), len=0x0102 = 258.
        assert_eq!(
            decode_header([2, 0x02, 0x01, 0, 0]).expect("valid header"),
            FrameHeader {
                channel: ShellChannel::Stderr,
                payload_len: 258,
            },
            "stderr header must decode the little-endian length"
        );
    }

    #[test]
    fn decode_invalid_channel_is_parse_error() {
        let err = decode_header([9, 0, 0, 0, 0]).expect_err("channel id 9 is invalid");
        assert!(
            matches!(err, RustADBError::ADBShellV2ParseError(_)),
            "an invalid channel id must surface as ADBShellV2ParseError, got {err:?}"
        );
    }

    #[test]
    fn encode_prefixes_id_and_le_length() {
        // close-stdin (id=4) carries an empty payload in practice.
        assert_eq!(
            encode(ShellChannel::CloseStdin, &[]),
            vec![4, 0, 0, 0, 0],
            "close-stdin must encode as id=4 with a zero LE length and no payload"
        );
        // stdin (id=0) with a 3-byte payload.
        assert_eq!(
            encode(ShellChannel::Stdin, b"abc"),
            vec![0, 3, 0, 0, 0, b'a', b'b', b'c'],
            "stdin frame must be [id=0][len=3 LE][payload]"
        );
    }

    #[test]
    fn encode_then_decode_header_is_consistent() {
        let frame = encode(ShellChannel::Stdout, b"hello");
        let header: [u8; HEADER_LEN] = frame[..HEADER_LEN].try_into().expect("header slice");
        let decoded = decode_header(header).expect("decode encoded header");
        assert_eq!(
            decoded,
            FrameHeader {
                channel: ShellChannel::Stdout,
                payload_len: 5,
            },
            "encode/decode_header must agree on channel and length"
        );
        assert_eq!(
            &frame[HEADER_LEN..],
            b"hello",
            "the payload must follow the header verbatim"
        );
    }
}
