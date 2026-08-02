//! The transport seam. Native (wtransport) and test (loopback) impls
//! exist today; a wasm WebTransport shim implements the same trait later.
//! Every method is sync and non-blocking — exactly what a channel bridge
//! or a JS callback queue can provide.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnStatus {
	Connecting,
	Connected,
	Closed,
}

#[derive(Debug)]
pub enum TransportEvent {
	/// Connection established; reliable stream is open.
	Connected,
	/// One unreliable datagram.
	Datagram(Vec<u8>),
	/// Bytes from the reliable ordered stream (arbitrary chunking —
	/// feed a `FrameReader`).
	Reliable(Vec<u8>),
	Disconnected {
		reason: String,
	},
}

// `Send` is required off-wasm (the engine may hand the transport across
// threads) but must NOT be required on wasm32: the browser WebTransport
// handle a future shim wraps is a JS object and inherently !Send.
macro_rules! transport_trait {
	($($bound:tt)*) => {
		pub trait NetTransport $($bound)* {
			/// Fire-and-forget; silently dropped when not connected (datagrams
			/// are lossy by contract anyway).
			fn send_datagram(&mut self, bytes: &[u8]);
			/// Non-blocking enqueue onto the reliable ordered stream.
			fn send_reliable(&mut self, bytes: &[u8]);
			/// Drain everything received since the last call. Never blocks.
			fn poll(&mut self, out: &mut Vec<TransportEvent>);
			fn status(&self) -> ConnStatus;
		}
	};
}

#[cfg(not(target_arch = "wasm32"))]
transport_trait!(: Send);
#[cfg(target_arch = "wasm32")]
transport_trait!();
