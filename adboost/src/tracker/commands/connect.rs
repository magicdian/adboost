use std::net::SocketAddr;

use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::proxy::host_service::{self, FrameDecoder, HostStatus};
use crate::tracker::device_tracker::TrackerConnection;
use crate::tracker::models::{ServerFeatures, TrackerConfig, TrackerFormat, TrackerHealth};
use crate::tracker::{DeviceTracker, TrackerError};

/// The result of opening one tracker connection and its negotiated format.
pub(super) struct OpenOutcome {
    pub(super) connection: TrackerConnection,
    pub(super) format: TrackerFormat,
    pub(super) proto_downgraded: bool,
}

impl DeviceTracker {
    /// Connect to the adb server at `addr` and negotiate a tracker format.
    ///
    /// The default [`TrackerConfig`] enables bounded reconnect; use
    /// [`Self::connect_with_config`] to disable it or to request an explicit
    /// format.
    pub async fn connect(addr: SocketAddr) -> Result<Self, TrackerError> {
        Self::connect_with_config(TrackerConfig {
            addr,
            ..TrackerConfig::default()
        })
        .await
    }

    /// Connect to the default local adb server (`127.0.0.1:5037`).
    pub async fn connect_default() -> Result<Self, TrackerError> {
        Self::connect_with_config(TrackerConfig::default()).await
    }

    /// Connect and request `format` explicitly.
    ///
    /// `None` means auto-negotiate; `Some(format)` opens exactly that service
    /// and does not silently fall back.
    pub async fn connect_with_format(
        addr: SocketAddr,
        format: Option<TrackerFormat>,
    ) -> Result<Self, TrackerError> {
        Self::connect_with_config(TrackerConfig {
            addr,
            format,
            ..TrackerConfig::default()
        })
        .await
    }

    /// Connect with an explicit configuration.
    ///
    /// The initial connect is a single attempt; a failure is returned to the
    /// caller. The reconnect policy applies to failures after a successful
    /// initial connection.
    pub async fn connect_with_config(config: TrackerConfig) -> Result<Self, TrackerError> {
        let outcome = open_connection(&config, false).await?;
        Ok(Self::new(
            config,
            outcome.connection,
            outcome.format,
            outcome.proto_downgraded,
        ))
    }

    /// The negotiated (or explicitly requested) tracker format.
    #[must_use]
    pub const fn format(&self) -> TrackerFormat {
        self.format
    }

    /// A watch receiver for connection health transitions.
    #[must_use]
    pub fn health(&self) -> watch::Receiver<TrackerHealth> {
        self.health_tx.subscribe()
    }
}

/// Open one tracker connection, negotiating the format unless the config pins
/// it or the proto path was already downgraded for this tracker's lifetime.
pub(super) async fn open_connection(
    config: &TrackerConfig,
    proto_downgraded: bool,
) -> Result<OpenOutcome, TrackerError> {
    if let Some(format) = config.format {
        let connection = open_service(config.addr, format).await?;
        return Ok(OpenOutcome {
            connection,
            format,
            proto_downgraded,
        });
    }

    if proto_downgraded {
        let connection = open_service(config.addr, TrackerFormat::LongText).await?;
        return Ok(OpenOutcome {
            connection,
            format: TrackerFormat::LongText,
            proto_downgraded: true,
        });
    }

    let format = negotiate_format(config.addr).await;
    match open_service(config.addr, format).await {
        Ok(connection) => Ok(OpenOutcome {
            connection,
            format,
            proto_downgraded,
        }),
        Err(TrackerError::Unsupported { .. }) if format == TrackerFormat::ProtoBinary => {
            tracing::warn!(
                addr = %config.addr,
                "device tracker proto service failed despite devicetracker_proto_format; \
                 falling back to long text for this tracker"
            );
            let connection = open_service(config.addr, TrackerFormat::LongText).await?;
            Ok(OpenOutcome {
                connection,
                format: TrackerFormat::LongText,
                proto_downgraded: true,
            })
        }
        Err(error) => Err(error),
    }
}

async fn negotiate_format(addr: SocketAddr) -> TrackerFormat {
    match ServerFeatures::query(addr).await {
        Ok(features) if features.supports_device_tracker_proto() => TrackerFormat::ProtoBinary,
        Ok(_) => TrackerFormat::LongText,
        Err(error) => {
            tracing::debug!(
                addr = %addr,
                error = %error,
                "host:host-features query failed; using the long-text tracker"
            );
            TrackerFormat::LongText
        }
    }
}

async fn open_service(
    addr: SocketAddr,
    format: TrackerFormat,
) -> Result<TrackerConnection, TrackerError> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;

    let service = format.service();
    host_service::send_request(&mut stream, service).await?;
    match host_service::read_status(&mut stream).await? {
        HostStatus::Okay => Ok(TrackerConnection {
            stream,
            decoder: FrameDecoder::new(),
        }),
        HostStatus::Fail(detail) => Err(TrackerError::Unsupported { service, detail }),
    }
}
