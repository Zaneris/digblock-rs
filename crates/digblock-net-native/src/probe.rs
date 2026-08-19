//! One-shot reachability/latency probe: open a bare WebTransport session, read
//! the connection's own round-trip estimate, close immediately. Runs on its own
//! short-lived thread; the result arrives via `done`.
//!
//! The reported number is QUIC's smoothed RTT, NOT the time the connect took.
//! Timing the connect was consistent enough to rank servers by, but it was not a
//! ping and read about double one: establishing a WebTransport session is a
//! QUIC+TLS 1.3 handshake and then an HTTP/3 extended CONNECT over it — two
//! round trips at best — plus DNS resolution, which is not a round trip at all
//! and does not scale with distance. So a nearby server was listed at a multiple
//! of its real latency, and the figure could not be compared with the in-game
//! overlay's.
//!
//! `Connection::rtt` is the transport's own smoothed estimate, sampled from the
//! acks the handshake already exchanged.

use crate::CertPolicy;
use std::time::{Duration, Instant};
use wtransport::{ClientConfig, Endpoint};

pub fn probe(
	url: &str,
	policy: CertPolicy,
	timeout: Duration,
	done: impl FnOnce(Option<u32>) + Send + 'static,
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
				let config = match policy {
					CertPolicy::Insecure => ClientConfig::builder()
						.with_bind_default()
						.with_no_cert_validation()
						.build(),
					CertPolicy::PinnedSha256(hash) => ClientConfig::builder()
						.with_bind_default()
						.with_server_certificate_hashes([wtransport::tls::Sha256Digest::new(hash)])
						.build(),
				};
				let endpoint = Endpoint::client(config).ok()?;
				let start = Instant::now();
				let conn = tokio::time::timeout(timeout, endpoint.connect(&url))
					.await
					.ok()?
					.ok()?;
				// Handshake time is still the reachability answer; it is
				// just not the latency one. Kept as the fallback for the
				// case where the estimate has no sample of its own yet and
				// quinn still reports its RFC 9002 initial default, which
				// would read as one fixed figure against every server
				// listed.
				let handshake = start.elapsed();
				let rtt = conn.rtt();
				let took = if rtt.is_zero() || rtt > handshake {
					handshake
				} else {
					rtt
				};
				// Close politely (well inside the server's preamble
				// deadline); the measurement is already taken.
				conn.close(wtransport::VarInt::from_u32(0), b"probe");
				Some(took.as_millis().min(u128::from(u32::MAX)) as u32)
			});
			done(ms);
		});
	if spawned.is_err() {
		log::warn!("failed to spawn net-probe thread");
	}
}
