//! One-shot reachability/latency probe: open a bare WebTransport session, read
//! the connection's own round-trip estimate, close immediately. Runs on its own
//! short-lived thread; the result arrives via `done`.
//!
//! The reported number is QUIC's smoothed RTT, NOT the time the connect took.
//! Opening a session is `probe::SESSION_ROUND_TRIPS` round trips plus DNS,
//! which is not a round trip at all — so timing it ranked servers well enough
//! and listed every one at a multiple of its latency. `Connection::rtt` is the
//! transport's own smoothed estimate, sampled from the acks the handshake
//! already exchanged; the opening time is kept only as the flagged fallback
//! `Ping::from_handshake` makes of it.

use crate::CertPolicy;
use digblock_net::probe::Ping;
use std::time::{Duration, Instant};
use wtransport::Endpoint;

pub fn probe(
	url: &str,
	policy: CertPolicy,
	timeout: Duration,
	done: impl FnOnce(Option<Ping>) + Send + 'static,
) {
	let url = url.to_string();
	let spawned = std::thread::Builder::new()
		.name("net-probe".into())
		.spawn(move || {
			let rt = tokio::runtime::Builder::new_current_thread()
				.enable_all()
				.build()
				.expect("tokio runtime");
			let ms = rt.block_on(async {
				let endpoint = Endpoint::client(policy.client_config()).ok()?;
				let start = Instant::now();
				let conn = tokio::time::timeout(timeout, endpoint.connect(&url))
					.await
					.ok()?
					.ok()?;
				// Handshake time is still the reachability answer, just not the latency one. It
				// stands in, divided down, when the estimate has no sample of its own yet and
				// quinn still reports its RFC 9002 initial default, which would read as one
				// fixed figure against every server listed.
				let handshake = start.elapsed();
				let rtt = conn.rtt();
				let ms = |d: Duration| d.as_millis().min(u128::from(u32::MAX)) as u32;
				let ping = if rtt.is_zero() || rtt > handshake {
					Ping::from_handshake(ms(handshake))
				} else {
					Ping::measured(ms(rtt))
				};
				// Close politely (well inside the server's preamble
				// deadline); the measurement is already taken.
				conn.close(wtransport::VarInt::from_u32(0), b"probe");
				Some(ping)
			});
			done(ms);
		});
	if spawned.is_err() {
		log::warn!("failed to spawn net-probe thread");
	}
}
