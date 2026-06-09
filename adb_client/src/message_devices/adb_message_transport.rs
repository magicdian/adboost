use std::future::Future;
use std::time::Duration;

use crate::{
    Result, adb_transport::ADBTransport,
    message_devices::adb_transport_message::ADBTransportMessage,
};

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(u64::MAX);
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Trait representing a transport able to read and write messages.
///
/// All I/O methods are `async`. [`trait_variant::make`] generates a `Send`
/// variant of every returned future, keeping the trait usable from a
/// multi-threaded tokio runtime. The `: ADBTransport + Clone + Send + 'static`
/// bounds are preserved — the persistent reader clones the transport and drives
/// it on a separate task.
///
/// Provided (default) methods are written as `fn .. -> impl Future + Send`
/// rather than `async fn { .. .await }`: `trait_variant` 0.1.2 cannot rewrite a
/// default `async fn` whose body contains `.await`, so the explicit-future form
/// is the supported recipe for default methods.
#[trait_variant::make(Send)]
pub trait ADBMessageTransport: ADBTransport + Clone + Send + 'static {
    /// An upgrade of the connection has been asked by the device.
    /// Some transports may not need this feature, a blanket implementation is provided as default implementation.
    fn upgrade_connection(&mut self) -> impl Future<Output = Result<()>> + Send {
        async {
            log::trace!("not upgrade needed fot this transport");
            Ok(())
        }
    }

    /// Read a message using given timeout on the underlying transport
    async fn read_message_with_timeout(
        &mut self,
        read_timeout: Duration,
    ) -> Result<ADBTransportMessage>;

    /// Read data to underlying connection, using default timeout
    fn read_message(&mut self) -> impl Future<Output = Result<ADBTransportMessage>> + Send {
        self.read_message_with_timeout(DEFAULT_READ_TIMEOUT)
    }

    /// Write a message using given timeout on the underlying transport
    async fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        write_timeout: Duration,
    ) -> Result<()>;

    /// Write data to underlying connection, using default timeout
    fn write_message(
        &mut self,
        message: ADBTransportMessage,
    ) -> impl Future<Output = Result<()>> + Send {
        self.write_message_with_timeout(message, DEFAULT_WRITE_TIMEOUT)
    }
}
