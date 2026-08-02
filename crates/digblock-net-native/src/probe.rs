//! One-shot reachability/latency probe: time a bare WebTransport connect,
//! then close immediately. The reported number includes the QUIC+TLS
//! handshake (~2×RTT) — honest and consistent for ranking servers in the
//! browser list. Runs on its own short-lived thread, mirroring
//! channel_transport's pattern; the result arrives via `done`.

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
				let elapsed = start.elapsed().as_millis().min(u32::MAX as u128) as u32;
				// Close politely (well inside the server's preamble
				// deadline); the measurement is already taken.
				conn.close(wtransport::VarInt::from_u32(0), b"probe");
				Some(elapsed)
			});
			done(ms);
		});
	if spawned.is_err() {
		log::warn!("failed to spawn net-probe thread");
	}
}
