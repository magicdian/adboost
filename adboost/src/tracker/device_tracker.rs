use std::fmt;
use std::net::SocketAddr;

use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::proxy::host_service::FrameDecoder;
use crate::tracker::models::{TrackerConfig, TrackerFormat, TrackerHealth};

/// A long-lived, typed adb device tracker.
///
/// The tracker owns one connection to the adb server, negotiates
/// `host:track-devices-proto-binary` or `host:track-devices-l`, and returns one
/// [`crate::tracker::DeviceSnapshot`] per length-prefixed frame. It never
/// parses adb text in downstream code.
///
/// Use [`Self::next_snapshot`] repeatedly; each call is cancel-safe (dropping
/// the future mid-frame preserves the partial frame). [`Self::health`] reports
/// connection transitions for a poll fallback.
pub struct DeviceTracker {
    pub(super) config: TrackerConfig,
    pub(super) state: Option<TrackerConnection>,
    pub(super) format: TrackerFormat,
    pub(super) health_tx: watch::Sender<TrackerHealth>,
    pub(super) health_disconnected: bool,
    pub(super) proto_downgraded: bool,
    pub(super) reconnect_attempts: u32,
}

pub(super) struct TrackerConnection {
    pub(super) stream: TcpStream,
    pub(super) decoder: FrameDecoder,
}

impl DeviceTracker {
    pub(super) fn new(
        config: TrackerConfig,
        connection: TrackerConnection,
        format: TrackerFormat,
        proto_downgraded: bool,
    ) -> Self {
        let (health_tx, _) = watch::channel(TrackerHealth::Connected);
        Self {
            config,
            state: Some(connection),
            format,
            health_tx,
            health_disconnected: false,
            proto_downgraded,
            reconnect_attempts: 0,
        }
    }
}

impl fmt::Debug for DeviceTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceTracker")
            .field("addr", &self.config.addr)
            .field("format", &self.format)
            .field("connected", &self.state.is_some())
            .finish_non_exhaustive()
    }
}

impl DeviceTracker {
    /// The configured adb server address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.config.addr
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::DeviceTracker;
    use crate::tracker::TrackerError;
    use crate::tracker::models::{
        ConnectionState, ReconnectPolicy, ServerFeatures, TrackerConfig, TrackerFormat,
        TrackerHealth, TransportKind,
    };
    use crate::tracker::wire::test_support::{FixtureDevice, devices};

    fn fast_config(addr: SocketAddr) -> TrackerConfig {
        TrackerConfig {
            addr,
            format: None,
            reconnect: Some(ReconnectPolicy {
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(5),
                max_attempts: None,
            }),
        }
    }

    async fn within_timeout<T>(future: impl std::future::Future<Output = T>) -> T {
        timeout(Duration::from_secs(5), future)
            .await
            .expect("test timed out")
    }

    async fn accept(listener: &TcpListener) -> TcpStream {
        let (stream, _) = listener.accept().await.expect("accept connection");
        stream
    }

    async fn read_service(stream: &mut TcpStream) -> String {
        let mut length = [0u8; 4];
        stream
            .read_exact(&mut length)
            .await
            .expect("read service length");
        let text = std::str::from_utf8(&length).expect("service length is ASCII");
        let length = usize::from_str_radix(text, 16).expect("service length is hex");
        let mut service = vec![0u8; length];
        stream
            .read_exact(&mut service)
            .await
            .expect("read service name");
        String::from_utf8(service).expect("service name is UTF-8")
    }

    async fn write_okay(stream: &mut TcpStream) {
        stream.write_all(b"OKAY").await.expect("write OKAY");
        stream.flush().await.expect("flush OKAY");
    }

    async fn write_fail(stream: &mut TcpStream, reason: &str) {
        stream.write_all(b"FAIL").await.expect("write FAIL");
        write_frame(stream, reason.as_bytes()).await;
    }

    async fn write_frame(stream: &mut TcpStream, body: &[u8]) {
        stream
            .write_all(format!("{:04x}", body.len()).as_bytes())
            .await
            .expect("write frame length");
        stream.write_all(body).await.expect("write frame body");
        stream.flush().await.expect("flush frame");
    }

    async fn serve_features(stream: &mut TcpStream, csv: &str) {
        assert_eq!(
            read_service(stream).await,
            "host:host-features",
            "the tracker must negotiate with the server-level features query"
        );
        write_okay(stream).await;
        write_frame(stream, csv.as_bytes()).await;
    }

