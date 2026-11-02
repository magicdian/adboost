use std::net::SocketAddr;

use tokio::net::TcpStream;

use crate::proxy::host_service::{self, FrameDecoder, HostStatus};
use crate::tracker::TrackerError;
use crate::tracker::models::{DEFAULT_SERVER_ADDR, ServerFeatures};

const HOST_FEATURES_SERVICE: &str = "host:host-features";

impl ServerFeatures {
    /// Query the adb server's **server-level** feature set
    /// (`host:host-features`).
    ///
    /// This is not the per-transport `host:features` set exposed by
    /// [`crate::HostFeatures`]. It works with zero devices and is the query
    /// adblib uses to choose the tracker format.
    pub async fn query(addr: SocketAddr) -> Result<Self, TrackerError> {
        let mut stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;

        host_service::send_request(&mut stream, HOST_FEATURES_SERVICE).await?;
        match host_service::read_status(&mut stream).await? {
            HostStatus::Okay => {}
            HostStatus::Fail(detail) => {
                return Err(TrackerError::Unsupported {
                    service: HOST_FEATURES_SERVICE,
                    detail,
                });
            }
        }

        let mut decoder = FrameDecoder::new();
        let body =
            decoder
                .next_frame(&mut stream)
                .await?
                .ok_or_else(|| TrackerError::Protocol {
                    detail: "host:host-features stream closed before a frame".to_string(),
                })?;
        let csv = std::str::from_utf8(&body).map_err(|error| TrackerError::Protocol {
            detail: format!("host:host-features reply is not UTF-8: {error}"),
        })?;

        Ok(Self::from_csv(csv))
    }

    /// Query the default local adb server (`127.0.0.1:5037`).
    pub async fn query_default() -> Result<Self, TrackerError> {
        Self::query(DEFAULT_SERVER_ADDR).await
    }
}
