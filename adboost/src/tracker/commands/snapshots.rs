use crate::tracker::commands::connect::open_connection;
use crate::tracker::models::{DeviceSnapshot, TrackerHealth};
use crate::tracker::wire;
use crate::tracker::{DeviceTracker, TrackerError};

impl DeviceTracker {
    /// Wait for the next complete snapshot.
    ///
    /// One length-prefixed frame is one snapshot. `Ok(None)` means the server
    /// closed the stream cleanly at a frame boundary; calling this method again
    /// reconnects (subject to the configured policy).
    ///
    /// The future is cancel-safe: dropping it mid-frame leaves the partial
    /// frame in the tracker, so the next call resumes at the same byte.
    pub async fn next_snapshot(&mut self) -> Result<Option<DeviceSnapshot>, TrackerError> {
        loop {
            if self.state.is_none() {
                self.ensure_connected().await?;
            }

            let frame = {
                let Some(connection) = self.state.as_mut() else {
                    continue;
                };
                connection.decoder.next_frame(&mut connection.stream).await
            };

            match frame {
                Ok(Some(body)) => match wire::decode(self.format, &body) {
                    Ok(snapshot) => {
                        self.reconnect_attempts = 0;
                        self.publish_connected();
                        return Ok(Some(snapshot));
                    }
                    // A payload decode failure leaves the frame boundary intact,
                    // so the connection remains usable.
                    Err(error) => return Err(error),
                },
                Ok(None) => {
                    self.state = None;
                    self.reconnect_attempts = 0;
                    self.publish_disconnected("tracker stream closed cleanly");
                    return Ok(None);
                }
                Err(error) => {
                    self.state = None;
                    self.publish_disconnected(error.to_string());

                    let Some(policy) = self.config.reconnect.clone() else {
                        return Err(TrackerError::from(error));
                    };
                    if !policy.allows_attempt(self.reconnect_attempts) {
                        self.reconnect_attempts = 0;
                        return Err(TrackerError::from(error));
                    }

                    let delay = policy.backoff(self.reconnect_attempts);
                    self.reconnect_attempts = self.reconnect_attempts.saturating_add(1);
                    tracing::debug!(
                        addr = %self.config.addr,
                        attempt = self.reconnect_attempts,
                        ?delay,
                        error = %error,
                        "tracker connection failed; reconnecting after backoff"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    async fn ensure_connected(&mut self) -> Result<(), TrackerError> {
        loop {
            match open_connection(&self.config, self.proto_downgraded).await {
                Ok(outcome) => {
                    self.state = Some(outcome.connection);
                    self.format = outcome.format;
                    self.proto_downgraded = outcome.proto_downgraded;
                    self.publish_connected();
                    return Ok(());
                }
                Err(error) => {
                    let Some(policy) = self.config.reconnect.clone() else {
                        return Err(error);
                    };
                    if !policy.allows_attempt(self.reconnect_attempts) {
                        self.reconnect_attempts = 0;
                        return Err(error);
                    }

                    let delay = policy.backoff(self.reconnect_attempts);
                    self.reconnect_attempts = self.reconnect_attempts.saturating_add(1);
                    tracing::debug!(
                        addr = %self.config.addr,
                        attempt = self.reconnect_attempts,
                        ?delay,
                        error = %error,
                        "tracker reconnect failed; backing off"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    pub(crate) fn publish_connected(&mut self) {
        if self.health_disconnected {
            self.health_disconnected = false;
            let _ = self.health_tx.send_replace(TrackerHealth::Connected);
        }
    }

    pub(crate) fn publish_disconnected(&mut self, reason: impl Into<String>) {
        if !self.health_disconnected {
            self.health_disconnected = true;
            let _ = self.health_tx.send_replace(TrackerHealth::Disconnected {
                reason: reason.into(),
            });
        }
    }
}
