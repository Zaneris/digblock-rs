//! Browser WebTransport driver for digblock-net — the wasm analogue of
//! digblock-net-native. The bindings are hand-written (web-sys's
//! WebTransport API still sits behind the global `web_sys_unstable_apis`
//! cfg, which would poison every wasm build command for ~80 lines of
//! glue).

mod hex;
pub use hex::parse_sha256_hex;

#[cfg(target_arch = "wasm32")]
mod transport;
#[cfg(target_arch = "wasm32")]
pub use transport::{connect, probe, WebTransportClient};
