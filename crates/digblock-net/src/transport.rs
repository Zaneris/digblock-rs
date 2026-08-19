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
			/// The one method here allowed to take a moment, and it has to
			/// be: dropping a transport closes the connection immediately,
			/// so a goodbye written a microsecond earlier is still sitting
			/// in a stream buffer when the socket goes. On a process exit
			/// there is no later.
			///
			/// Bounded and best-effort in every implementation. Native waits
			/// briefly for the flush; the browser cannot wait at all on an
			/// unloading page, so it asks and moves on. Callers must treat
			/// this as a courtesy, never a guarantee.
			fn close(&mut self) {}

			/// The TRANSPORT's own smoothed round trip, in milliseconds —
			/// what a ping reports, and the same quantity the server
			/// browser's probe shows.
			///
			/// This exists because the client cannot derive it. The RTT the
			/// netcode measures itself is the input→ack loop, which carries
			/// the sim tick boundary, the snapshot cadence and the input
			/// buffer on top of the wire — and on a LAN that is not an
			/// offset but QUANTIZATION: every sample is one tick or two
			/// against a wire far shorter than a tick, so no constant
			/// subtracted from it can recover a number the sampling never
			/// contained.
			///
			/// `None` when the transport has no estimate.
			fn wire_rtt_ms(&self) -> Option<f32> {
				None
			}

			/// Can this transport carry datagrams at all?
			///
			/// Answered before the client sends its `Hello`, because the
			/// answer rides it: a `false` here makes the client announce
			/// `ClientMsg::NoDatagrams` and route its unreliable traffic
			/// over the reliable stream in both directions.
			///
			/// True for every transport but one, and that one is not a
			/// degraded network — it is a browser that ships
			/// `WebTransport.datagrams` with no writable side. There is
			/// nothing to retry and nothing that improves, so this is a
			/// property of the transport rather than a condition on it.
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
