//! wtransport (WebTransport/QUIC) endpoint: accepts sessions, parses the
//! wire, and bridges to the sim thread. All game logic lives in sim.rs —
//! this file only moves bytes.

use crate::sim::{OutMsg, SimEvent, OUT_CAP};
use crossbeam_channel::Sender;
use digblock_net::codec::{self, FrameReader};
use digblock_net::messages::{ClientMsg, Datagram, ServerMsg};
use digblock_net::protocol::{
	decode_preamble, encode_preamble, MIN_PROTOCOL, PREAMBLE_LEN, PROTOCOL_VERSION, TICK_RATE,
};
use std::time::Duration;
use tokio::sync::mpsc::channel;
use wtransport::endpoint::endpoint_side::Server;
use wtransport::tls::Sha256DigestFmt;
use wtransport::{Connection, Endpoint, Identity, RecvStream, ServerConfig};

/// How long a fresh connection gets to produce its 8-byte preamble.
const PREAMBLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Inbound throttling window. When a budget is exhausted the server stops
/// reading (reliable) or drops without decoding (datagrams) until the
/// window rolls; QUIC flow control backpressures the sender naturally.
const THROTTLE_WINDOW: Duration = Duration::from_millis(100);
/// Reliable frames decoded per window (edit floods can't amplify into
/// unbounded SimEvents + rejection storms).
const MSG_BUDGET: u32 = 32;
/// Input datagrams decoded per window (2× the tick rate is generous).
const DGRAM_BUDGET: u32 = TICK_RATE * 2 / 10;

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

	// Version gate BEFORE any postcard decode: the frozen preamble is the
	// one thing every DigBlock build past and future can exchange, so a
	// differently-shaped old Hello can never hit a decode error — it is
	// refused (or dropped) here without ever reaching the frame parser.
	let client_protocol =
		match tokio::time::timeout(PREAMBLE_TIMEOUT, read_preamble(&mut recv)).await {
			Ok(Some(pre)) => match decode_preamble(&pre) {
				Some(v) => v,
				None => return log::warn!("conn {conn_id}: bad preamble magic"),
			},
			Ok(None) => return log::warn!("conn {conn_id}: closed before preamble"),
			Err(_) => return log::warn!("conn {conn_id}: preamble timeout"),
		};
	// Always answer with our own preamble so even a refused client can
	// report the server's version.
	if send
		.write_all(&encode_preamble(PROTOCOL_VERSION))
		.await
		.is_err()
	{
		return;
	}
	if !(MIN_PROTOCOL..=PROTOCOL_VERSION).contains(&client_protocol) {
		let refused = ServerMsg::Refused {
			reason: format!(
				"protocol {client_protocol}, server supports {MIN_PROTOCOL}..={PROTOCOL_VERSION}"
			),
		};
		let mut buf = Vec::new();
		if codec::write_frame(&refused, &mut buf).is_ok() {
			let _ = send.write_all(&buf).await;
			let _ = send.finish().await;
		}
		log::info!("conn {conn_id}: refused (protocol {client_protocol})");
		return;
	}

	let (out_tx, mut out_rx) = channel::<OutMsg>(OUT_CAP);
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
	let mut window = tokio::time::Instant::now();
	let mut msg_budget = MSG_BUDGET;
	let mut dgram_budget = DGRAM_BUDGET;
	loop {
		// Drain frames already buffered (possibly left over from a window
		// where the budget ran out).
		while msg_budget > 0 {
			match frames.next_frame() {
				Ok(Some(frame)) => {
					msg_budget -= 1;
					match codec::decode::<ClientMsg>(&frame) {
						Ok(msg) => {
							let _ = events.send(SimEvent::Msg { conn_id, msg });
						}
						Err(e) => log::warn!("conn {conn_id}: bad frame: {e}"),
					}
				}
				Ok(None) => break,
				Err(e) => {
					log::warn!("conn {conn_id}: stream corrupt: {e}");
					return;
				}
			}
		}
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
					// Past the budget: drop without decoding.
					if dgram_budget > 0 {
						dgram_budget -= 1;
						if let Ok(Datagram::Input(batch)) = codec::decode_datagram(d.payload().as_ref()) {
							let _ = events.send(SimEvent::Inputs { conn_id, batch });
						}
					}
				}
				Err(e) => {
					log::debug!("conn {conn_id}: {e}");
					break;
				}
			},
			// Reliable reads pause while the frame budget is exhausted —
			// QUIC flow control then backpressures the sender.
			read = recv.read(&mut buf), if msg_budget > 0 => match read {
				Ok(Some(n)) => frames.extend(&buf[..n]),
				Ok(None) | Err(_) => break,
			},
			_ = tokio::time::sleep_until(window + THROTTLE_WINDOW) => {
				window = tokio::time::Instant::now();
				msg_budget = MSG_BUDGET;
				dgram_budget = DGRAM_BUDGET;
			},
		}
	}
}

/// Fill exactly one preamble from the stream; None = closed early.
async fn read_preamble(recv: &mut RecvStream) -> Option<[u8; PREAMBLE_LEN]> {
	let mut pre = [0u8; PREAMBLE_LEN];
	let mut have = 0;
	while have < PREAMBLE_LEN {
		match recv.read(&mut pre[have..]).await {
			Ok(Some(n)) if n > 0 => have += n,
			_ => return None,
		}
	}
	Some(pre)
}
