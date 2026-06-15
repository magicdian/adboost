use crate::Result;

/// Trait representing a transport usable by ADB protocol.
///
/// All I/O methods are `async`. [`trait_variant::make`] generates a `Send`
/// variant of every returned future so transports can be driven from a
/// multi-threaded tokio runtime. The crate does not own a runtime — the
/// consumer drives these futures on its own executor.
#[trait_variant::make(Send)]
pub trait ADBTransport {
    /// Initializes the connection to this transport.
    async fn connect(&mut self) -> Result<()>;

    /// Shuts down the connection to this transport.
    async fn disconnect(&mut self) -> Result<()>;
}
