#![doc = include_str!("./README.md")]

mod stream_transport;

pub use stream_transport::{AsyncStream, StreamConnection, StreamTransport};
