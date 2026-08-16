//! Native transport driver: wtransport (QUIC/WebTransport) on a
//! background thread, bridged to the sans-IO `digblock-net` crate
//! through non-blocking channels.

mod channel_transport;
mod probe;

pub use channel_transport::{CertPolicy, ChannelTransport, connect, connect_with};
pub use probe::probe;
