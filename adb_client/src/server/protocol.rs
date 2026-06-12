//! Smartsocket **host protocol** wire primitives (server side).
//!
//! These are the I/O-free encode/decode functions at the heart of an ADB
//! server's host protocol — the part every adb server shares regardless of how
//! it reaches devices. They are factored out as pure functions so the protocol's
//! many easy-to-get-wrong reply framings can be unit-tested without a socket or
//! a device (the [`tests`] module is the oracle).
//!
//! # The framing, in one place
//!
//! A request is `4 ASCII hex` (length) + the service string. A reply is one of:
//!
//! - **bare `OKAY`** — transport selection / local-service accept; the socket
//!   then stays open and (for local services) becomes a raw byte stream. The
//!   client must NOT read a length after it.
//! - **`OKAY` + `%04x`+payload** — a host *data* query (version/devices/…); the
//!   client reads the 4-hex length then that many bytes, then the socket closes.
//! - **`FAIL` + `%04x`+reason** — terminal error.
//! - **`OKAY``OKAY`** (two bare OKAYs) — the `forward` family success reply
//!   (AOSP `adb.cpp` host side: 1st = connect, 2nd = status). Writing only one
//!   desyncs modern clients.
//! - **`OKAY` + 8-byte LE id** — `host:tport:*` only. The client reads exactly 8
//!   bytes after the OKAY; the legacy `host:transport*` variants must NOT write
//!   them.
//!
//! The byte-emitting helpers return owned `Vec<u8>` rather than writing to a
//! socket, keeping them pure; the listener layer is responsible for the actual
//! `write_all`. See [`crate::server`] for the assembled frontend.

/// Parse the 4-byte ASCII-hex length prefix that opens every host request.
///
/// Returns `None` when the bytes are not valid ASCII hex (the caller should
/// then drop the connection rather than guess a length).
#[must_use]
pub fn parse_hex_len(buf: &[u8; 4]) -> Option<usize> {
    let s = std::str::from_utf8(buf).ok()?;
    usize::from_str_radix(s, 16).ok()
}

/// Encode `%04x`+payload — the frame shared by host data queries and `FAIL`
/// reasons. The 4-hex length counts the payload bytes only (not the 4 hex
/// digits themselves).
///
/// Payloads longer than `0xFFFF` cannot be represented by a 4-hex length; this
/// returns `None` so the caller fails loudly rather than emitting a truncated
/// length the client would misread.
#[must_use]
pub fn encode_framed(payload: &str) -> Option<Vec<u8>> {
    if payload.len() > 0xFFFF {
        return None;
    }
    let mut out = format!("{:04x}", payload.len()).into_bytes();
    out.extend_from_slice(payload.as_bytes());
    Some(out)
}

/// The single source of truth for transport-ids.
///
/// Computes a 1-based index for `serial` within the **sorted** serial set. The
/// same function must back `devices-l` output, `host:transport-id:N` selection,
/// and the `host:tport` 8-byte reply — otherwise the `transport_id` a client
/// reads from `adb -l` won't match what later selection targets.
///
/// Returns `None` if `serial` is not in `serials`.
#[must_use]
pub fn transport_id_for(serial: &str, serials: &[String]) -> Option<u64> {
    let mut sorted: Vec<&String> = serials.iter().collect();
    sorted.sort();
    sorted
        .iter()
        .position(|s| s.as_str() == serial)
        .map(|i| (i + 1) as u64)
}

/// Bytes for a host **data query** reply: `OKAY` + `%04x`+payload.
///
/// `None` when the payload exceeds the 4-hex length ceiling (see
/// [`encode_framed`]).
#[must_use]
pub fn okay_data(payload: &str) -> Option<Vec<u8>> {
    let mut out = b"OKAY".to_vec();
    out.extend_from_slice(&encode_framed(payload)?);
    Some(out)
}

/// Bytes for a terminal `FAIL` reply: `FAIL` + `%04x`+reason.
///
/// The reason is truncated to `0xFFFF` bytes (on a char boundary) rather than
/// failing — an error reply must always be sendable, even for an over-long
/// reason. `encode_framed` cannot fail on the truncated input.
#[must_use]
pub fn fail(reason: &str) -> Vec<u8> {
    let reason = truncate_on_char_boundary(reason, 0xFFFF);
    let mut out = b"FAIL".to_vec();
    // `reason` is now <= 0xFFFF bytes, so encode_framed always succeeds.
    out.extend_from_slice(&encode_framed(reason).unwrap_or_else(|| b"0000".to_vec()));
    out
}

/// Bytes for a bare `OKAY` — transport selection / local-service accept. The
/// socket stays open afterwards; no length, no payload follows.
#[must_use]
pub fn okay() -> Vec<u8> {
    b"OKAY".to_vec()
}

