//! wtransport (WebTransport/QUIC) endpoint: accepts sessions, parses the
//! wire, and bridges to the sim thread. All game logic lives in sim.rs —
//! this file only moves bytes.

use crate::sim::{OutMsg, SimEvent};
use crossbeam_channel::Sender;
use digblock_net::codec::{self, FrameReader};
use digblock_net::messages::{ClientMsg, Datagram};
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;
use wtransport::endpoint::endpoint_side::Server;
use wtransport::tls::Sha256DigestFmt;
use wtransport::{Connection, Endpoint, Identity, ServerConfig};

/// Build the endpoint with a fresh self-signed certificate. The SHA-256
/// hash is logged for browser `serverCertificateHashes` dev flows; the
/// native client skips validation entirely.
pub fn make_endpoint(port: u16) -> anyhow::Result<Endpoint<Server>> {
	let identity = Identity::self_signed(["localhost", "127.0.0.1", "::1"])?;
	let hash = identity.certificate_chain().as_slice()[0].hash();
	log::info!("cert sha-256: {}", hash.fmt(Sha256DigestFmt::BytesArray));
	// IPv4-any rather than the dual-stack default: Docker Desktop's UDP
	// port proxy fails to deliver to a dual-stack [::] bind in-container,
	// and v4-only costs nothing for a game server.
	let config = ServerConfig::builder()
		.with_bind_config(wtransport::config::IpBindConfig::InAddrAnyV4, port)
		.with_identity(identity)
		.max_idle_timeout(Some(Duration::from_secs(15)))?
		.keep_alive_interval(Some(Duration::from_secs(3)))
		.build();
	Ok(Endpoint::server(config)?)
}

/// Accept loop: one task per connection, forever.
pub async fn serve(endpoint: Endpoint<Server>, events: Sender<SimEvent>) {
	let mut next_conn: u64 = 1;
	loop {
		let incoming = endpoint.accept().await;
		let conn_id = next_conn;
		next_conn += 1;
		let events = events.clone();
		tokio::spawn(async move {
			let conn = match incoming.await {
				Ok(request) => match request.accept().await {
					Ok(conn) => conn,
					Err(e) => return log::warn!("conn {conn_id}: accept: {e}"),
				},
				Err(e) => return log::warn!("conn {conn_id}: session: {e}"),
			};
			log::info!("conn {conn_id}: {}", conn.remote_address());
			handle_connection(conn, conn_id, &events).await;
			let _ = events.send(SimEvent::Disconnected { conn_id });
			log::info!("conn {conn_id}: closed");
		});
	}
}

async fn handle_connection(conn: Connection, conn_id: u64, events: &Sender<SimEvent>) {
	// The client opens the control stream immediately after connecting.
	let (mut send, mut recv) = match conn.accept_bi().await {
		Ok(streams) => streams,
		Err(e) => return log::warn!("conn {conn_id}: control stream: {e}"),
	};
	let (out_tx, mut out_rx) = unbounded_channel::<OutMsg>();
	if events
		.send(SimEvent::Connected {
			conn_id,
			out: out_tx,
		})
		.is_err()
	{
		return; // sim gone (shutdown)
	}

	let mut frames = FrameReader::default();
	let mut buf = vec![0u8; 64 * 1024];
	loop {
		tokio::select! {
			out = out_rx.recv() => match out {
				Some(OutMsg::Datagram(bytes)) => {
					// Lossy by contract: errors (e.g. too-early, MTU) drop.
					let _ = conn.send_datagram(bytes);
				}
				Some(OutMsg::Frame(bytes)) => {
					if send.write_all(&bytes).await.is_err() {
						break;
					}
				}
				Some(OutMsg::Close) | None => break,
			},
			dgram = conn.receive_datagram() => match dgram {
				Ok(d) => {
					if let Ok(Datagram::Input(batch)) = codec::decode_datagram(d.payload().as_ref()) {
						let _ = events.send(SimEvent::Inputs { conn_id, batch });
					}
				}
				Err(e) => {
					log::debug!("conn {conn_id}: {e}");
					break;
				}
			},
			read = recv.read(&mut buf) => match read {
				Ok(Some(n)) => {
					frames.extend(&buf[..n]);
					loop {
						match frames.next_frame() {
							Ok(Some(frame)) => match codec::decode::<ClientMsg>(&frame) {
								Ok(msg) => {
									let _ = events.send(SimEvent::Msg { conn_id, msg });
								}
								Err(e) => log::warn!("conn {conn_id}: bad frame: {e}"),
							},
							Ok(None) => break,
							Err(e) => {
								log::warn!("conn {conn_id}: stream corrupt: {e}");
								return;
							}
						}
					}
				}
				Ok(None) | Err(_) => break,
			},
		}
	}
}