    #[tokio::test]
    async fn long_text_negotiates_and_streams_snapshots() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut features = accept(&listener).await;
            serve_features(&mut features, "cmd,shell_v2").await;
            drop(features);

            let mut tracker = accept(&listener).await;
            assert_eq!(read_service(&mut tracker).await, "host:track-devices-l");
            write_okay(&mut tracker).await;
            write_frame(
                &mut tracker,
                b"YTGUSCNFMFAIK7ZP       device usb:4-1.2 product:vext_f57aes model:f57aes device:f57aes transport_id:164\n",
            )
            .await;
            write_frame(&mut tracker, b"").await;
            drop(tracker);
        });

        let mut tracker = within_timeout(DeviceTracker::connect(addr))
            .await
            .expect("connect");
        assert_eq!(tracker.addr(), addr);
        assert_eq!(tracker.format(), TrackerFormat::LongText);

        let snapshot = within_timeout(tracker.next_snapshot())
            .await
            .expect("snapshot")
            .expect("some snapshot");
        assert_eq!(snapshot.malformed_rows, 0);
        assert_eq!(snapshot.devices.len(), 1);
        let device = &snapshot.devices[0];
        assert_eq!(device.serial.as_str(), "YTGUSCNFMFAIK7ZP");
        assert_eq!(device.state, ConnectionState::Device);
        assert_eq!(device.transport, TransportKind::Usb);
        assert_eq!(device.bus_address.as_deref(), Some("4-1.2"));
        assert_eq!(device.transport_id, Some(164));

        let empty = within_timeout(tracker.next_snapshot())
            .await
            .expect("snapshot")
            .expect("empty snapshot");
        assert!(empty.is_empty());

        let end = within_timeout(tracker.next_snapshot())
            .await
            .expect("clean EOF");
        assert!(end.is_none());

        server.await.unwrap();
    }

    #[tokio::test]
    async fn proto_negotiates_and_decodes_a_snapshot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let proto_body = devices(&[FixtureDevice {
            serial: "YTGUSCNFMFAIK7ZP",
            state: 8,
            bus_address: "usb:4-1.2",
            product: "vext_f57aes",
            model: "f57aes",
            device: "f57aes",
            connection_type: 1,
            negotiated_speed: 480,
            max_speed: 5000,
            transport_id: 187,
        }]);

        let server = tokio::spawn(async move {
            let mut features = accept(&listener).await;
            serve_features(&mut features, "cmd,devicetracker_proto_format").await;
            drop(features);

            let mut tracker = accept(&listener).await;
            assert_eq!(
                read_service(&mut tracker).await,
                "host:track-devices-proto-binary"
            );
            write_okay(&mut tracker).await;
            write_frame(&mut tracker, &proto_body).await;
            drop(tracker);
        });

        let mut tracker = within_timeout(DeviceTracker::connect(addr))
            .await
            .expect("connect");
        assert_eq!(tracker.format(), TrackerFormat::ProtoBinary);
        let snapshot = within_timeout(tracker.next_snapshot())
            .await
            .expect("snapshot")
            .expect("some snapshot");
        let device = &snapshot.devices[0];
        assert_eq!(device.serial.as_str(), "YTGUSCNFMFAIK7ZP");
        assert_eq!(device.state, ConnectionState::Device);
        assert_eq!(device.transport, TransportKind::Usb);
        assert_eq!(device.negotiated_speed, Some(480));
        assert_eq!(device.max_speed, Some(5000));
        assert_eq!(device.transport_id, Some(187));

        assert!(
            within_timeout(tracker.next_snapshot())
                .await
                .expect("clean EOF")
                .is_none()
        );

        server.await.unwrap();
    }

    #[tokio::test]
    async fn proto_failure_falls_back_to_long_text() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut features = accept(&listener).await;
            serve_features(&mut features, "devicetracker_proto_format").await;
            drop(features);

            let mut proto = accept(&listener).await;
            assert_eq!(
                read_service(&mut proto).await,
                "host:track-devices-proto-binary"
            );
            write_fail(&mut proto, "unknown host service").await;
            drop(proto);

            let mut long = accept(&listener).await;
            assert_eq!(read_service(&mut long).await, "host:track-devices-l");
            write_okay(&mut long).await;
            write_frame(&mut long, b"ABC123 device transport_id:7\n").await;
            drop(long);
        });

        let mut tracker = within_timeout(DeviceTracker::connect(addr))
            .await
            .expect("connect with fallback");
        assert_eq!(tracker.format(), TrackerFormat::LongText);
        let snapshot = within_timeout(tracker.next_snapshot())
            .await
            .expect("snapshot")
            .expect("some snapshot");
        assert_eq!(snapshot.devices[0].serial.as_str(), "ABC123");

        server.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_proto_format_does_not_fall_back() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut proto = accept(&listener).await;
            assert_eq!(
                read_service(&mut proto).await,
                "host:track-devices-proto-binary"
            );
            write_fail(&mut proto, "unknown host service").await;
            drop(proto);

            match timeout(Duration::from_millis(200), listener.accept()).await {
                Err(_) => {}
                Ok(Ok(_)) => panic!("explicit proto format must not fall back to long text"),
                Ok(Err(error)) => panic!("accept failed: {error}"),
            }
        });

        let error = within_timeout(DeviceTracker::connect_with_format(
            addr,
            Some(TrackerFormat::ProtoBinary),
        ))
        .await
        .expect_err("an explicit proto FAIL must surface");
        assert!(
            matches!(error, TrackerError::Unsupported { .. }),
            "unexpected error: {error:?}"
        );

        server.await.unwrap();
    }

    #[tokio::test]
    async fn server_features_query_uses_the_server_level_service() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = accept(&listener).await;
            serve_features(&mut stream, "cmd,devicetracker_proto_format").await;
            drop(stream);
        });

        let features = within_timeout(ServerFeatures::query(addr))
            .await
            .expect("features query");
        assert!(features.contains("cmd"));
        assert!(features.supports_device_tracker_proto());

        server.await.unwrap();
    }

    #[tokio::test]
    async fn clean_eof_returns_none_and_reconnects_on_the_next_call() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut features = accept(&listener).await;
            serve_features(&mut features, "cmd").await;
            drop(features);
            let mut first = accept(&listener).await;
            assert_eq!(read_service(&mut first).await, "host:track-devices-l");
            write_okay(&mut first).await;
            write_frame(&mut first, b"FIRST device transport_id:1\n").await;
            drop(first);

            let mut features = accept(&listener).await;
            serve_features(&mut features, "cmd").await;
            drop(features);
            let mut second = accept(&listener).await;
            assert_eq!(read_service(&mut second).await, "host:track-devices-l");
            write_okay(&mut second).await;
            write_frame(&mut second, b"SECOND device transport_id:2\n").await;
            drop(second);
        });

        let mut tracker = within_timeout(DeviceTracker::connect_with_config(fast_config(addr)))
            .await
            .expect("connect");
        let health = tracker.health();
        assert!(health.borrow().is_connected());

        let first = within_timeout(tracker.next_snapshot())
            .await
            .expect("snapshot")
            .expect("some snapshot");
        assert_eq!(first.devices[0].serial.as_str(), "FIRST");

        let end = within_timeout(tracker.next_snapshot())
            .await
            .expect("clean EOF");
        assert!(end.is_none());
        assert!(
            matches!(*health.borrow(), TrackerHealth::Disconnected { .. }),
            "a clean close must publish Disconnected"
        );

        let second = within_timeout(tracker.next_snapshot())
            .await
            .expect("snapshot after reconnect")
            .expect("some snapshot");
        assert_eq!(second.devices[0].serial.as_str(), "SECOND");
        assert!(health.borrow().is_connected());

        server.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_frame_reconnects_with_backoff() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut features = accept(&listener).await;
            serve_features(&mut features, "cmd").await;
            drop(features);
            let mut broken = accept(&listener).await;
            assert_eq!(read_service(&mut broken).await, "host:track-devices-l");
            write_okay(&mut broken).await;
            broken
                .write_all(b"0010ABC")
                .await
                .expect("write a truncated frame");
            broken.flush().await.expect("flush truncated frame");
            drop(broken);

            let mut features = accept(&listener).await;
            serve_features(&mut features, "cmd").await;
            drop(features);
            let mut healthy = accept(&listener).await;
            assert_eq!(read_service(&mut healthy).await, "host:track-devices-l");
            write_okay(&mut healthy).await;
            write_frame(&mut healthy, b"RECOVERED device transport_id:9\n").await;
            drop(healthy);
        });

        let mut tracker = within_timeout(DeviceTracker::connect_with_config(fast_config(addr)))
            .await
            .expect("connect");
        let snapshot = within_timeout(tracker.next_snapshot())
            .await
            .expect("snapshot after reconnect")
            .expect("some snapshot");
        assert_eq!(snapshot.devices[0].serial.as_str(), "RECOVERED");
        assert!(tracker.health().borrow().is_connected());

        server.await.unwrap();
    }

    #[tokio::test]
    async fn reconnect_disabled_returns_the_stream_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut features = accept(&listener).await;
            serve_features(&mut features, "cmd").await;
            drop(features);
            let mut broken = accept(&listener).await;
            assert_eq!(read_service(&mut broken).await, "host:track-devices-l");
            write_okay(&mut broken).await;
            broken
                .write_all(b"0010ABC")
                .await
                .expect("write a truncated frame");
            broken.flush().await.expect("flush truncated frame");
            drop(broken);
        });

        let config = TrackerConfig {
            addr,
            format: None,
            reconnect: None,
        };
        let mut tracker = within_timeout(DeviceTracker::connect_with_config(config))
            .await
            .expect("connect");
        let error = within_timeout(tracker.next_snapshot())
            .await
            .expect_err("a truncated frame must surface when reconnect is disabled");
        assert!(
            matches!(error, TrackerError::Protocol { .. }),
            "unexpected error: {error:?}"
        );
        assert!(matches!(
            *tracker.health().borrow(),
            TrackerHealth::Disconnected { .. }
        ));

        server.await.unwrap();
    }

    #[tokio::test]
    async fn disconnected_health_is_edge_triggered() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut features = accept(&listener).await;
            serve_features(&mut features, "cmd").await;
            drop(features);
            let mut tracker = accept(&listener).await;
            assert_eq!(read_service(&mut tracker).await, "host:track-devices-l");
            write_okay(&mut tracker).await;
            drop(tracker);
        });

        let mut tracker = within_timeout(DeviceTracker::connect(addr))
            .await
            .expect("connect");
        let mut health = tracker.health();
        assert!(health.borrow().is_connected());

        tracker.publish_disconnected("first");
        assert!(health.has_changed().expect("sender alive"));
        health.borrow_and_update();

        tracker.publish_disconnected("second");
        assert!(
            !health.has_changed().expect("sender alive"),
            "Disconnected must be published once per failure episode"
        );

        tracker.publish_connected();
        assert!(health.has_changed().expect("sender alive"));
        assert!(health.borrow_and_update().is_connected());

        server.await.unwrap();
    }

    #[tokio::test]
    async fn next_snapshot_is_cancel_safe_mid_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (first_half_sent_tx, first_half_sent_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut features = accept(&listener).await;
            serve_features(&mut features, "cmd").await;
            drop(features);
            let mut tracker = accept(&listener).await;
            assert_eq!(read_service(&mut tracker).await, "host:track-devices-l");
            write_okay(&mut tracker).await;

            let body = b"ABC123 device transport_id:7\n";
            tracker
                .write_all(format!("{:04x}", body.len()).as_bytes())
                .await
                .expect("write frame length");
            tracker
                .write_all(&body[..5])
                .await
                .expect("write first half");
            tracker.flush().await.expect("flush first half");
            first_half_sent_tx.send(()).expect("signal first half");
            release_rx.await.expect("wait for release");
            tracker
                .write_all(&body[5..])
                .await
                .expect("write second half");
            tracker.flush().await.expect("flush second half");
            drop(tracker);
        });

        let mut tracker = within_timeout(DeviceTracker::connect(addr))
            .await
            .expect("connect");
        first_half_sent_rx.await.expect("first half was sent");
        tokio::time::sleep(Duration::from_millis(20)).await;

        let cancelled = timeout(Duration::from_millis(100), tracker.next_snapshot()).await;
        assert!(
            cancelled.is_err(),
            "the first snapshot future should still be waiting for the rest of the frame"
        );

        release_tx.send(()).expect("release second half");
        let snapshot = within_timeout(tracker.next_snapshot())
            .await
            .expect("snapshot after cancellation")
            .expect("some snapshot");
        assert_eq!(snapshot.devices[0].serial.as_str(), "ABC123");

        server.await.unwrap();
    }
}
