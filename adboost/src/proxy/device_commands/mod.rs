mod forward;
mod host_features;
mod install;
mod list;
mod logcat;
mod reboot;
mod reconnect;
mod recv;
mod remount;
mod reverse;
mod root;
mod send;
mod stat;
mod tcpip;
mod transport;
mod uninstall;
mod usb;
mod verity;

#[cfg(feature = "framebuffer")]
mod framebuffer;

use crate::{Result, RustADBError, message_devices::adb_transport_message::MAX_PAYLOAD};

/// Validate a wire-supplied length before using it to size an allocation.
///
/// The sync proxy commands (LIST/DENT name length, RECV FAIL reason length) read
/// a raw little-endian `u32` from the relayed device and use it directly as a
/// `Vec` length. A hostile or corrupt response can request up to ~4 GiB per entry,
/// forcing the proxy to allocate that much before the following read fails. Mirror
/// the message-transport discipline (`payload_len_within_bound`) and reject any
/// length beyond [`MAX_PAYLOAD`] — far larger than any real filename or error
/// string — before allocating.
fn checked_wire_len(len: u32) -> Result<usize> {
    let len = len as usize;
    if len > MAX_PAYLOAD {
        return Err(RustADBError::ADBRequestFailed(format!(
            "wire length {len} exceeds MAX_PAYLOAD {MAX_PAYLOAD}; refusing to allocate"
        )));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::checked_wire_len;
    use crate::{RustADBError, message_devices::adb_transport_message::MAX_PAYLOAD};

    #[test]
    fn accepts_reasonable_length() {
        assert_eq!(
            checked_wire_len(256).expect("a small length is accepted"),
            256
        );
        let max = u32::try_from(MAX_PAYLOAD).expect("MAX_PAYLOAD fits u32");
        assert_eq!(
            checked_wire_len(max).expect("exactly MAX_PAYLOAD is accepted"),
            MAX_PAYLOAD
        );
    }

    #[test]
    fn rejects_oversize_length() {
        assert!(
            matches!(
                checked_wire_len(u32::MAX),
                Err(RustADBError::ADBRequestFailed(_))
            ),
            "a ~4 GiB wire length must be refused before allocating"
        );
    }
}
