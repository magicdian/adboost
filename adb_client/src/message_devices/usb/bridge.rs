//! Bidirectional byte bridge between a host TCP socket and a device session.
//!
//! Shared by every data path that splices a local TCP stream onto a device
//! [`MultiplexedSession`]: the `server` frontend's `forward` / local-service
//! bridges, and the reverse data path's host-dial side
//! ([`ReverseEngine`][crate::usb::ReverseEngine]). Exposed from `usb::` so
//! external "acts-as-a-server" backends can reuse the exact same half-close
//! semantics instead of re-deriving them.

use tokio::net::TcpStream;

use crate::usb::MultiplexedSession;

/// Bridge a host TCP socket to a device [`MultiplexedSession`] bidirectionally.
///
/// Both halves are `AsyncRead`/`AsyncWrite`. Each direction is copied
/// independently; when one direction reaches EOF, the *write* half of the other
/// peer is shut down (propagating the half-close as EOF) rather than aborting
/// the opposite copy. This is essential for request/response and
/// `echo … | nc`-style flows over `reverse`/`forward`: the peer may close its
/// send side after the request while still expecting the reply to flow back.
/// The bridge ends only once BOTH directions complete, so a late reply is not
/// truncated.
pub async fn bridge_tcp_session(host: TcpStream, session: MultiplexedSession) {
    use tokio::io::AsyncWriteExt as _;

    let local_id = session.local_id();
    let (mut usb_read, mut usb_write) = session.into_split();
    let (mut host_read, mut host_write) = host.into_split();

    // host → device, then signal EOF to the device by shutting its write half.
    let h2u = tokio::spawn(async move {
        let n = tokio::io::copy(&mut host_read, &mut usb_write).await;
        tracing::trace!("bridge h2u (host→device) ended local_id={local_id}: {n:?}");
        let _ = usb_write.shutdown().await;
    });
    // device → host, then signal EOF to the host.
    let u2h = tokio::spawn(async move {
        let n = tokio::io::copy(&mut usb_read, &mut host_write).await;
        tracing::trace!("bridge u2h (device→host) ended local_id={local_id}: {n:?}");
        let _ = host_write.shutdown().await;
    });

    // Wait for BOTH directions to finish so a late reply is not truncated.
    let _ = tokio::join!(h2u, u2h);
}
