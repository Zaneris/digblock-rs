//! `NetTransport` impl backed by wtransport on a detached background
//! thread running a current-thread tokio runtime. The engine side stays
//! sync: outbound is a tokio unbounded mpsc (sync `send`, async `recv`),
//! inbound is a crossbeam unbounded channel (sync on both ends).

use crossbeam_channel::{unbounded, Receiver, Sender};
use digblock_net::transport::{ConnStatus, NetTransport, TransportEvent};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use wtransport::{ClientConfig, Endpoint};

enum Outgoing {
	Datagram(Vec<u8>),
	Reliable(Vec<u8>),
}

/// How the client validates the server's TLS certificate.
#[derive(Clone, Copy, Debug)]
pub enum CertPolicy {
	/// Trust anything — dev/LAN posture for self-signed servers. Loudly
	/// logged; pass `--cert-hash` for real deployments.
	Insecure,
	/// Trust exactly this SHA-256 certificate hash (the server logs its
	/// own hash at startup).
	PinnedSha256([u8; 32]),
}

impl CertPolicy {
	/// Parse a pinned policy from the 64-hex-char hash the server logs at
	/// startup (separators like `:` or spaces are tolerated). Shared by
	/// every `--cert-hash` flag so the launchers can't drift.
	pub fn pinned_from_hex(hex: &str) -> Result<CertPolicy, String> {
		let clean: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
		if clean.len() != 64 {
			return Err(format!("want 64 hex chars, got {}", clean.len()));
		}
		let mut out = [0u8; 32];
		for (i, byte) in out.iter_mut().enumerate() {
			*byte = u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).expect("hex checked");
		}
		Ok(CertPolicy::PinnedSha256(out))
	}
}

pub struct ChannelTransport {
	out_tx: UnboundedSender<Outgoing>,
	in_rx: Receiver<TransportEvent>,
	status: ConnStatus,
}

/// Connect in the background with the dev [`CertPolicy::Insecure`]
/// posture; returns immediately with `Connecting`. `url` is a
/// WebTransport URL, e.g. `https://localhost:4433`.
pub fn connect(url: &str) -> ChannelTransport {
	connect_with(url, CertPolicy::Insecure)
}

pub fn connect_with(url: &str, policy: CertPolicy) -> ChannelTransport {
	if matches!(policy, CertPolicy::Insecure) {
		// Once per process, not per connection — multi-bot runs would
		// otherwise repeat it per thread.
		static WARN_ONCE: std::sync::Once = std::sync::Once::new();
		WARN_ONCE.call_once(|| {
			log::warn!(
				"TLS certificate validation is OFF (dev posture) — \
				 pass --cert-hash <sha256-hex> to pin the server's certificate"
			);
		});
	}
	let (out_tx, out_rx) = unbounded_channel();
	let (in_tx, in_rx) = unbounded();
	let url = url.to_string();
	std::thread::Builder::new()
		.name("net-client".into())
		.spawn(move || {
			let rt = tokio::runtime::Builder::new_current_thread()
				.enable_all()
				.build()
				.expect("tokio runtime");
			rt.block_on(run(url, policy, out_rx, in_tx));
		})
		.expect("spawn net-client thread");
	ChannelTransport {
		out_tx,
		in_rx,
		status: ConnStatus::Connecting,
	}
}

impl NetTransport for ChannelTransport {
	fn send_datagram(&mut self, bytes: &[u8]) {
		let _ = self.out_tx.send(Outgoing::Datagram(bytes.to_vec()));
	}

	fn send_reliable(&mut self, bytes: &[u8]) {
		let _ = self.out_tx.send(Outgoing::Reliable(bytes.to_vec()));
	}

	fn poll(&mut self, out: &mut Vec<TransportEvent>) {
		loop {
			match self.in_rx.try_recv() {
				Ok(ev) => {
					match &ev {
						TransportEvent::Connected => self.status = ConnStatus::Connected,
						TransportEvent::Disconnected { .. } => self.status = ConnStatus::Closed,
						_ => {}
					}
					out.push(ev);
				}
				Err(crossbeam_channel::TryRecvError::Empty) => break,
				// The background thread is GONE without having said so —
				// it panicked, or unwound past its `disconnect` call. This
				// used to be indistinguishable from "idle": `try_iter`
				// yields nothing either way and `status` is a cached field,
				// so the engine sat in a session with no snapshots, no
				// error and no way back. Synthesize the disconnect the
				// thread never got to send.
				Err(crossbeam_channel::TryRecvError::Disconnected) => {
					if self.status != ConnStatus::Closed {
						self.status = ConnStatus::Closed;
						out.push(TransportEvent::Disconnected {
							reason: "network thread stopped".into(),
						});
					}
					break;
				}
			}
		}
	}

	fn status(&self) -> ConnStatus {
		self.status
	}
}

async fn run(
	url: String,
	policy: CertPolicy,
	mut out_rx: UnboundedReceiver<Outgoing>,
	in_tx: Sender<TransportEvent>,
) {
	let disconnect = |reason: String| {
		let _ = in_tx.send(TransportEvent::Disconnected { reason });
	};

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
	let endpoint = match Endpoint::client(config) {
		Ok(e) => e,
		Err(e) => return disconnect(format!("endpoint: {e}")),
	};
	let conn = match endpoint.connect(&url).await {
		Ok(c) => c,
		Err(e) => return disconnect(format!("connect: {e}")),
	};
	let (mut send, mut recv) = match conn.open_bi().await {
		Ok(opening) => match opening.await {
			Ok(s) => s,
			Err(e) => return disconnect(format!("open stream: {e}")),
		},
		Err(e) => return disconnect(format!("open stream: {e}")),
	};
	let _ = in_tx.send(TransportEvent::Connected);

	let mut buf = vec![0u8; 64 * 1024];
	let reason = loop {
		tokio::select! {
			out = out_rx.recv() => match out {
				Some(Outgoing::Datagram(b)) => {
					// Best-effort by contract; drop on error.
					let _ = conn.send_datagram(b);
				}
				Some(Outgoing::Reliable(b)) => {
					if let Err(e) = send.write_all(&b).await {
						break format!("stream write: {e}");
					}
				}
				// Engine dropped the transport: shut down quietly.
				None => break "closed by client".to_string(),
			},
			dgram = conn.receive_datagram() => match dgram {
				Ok(d) => {
					let _ = in_tx.send(TransportEvent::Datagram(d.payload().to_vec()));
				}
				Err(e) => break format!("connection: {e}"),
			},
			n = recv.read(&mut buf) => match n {
				Ok(Some(n)) => {
					let _ = in_tx.send(TransportEvent::Reliable(buf[..n].to_vec()));
				}
				Ok(None) => break "stream closed by server".to_string(),
				Err(e) => break format!("stream read: {e}"),
			},
		}
	};
	disconnect(reason);
}
