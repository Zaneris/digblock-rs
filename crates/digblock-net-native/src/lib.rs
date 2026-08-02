//! Native transport driver: wtransport (QUIC/WebTransport) on a
//! background thread, bridged to the sans-IO `digblock-net` crate
//! through non-blocking channels.

mod channel_transport;

pub use channel_transport::{connect, connect_with, CertPolicy, ChannelTransport};