/// Bytes for the `forward` family success reply: **two** bare OKAYs.
///
/// Per AOSP Android-14 `adb.cpp::handle_forward_request`, the host side writes
/// `OKAY` (connect) then `OKAY` (status). Writing only one desyncs modern
/// clients. Failure is still a single [`fail`].
#[must_use]
pub fn okay_twice() -> Vec<u8> {
    b"OKAYOKAY".to_vec()
}

/// Bytes for the `host:tport:*` reply: `OKAY` + the transport-id as 8 bytes LE.
///
/// Modern clients read **exactly** 8 bytes after this OKAY. The legacy
/// `host:transport:*` / `transport-any` / `transport-id:*` variants use a bare
/// [`okay`] and must NOT carry these 8 bytes.
#[must_use]
pub fn okay_tport(id: u64) -> Vec<u8> {
    let mut out = b"OKAY".to_vec();
    out.extend_from_slice(&id.to_le_bytes());
    out
}

/// Truncate `s` to at most `max_bytes` bytes without splitting a UTF-8 char.
fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_len_decodes_lowercase_and_uppercase() {
        assert_eq!(parse_hex_len(b"000c"), Some(12));
        assert_eq!(parse_hex_len(b"001f"), Some(31));
        assert_eq!(parse_hex_len(b"FFFF"), Some(0xFFFF));
        assert_eq!(parse_hex_len(b"0000"), Some(0));
    }

    #[test]
    fn parse_hex_len_rejects_non_hex() {
        // A non-hex byte must yield None so the listener drops the connection
        // instead of waiting for a garbage-derived byte count.
        assert_eq!(parse_hex_len(b"zzzz"), None);
        assert_eq!(parse_hex_len(b"00 0"), None);
    }

    #[test]
    fn encode_framed_prefixes_length_in_4_hex() {
        assert_eq!(encode_framed("device").unwrap(), b"0006device");
        assert_eq!(encode_framed("").unwrap(), b"0000");
    }

    #[test]
    fn encode_framed_rejects_oversize_payload() {
        // A 4-hex length cannot represent > 0xFFFF; must fail loudly.
        let big = "x".repeat(0x1_0000);
        assert_eq!(encode_framed(&big), None);
    }

    #[test]
    fn okay_data_is_okay_plus_framed_payload() {
        assert_eq!(okay_data("0039").unwrap(), b"OKAY00040039");
    }

    #[test]
    fn fail_is_fail_plus_framed_reason() {
        assert_eq!(fail("no such device"), b"FAIL000eno such device");
    }

    #[test]
    fn fail_truncates_overlong_reason_rather_than_panicking() {
        let reason = "x".repeat(0x2_0000);
        let bytes = fail(&reason);
        // Reason is truncated to 0xFFFF; the length field reflects the truncation.
        assert_eq!(&bytes[..4], b"FAIL");
        assert_eq!(&bytes[4..8], b"ffff");
        assert_eq!(bytes.len(), 4 + 4 + 0xFFFF);
    }

    #[test]
    fn okay_is_bare_four_bytes() {
        assert_eq!(okay(), b"OKAY");
    }

    #[test]
    fn okay_twice_is_two_bare_okays() {
        // forward-family success: 1st OKAY = connect, 2nd = status.
        assert_eq!(okay_twice(), b"OKAYOKAY");
        assert_eq!(okay_twice().len(), 8);
    }

    #[test]
    fn okay_tport_is_okay_plus_8_byte_le_id() {
        let bytes = okay_tport(4);
        assert_eq!(&bytes[..4], b"OKAY");
        assert_eq!(&bytes[4..], &4u64.to_le_bytes());
        assert_eq!(bytes.len(), 12);
    }

    #[test]
    fn transport_id_is_1_based_over_sorted_serials() {
        // Deliberately unsorted input; ids follow lexicographic order, 1-based.
        let serials = vec![
            "emulator-5554".to_string(),
            "192.168.1.5".to_string(),
            "device1".to_string(),
        ];
        // sorted: "192.168.1.5"(1), "device1"(2), "emulator-5554"(3)
        assert_eq!(transport_id_for("192.168.1.5", &serials), Some(1));
        assert_eq!(transport_id_for("device1", &serials), Some(2));
        assert_eq!(transport_id_for("emulator-5554", &serials), Some(3));
    }

    #[test]
    fn transport_id_absent_serial_is_none() {
        let serials = vec!["a".to_string(), "b".to_string()];
        assert_eq!(transport_id_for("c", &serials), None);
    }

    #[test]
    fn transport_id_is_stable_regardless_of_input_order() {
        // The same set in a different order must yield the same ids — this is
        // what keeps devices-l / transport-id:N / tport consistent.
        let a = vec!["z".to_string(), "a".to_string(), "m".to_string()];
        let b = vec!["a".to_string(), "m".to_string(), "z".to_string()];
        for s in ["a", "m", "z"] {
            assert_eq!(transport_id_for(s, &a), transport_id_for(s, &b));
        }
    }
}
