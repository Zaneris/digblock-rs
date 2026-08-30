//! What the server browser's probe reports, shared by the native and browser probes so a row
//! reads the same on both. A probe opens a throwaway session and reads the transport's own
//! smoothed round-trip estimate; when the transport offers none (Chrome has no
//! `WebTransport.getStats`), the only figure left is how long the session took to open, and
//! that is not a latency but a fixed count of them — so it is reported divided down, and
//! FLAGGED, rather than as a number three times too large.

/// One latency for a server row, and whether it was measured or derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ping {
	pub ms: u32,
	/// Derived from the session's opening time rather than sampled from
	/// acks. The row marks it, so a player can tell a reading from a guess.
	pub estimated: bool,
}

/// Round trips it takes to open a WebTransport session against our server: address validation
/// (the server's Retry), the TLS handshake, and the HTTP/3 extended CONNECT. Measured, not
/// derived (2026-08-25): the opening time is this many times the round trip plus a
/// couple of milliseconds, at every latency and whether or not the browser had a session to
/// resume. If the server ever stops issuing Retry this drops by one — re-measure, don't reason.
pub const SESSION_ROUND_TRIPS: u32 = 3;

impl Ping {
	pub fn measured(ms: u32) -> Self {
		Self {
			ms,
			estimated: false,
		}
	}

	/// The estimate for a session that took `handshake_ms` to open.
	pub fn from_handshake(handshake_ms: u32) -> Self {
		Self {
			ms: handshake_ms / SESSION_ROUND_TRIPS,
			estimated: true,
		}
	}
}
