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

			/// Flush what is queued and close down gracefully.
			///
			/// The one method here that is allowed to take a moment, and
			/// it has to be: dropping a transport closes the connection
			/// immediately, so a goodbye written a microsecond earlier is
			/// still sitting in a stream buffer when the socket goes. On a
			/// process exit there is no later — a detached network thread
			/// does not outlive `main`.
			///
			/// Bounded and best-effort in every implementation. Native
			/// waits briefly for the flush; the browser cannot wait at all
			/// on an unloading page, so it asks and moves on. Callers are
			/// on their way out and must treat this as a courtesy, never
			/// as a guarantee — the server still reaps a connection that
			/// simply vanishes, just slowly.
			fn close(&mut self) {}

			/// The TRANSPORT's own smoothed round trip, in milliseconds —
			/// what a ping reports, and the same quantity the server
			/// browser's probe shows.
			///
			/// This exists because the client cannot derive it. The RTT the
			/// netcode measures itself is the input→ack loop, which carries
			/// the sim tick boundary, the snapshot cadence and the input
			/// buffer on top of the wire — a whole pipeline's worth, and on
			/// a LAN that is not an offset but QUANTIZATION: every sample is
			/// either one tick or two, against a wire far shorter than a
			/// single tick, so no constant subtracted from it can recover a
			/// number the sampling never contained.
			///
			/// `None` when the transport has no estimate — loopback and the
			/// benchmark wires, which have no path to measure.
			fn wire_rtt_ms(&self) -> Option<f32> {
				None
			}
		}
	};
}

#[cfg(not(target_arch = "wasm32"))]
transport_trait!(: Send);
#[cfg(target_arch = "wasm32")]
transport_trait!();
