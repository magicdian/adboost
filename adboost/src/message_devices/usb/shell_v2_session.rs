//! USB binding of the transport-generic shell-v2 session.
//!
//! The framing/streaming/writable logic lives in
//! [`crate::message_devices::shell_v2_session`] and is shared with the proxy
//! path. Here we only pin the generic session to the USB transport's split
//! halves ([`SessionReadHalf`] / [`SessionWriteHalf`] from
//! [`MultiplexedSession::into_split`]) and provide the constructor
//! [`PersistentUsbConnection::open_shell_v2`] uses.
//!
//! Dropping a [`ShellV2Session`] drops both halves; the shared
//! [`SessionInner`](crate::message_devices::usb) then fires a best-effort CLSE,
//! so cancelling a session (host-side close) tears down the underlying ADB
//! stream — the USB analogue of the proxy's drop-closes-the-socket.
//!
//! [`SessionReadHalf`]: crate::message_devices::usb::SessionReadHalf
//! [`SessionWriteHalf`]: crate::message_devices::usb::SessionWriteHalf
//! [`MultiplexedSession::into_split`]: crate::message_devices::usb::MultiplexedSession::into_split

use crate::message_devices::shell_v2_session::ShellV2Session as GenericShellV2Session;
use crate::message_devices::usb::persistent::{
    MultiplexedSession, SessionReadHalf, SessionWriteHalf,
};

/// A shell-v2 session multiplexed over a persistent USB connection.
///
/// Type alias for the transport-generic
/// [`ShellV2Session`](crate::message_devices::shell_v2_session::ShellV2Session)
/// pinned to the USB split halves. All behavior (streaming `read_frame`,
/// `write_stdin` / `close_stdin`, back-compat `execute`) comes from the generic
/// type.
pub type ShellV2Session = GenericShellV2Session<SessionReadHalf, SessionWriteHalf>;

/// Build a [`ShellV2Session`] from an already-opened `shell,v2`
/// [`MultiplexedSession`] by splitting it into read/write halves.
#[must_use]
pub(crate) fn from_multiplexed(session: MultiplexedSession) -> ShellV2Session {
    let (reader, writer) = session.into_split();
    GenericShellV2Session::new(reader, writer)
}
