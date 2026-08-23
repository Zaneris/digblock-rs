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
	/// Bytes from the reliable ordered stream (arbitrary chunking — feed a `FrameReader`).
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

			/// Flush what is queued and close down gracefully.
			///
			/// The one method allowed to take a moment, and it has to be: dropping a transport
			/// closes the connection at once, so a goodbye written a microsecond earlier is still
			/// in a stream buffer when the socket goes. Bounded and best-effort, never a guarantee.
			fn close(&mut self) {}

			/// The TRANSPORT's own smoothed round trip, in milliseconds — what a ping reports, and
			/// what the server browser's probe shows. `None` when the transport has no estimate.
			///
			/// The client cannot derive it: the RTT the netcode measures is the input→ack loop,
			/// which on a LAN is QUANTIZED to whole ticks, so no constant subtracted recovers it.
			fn wire_rtt_ms(&self) -> Option<f32> {
				None
			}

			/// Can this transport carry datagrams at all? Answered before the client sends its
			/// `Hello`, because the answer rides it: a `false` makes the client announce
			/// `ClientMsg::NoDatagrams` and route unreliable traffic over the reliable stream both
			/// ways. False only for a browser shipping `WebTransport.datagrams` with no writable
			/// side — a property of the transport, not a condition, so there is nothing to retry.
			fn datagrams_available(&self) -> bool {
				true
			}
		}
	};
}

#[cfg(not(target_arch = "wasm32"))]
transport_trait!(: Send);
#[cfg(target_arch = "wasm32")]
transport_trait!();
