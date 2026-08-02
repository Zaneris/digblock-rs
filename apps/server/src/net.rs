//! wtransport (WebTransport/QUIC) endpoint: accepts sessions, parses the
//! wire, and bridges to the sim thread. All game logic lives in sim.rs —
//! this file only moves bytes.

use crate::master::{self, MasterState, VerifyError};
use crate::sim::{OutMsg, SimEvent, OUT_CAP};
use crossbeam_channel::Sender;
use digblock_net::codec::{self, FrameReader};
use digblock_net::messages::{ClientMsg, Datagram, ServerMsg};
use digblock_net::protocol::{
	decode_preamble, encode_preamble, MIN_PROTOCOL, PREAMBLE_LEN, PROTOCOL_VERSION, TICK_RATE,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::channel;
use wtransport::endpoint::endpoint_side::Server;
use wtransport::tls::Sha256DigestFmt;
use wtransport::{Connection, Endpoint, Identity, RecvStream, SendStream, ServerConfig};

/// How long a fresh connection gets to produce its 8-byte preamble.
const PREAMBLE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long past the preamble the Hello frame gets (mirrors the sim's old
/// lobby deadline; net.rs owns it now that `Connected` is deferred until
/// the token verifies).
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Outer bound on the master verify round trip (ureq's own timeout is 3 s;
/// this also covers spawn_blocking queueing).
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Inbound throttling window. When a budget is exhausted the server stops
/// reading (reliable) or drops without decoding (datagrams) until the
/// window rolls; QUIC flow control backpressures the sender naturally.
const THROTTLE_WINDOW: Duration = Duration::from_millis(100);
/// Reliable frames decoded per window (edit floods can't amplify into
/// unbounded SimEvents + rejection storms).
const MSG_BUDGET: u32 = 32;
/// Input datagrams decoded per window (2× the tick rate is generous).
const DGRAM_BUDGET: u32 = TICK_RATE * 2 / 10;

/// Connections allowed to exist at once (counting pre-Hello lobby
/// members). The 255-player cap applies only post-Hello, and every
/// accepted connection costs a task + inbound budgets; past this, new
/// sessions are dropped before the WebTransport accept.
const MAX_CONNECTIONS: usize = 512;

/// Build the endpoint with a fresh self-signed certificate. The SHA-256
/// hash is logged so clients can pin it: browsers via
/// `serverCertificateHashes`, the native client via `--cert-hash`
/// (`CertPolicy::PinnedSha256`; the default dev posture is Insecure
/// with a loud warning).
pub fn make_endpoint(port: u16) -> anyhow::Result<(Endpoint<Server>, [u8; 32])> {
	let identity = Identity::self_signed(["localhost", "127.0.0.1", "::1"])?;
	let hash = identity.certificate_chain().as_slice()[0].hash();
	log::info!("cert sha-256: {}", hash.fmt(Sha256DigestFmt::BytesArray));
	let hash_bytes: [u8; 32] = *hash.as_ref();
	// IPv4-any rather than the dual-stack default: Docker Desktop's UDP
	// port proxy fails to deliver to a dual-stack [::] bind in-container,
	// and v4-only costs nothing for a game server.
	let config = ServerConfig::builder()
		.with_bind_config(wtransport::config::IpBindConfig::InAddrAnyV4, port)
		.with_identity(identity)
		.max_idle_timeout(Some(Duration::from_secs(15)))?
		.keep_alive_interval(Some(Duration::from_secs(3)))
		.build();
	Ok((Endpoint::server(config)?, hash_bytes))
}

/// Accept loop: one task per connection, forever.
pub async fn serve(endpoint: Endpoint<Server>, events: Sender<SimEvent>, master: Arc<MasterState>) {
	let mut next_conn: u64 = 1;
	let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
	loop {
		let incoming = endpoint.accept().await;
		if active.load(std::sync::atomic::Ordering::Relaxed) >= MAX_CONNECTIONS {
			// Dropping the incoming session rejects it without spending a
			// task on the handshake.
			log::warn!("connection refused: at capacity ({MAX_CONNECTIONS})");
			continue;
		}
		let conn_id = next_conn;
		next_conn += 1;
		let events = events.clone();
		let master = master.clone();
		active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		let active = active.clone();
		tokio::spawn(async move {
			let conn = match incoming.await {
				Ok(request) => match request.accept().await {
					Ok(conn) => conn,
					Err(e) => {
						active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
						return log::warn!("conn {conn_id}: accept: {e}");
					}
				},
				Err(e) => {
					active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
					return log::warn!("conn {conn_id}: session: {e}");
				}
			};
			log::info!("conn {conn_id}: {}", conn.remote_address());
			handle_connection(conn, conn_id, &events, &master).await;
			let _ = events.send(SimEvent::Disconnected { conn_id });
			active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
			log::info!("conn {conn_id}: closed");
		});
	}
}

async fn handle_connection(
	conn: Connection,
	conn_id: u64,
	events: &Sender<SimEvent>,
	master: &MasterState,
) {
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
		send_refused(
			&mut send,
			format!(
				"protocol {client_protocol}, server supports {MIN_PROTOCOL}..={PROTOCOL_VERSION}"
			),
		)
		.await;
		log::info!("conn {conn_id}: refused (protocol {client_protocol})");
		return;
	}

	// Await the Hello and verify its one-time token with the master BEFORE
	// the sim ever hears about this connection — there is no tokenless way
	// onto a server. Leftover bytes buffered past the Hello frame carry
	// into the main loop untouched.
	let mut frames = FrameReader::default();
	let mut buf = vec![0u8; 64 * 1024];
	let hello = match tokio::time::timeout(
		HELLO_TIMEOUT,
		read_first_frame(&mut recv, &mut frames, &mut buf),
	)
	.await
	{
		Ok(Some(frame)) => match codec::decode::<ClientMsg>(&frame) {
			Ok(msg) => msg,
			Err(e) => return log::warn!("conn {conn_id}: bad hello frame: {e}"),
		},
		Ok(None) => return log::warn!("conn {conn_id}: closed before hello"),
		Err(_) => return log::warn!("conn {conn_id}: hello timeout"),
	};
	let mut hello = hello;
	let ClientMsg::Hello { token, .. } = &hello else {
		return log::warn!("conn {conn_id}: first frame was not hello");
	};
	let token = *token;
	match verify_join(master, &token).await {
		Ok(display_name) => {
			// The master's identity wins over the claimed name; the sim's
			// sanitize_name still applies downstream.
			if let ClientMsg::Hello { name, .. } = &mut hello {
				*name = display_name;
			}
		}
		Err(reason) => {
			log::info!("conn {conn_id}: refused ({reason})");
			send_refused(&mut send, reason).await;
			return;
		}
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
	// Same channel as Connected, so ordering is guaranteed; the sim's
	// lobby reaper remains a harmless backstop.
	let _ = events.send(SimEvent::Msg {
		conn_id,
		msg: hello,
	});

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

/// Gatekeeper for the mandatory-token join flow. Fails closed: when the
/// master cannot confirm a token, nobody gets in — a registered server is a
/// listed server, and with the master down no legitimate player can mint a
/// token either.
async fn verify_join(master: &MasterState, token: &[u8; 16]) -> Result<String, String> {
	if *token == [0u8; 16] {
		return Err("joining requires a token from the master server".into());
	}
	let Some(reg) = master.registration() else {
		return Err("server not yet registered with master".into());
	};
	if master.token_already_used(token) {
		return Err("token already used".into());
	}
	let url = master.url.clone();
	let token_c = *token;
	let verify =
		tokio::task::spawn_blocking(move || master::verify_token(&url, &reg.server_key, &token_c));
	match tokio::time::timeout(VERIFY_TIMEOUT, verify).await {
		Ok(Ok(Ok(display_name))) => {
			master.mark_token_used(*token);
			Ok(display_name)
		}
		Ok(Ok(Err(VerifyError::Denied))) => {
			Err("token rejected — request a fresh join from the master server".into())
		}
		Ok(Ok(Err(VerifyError::Unavailable(e)))) => {
			log::warn!("master verify unavailable: {e}");
			Err("authentication service unavailable, try again shortly".into())
		}
		Ok(Err(e)) => {
			log::warn!("master verify task failed: {e}");
			Err("authentication service unavailable, try again shortly".into())
		}
		Err(_) => Err("authentication service unavailable, try again shortly".into()),
	}
}

async fn send_refused(send: &mut SendStream, reason: String) {
	let refused = ServerMsg::Refused { reason };
	let mut buf = Vec::new();
	if codec::write_frame(&refused, &mut buf).is_ok() {
		let _ = send.write_all(&buf).await;
		let _ = send.finish().await;
	}
}

/// Read until one complete frame is buffered; None = closed/corrupt.
/// Bytes past the frame stay in `frames` for the caller's main loop.
async fn read_first_frame(
	recv: &mut RecvStream,
	frames: &mut FrameReader,
	buf: &mut [u8],
) -> Option<Vec<u8>> {
	loop {
		match frames.next_frame() {
			Ok(Some(frame)) => return Some(frame),
			Ok(None) => {}
			Err(_) => return None,
		}
		match recv.read(buf).await {
			Ok(Some(n)) if n > 0 => frames.extend(&buf[..n]),
			_ => return None,
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
