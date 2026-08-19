//! wtransport (WebTransport/QUIC) endpoint: accepts sessions, parses the
//! wire, and bridges to the sim thread. All game logic lives in sim.rs —
//! this file only moves bytes.

use crate::master::{self, MasterState, VerifyError};
use crate::sim::{OUT_CAP, OutMsg, SimEvent};
use crossbeam_channel::Sender;
use digblock_net::codec::{self, FrameReader};
use digblock_net::messages::{ClientMsg, Datagram, ServerMsg};
use digblock_net::protocol::{
	MAX_CLIENT_FRAME, MIN_PROTOCOL, PREAMBLE_LEN, PROTOCOL_VERSION, TICK_RATE, decode_preamble,
	encode_preamble,
};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc::channel;
use wtransport::endpoint::endpoint_side::Server;
use wtransport::tls::Sha256DigestFmt;
use wtransport::{Connection, Endpoint, Identity, RecvStream, SendStream, ServerConfig};

/// How long a fresh connection gets to produce its `PREAMBLE_LEN` preamble.
const PREAMBLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard ceiling on everything before the sim hears about a connection:
/// the QUIC/WebTransport handshake, the control stream, both timed reads,
/// and the master round trip.
///
/// Without it a peer could hold a connection slot forever by simply never
/// opening the control stream — `accept_bi()` had no deadline, and
/// `keep_alive_interval` means the idle timeout never fires against a peer
/// whose QUIC stack merely ACKs (which the spec requires). A wave of frozen
/// tabs would then wedge the server against `MAX_CONNECTIONS` permanently.
const LOBBY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long past the preamble the Hello frame gets (mirrors the sim's old
/// lobby deadline; net.rs owns it now that `Connected` is deferred until
/// the token verifies).
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Outer bound on the master verify round trip — longer than the ureq agent's
/// own timeout in `master.rs`, since this also covers spawn_blocking queueing.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a broken stream waits for the CONNECTION to account for the
/// same ending. See [`peer_reason`] — this only ever elapses when the
/// connection is genuinely still up, and it delays the sim hearing about
/// the departure by exactly that much, so it stays short.
const CLOSE_REASON_WAIT: Duration = Duration::from_millis(500);

/// Inbound throttling window. When a budget is exhausted the server stops
/// reading (reliable) or drops without decoding (datagrams) until the
/// window rolls; QUIC flow control backpressures the sender naturally.
const THROTTLE_WINDOW: Duration = Duration::from_millis(100);
/// Reliable frames decoded per window (edit floods can't amplify into
/// unbounded SimEvents + rejection storms).
const MSG_BUDGET: u32 = 32;
/// Input datagrams decoded per window — a multiple of the tick rate, so an
/// honest client's bursts never clip.
const DGRAM_BUDGET: u32 = TICK_RATE * 2 / 10;

/// Connections allowed to exist at once (counting pre-Hello lobby
/// members). The player-id cap applies only post-Hello, and every
/// accepted connection costs a task + inbound budgets; past this, new
/// sessions are dropped before the WebTransport accept.
const MAX_CONNECTIONS: usize = 512;

/// Live connections one peer address may hold, by default. Generous enough
/// for a household or a shared connection playing together, tight enough
/// that no single address can spend the whole table — see [`IpSlot`].
///
/// Configurable (`--max-conns-per-ip`) because the default is calibrated
/// for an internet-facing server and every multi-client harness we run is
/// the opposite case: `just bot -- --bots 12` and `just compose-bots` put
/// every client behind ONE address, so the bots past the cap silently
/// failed to connect and the soak read as a netcode regression.
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 8;

/// Rotate the endpoint's certificate this often — well inside the validity
/// `fresh_identity` issues, so a few failed attempts (master down, clock
/// trouble) still leave days of margin before the pinned cert expires.
const CERT_ROTATE_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Retry cadence after a failed rotation, while days of validity remain.
const CERT_ROTATE_RETRY: Duration = Duration::from_secs(60 * 60);

/// Fresh self-signed identity + the SHA-256 pin clients use: browsers via
/// `serverCertificateHashes`, the native client via `--cert-hash`
/// (`CertPolicy::PinnedSha256`; the default dev posture is Insecure
/// with a loud warning).
fn fresh_identity() -> anyhow::Result<(Identity, [u8; 32])> {
	// Shorter validity than the wtransport default, which sits exactly ON
	// Chrome's cap for `serverCertificateHashes` certs — Chrome rejects at
	// the boundary with any clock skew. `rotate_certs` swaps in a fresh cert
	// every CERT_ROTATE_INTERVAL, so long-running servers never reach the
	// window's edge.
	let identity = Identity::self_signed_builder()
		.subject_alt_names(["localhost", "127.0.0.1", "::1"])
		.from_now_utc()
		.validity_days(10)
		.build()?;
	let hash = identity.certificate_chain().as_slice()[0].hash();
	log::info!("cert sha-256: {}", hash.fmt(Sha256DigestFmt::BytesArray));
	Ok((identity, *hash.as_ref()))
}

fn server_config(identity: Identity, port: u16) -> anyhow::Result<ServerConfig> {
	// IPv4-any rather than the dual-stack default: Docker Desktop's UDP
	// port proxy fails to deliver to a dual-stack [::] bind in-container,
	// and v4-only costs nothing for a game server.
	Ok(ServerConfig::builder()
		.with_bind_config(wtransport::config::IpBindConfig::InAddrAnyV4, port)
		.with_identity(identity)
		.max_idle_timeout(Some(Duration::from_secs(15)))?
		.keep_alive_interval(Some(Duration::from_secs(3)))
		.build())
}

/// Build the endpoint with a fresh self-signed certificate.
pub fn make_endpoint(port: u16) -> anyhow::Result<(Endpoint<Server>, [u8; 32])> {
	let (identity, hash) = fresh_identity()?;
	Ok((Endpoint::server(server_config(identity, port)?)?, hash))
}

/// Swap a fresh certificate into the live endpoint. Existing connections
/// are untouched (`reload_config` with `rebind: false` keeps the socket and
/// only replaces the TLS config for future handshakes); the bind port in
/// the rebuilt config is ignored.
pub fn rotate_cert(endpoint: &Endpoint<Server>) -> anyhow::Result<[u8; 32]> {
	let (identity, hash) = fresh_identity()?;
	endpoint.reload_config(server_config(identity, 0)?, false)?;
	Ok(hash)
}

/// Periodic in-process cert rotation: every `CERT_ROTATE_INTERVAL`, swap in
/// a fresh cert and nudge the master loop (via `reregister`) to re-register
/// so the master hands out the new pin. Joins racing the swap can see a
/// stale pin for one HTTP round trip and simply retry.
pub async fn rotate_certs(
	endpoint: Arc<Endpoint<Server>>,
	cert_hash: Arc<RwLock<[u8; 32]>>,
	reregister: Arc<Notify>,
) {
	loop {
		tokio::time::sleep(CERT_ROTATE_INTERVAL).await;
		loop {
			match rotate_cert(&endpoint) {
				Ok(hash) => {
					*cert_hash.write().unwrap() = hash;
					reregister.notify_one();
					log::info!("rotated certificate; re-registering new pin");
					break;
				}
				Err(e) => {
					log::error!(
						"cert rotation failed ({e}); retrying in {}s",
						CERT_ROTATE_RETRY.as_secs()
					);
					tokio::time::sleep(CERT_ROTATE_RETRY).await;
				}
			}
		}
	}
}

/// One occupied connection slot, released on drop — including on a panic
/// inside the connection task, which a manual `fetch_sub` would skip.
struct ConnSlot(Arc<std::sync::atomic::AtomicUsize>);

impl ConnSlot {
	fn take(active: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
		active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		Self(active.clone())
	}
}

impl Drop for ConnSlot {
	fn drop(&mut self) {
		self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
	}
}

/// Live connections per peer address.
type IpCounts = Arc<std::sync::Mutex<rustc_hash::FxHashMap<std::net::IpAddr, usize>>>;

/// One address's share of the connection table, released on drop.
///
/// [`MAX_CONNECTIONS`] alone let ONE peer hold every slot and force an
/// outbound token verify per slot per [`VERIFY_TIMEOUT`] window, turning
/// this server into an unauthenticated amplifier against its own master
/// The ordering that allows it is unavoidable — a
/// connection's account is not known until the verify answers — so the cap
/// goes on the thing that actually identifies the abuser at that point,
/// which is the address.
///
/// Same RAII shape as [`ConnSlot`], and for the same reason: a panic
/// anywhere in the connection task would skip a paired decrement and leak
/// the count forever.
struct IpSlot {
	counts: IpCounts,
	ip: std::net::IpAddr,
}

impl IpSlot {
	/// `None` when this address already holds `limit` connections.
	fn take(counts: &IpCounts, ip: std::net::IpAddr, limit: usize) -> Option<Self> {
		let mut guard = counts.lock().expect("ip counts");
		let n = guard.entry(ip).or_insert(0);
		if *n >= limit {
			return None;
		}
		*n += 1;
		drop(guard);
		Some(Self {
			counts: counts.clone(),
			ip,
		})
	}
}

impl Drop for IpSlot {
	fn drop(&mut self) {
		let mut guard = self.counts.lock().expect("ip counts");
		if let Some(n) = guard.get_mut(&self.ip) {
			*n -= 1;
			// Remove at zero: otherwise the map grows one entry per address
			// ever seen, for the life of the process.
			if *n == 0 {
				guard.remove(&self.ip);
			}
		}
	}
}

/// Accept loop: one task per connection, forever. The endpoint is shared
/// (`Arc`) with the cert-rotation task.
pub async fn serve(
	endpoint: Arc<Endpoint<Server>>,
	events: Sender<SimEvent>,
	master: Arc<MasterState>,
	max_conns_per_ip: usize,
) {
	let mut next_conn: u64 = 1;
	let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
	let per_ip: IpCounts = Arc::new(std::sync::Mutex::new(rustc_hash::FxHashMap::default()));
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
		// RAII, not paired calls: a panic anywhere in the task below would
		// skip a manual `fetch_sub` and leak the slot forever.
		let slot = ConnSlot::take(&active);
		let per_ip = per_ip.clone();
		tokio::spawn(async move {
			let _slot = slot;
			// The accept itself is bounded too — a peer that completes the
			// UDP handshake and then stalls the WebTransport CONNECT must
			// not own a slot indefinitely.
			let accepted =
				tokio::time::timeout(LOBBY_TIMEOUT, async { incoming.await?.accept().await }).await;
			let conn = match accepted {
				Ok(Ok(conn)) => conn,
				Ok(Err(e)) => return log::warn!("conn {conn_id}: accept: {e}"),
				Err(_) => return log::warn!("conn {conn_id}: accept timeout"),
			};
			// The per-address cap lands HERE rather than at the top of the
			// accept loop, because the peer address is not known until the
			// session resolves. That is the right place regardless: what it
			// protects is the outbound token verify below, and by this point
			// the QUIC handshake has been paid for either way.
			let ip = conn.remote_address().ip();
			let Some(_ip_slot) = IpSlot::take(&per_ip, ip, max_conns_per_ip) else {
				return log::warn!(
					"conn {conn_id}: refused, {ip} already holds {max_conns_per_ip} connections \
					 (raise --max-conns-per-ip if this is a local soak)"
				);
			};
			log::info!("conn {conn_id}: {}", conn.remote_address());
			let why = handle_connection(conn, conn_id, &events, &master).await;
			let _ = events.send(SimEvent::Disconnected { conn_id });
			match why {
				Some(why) => log::info!("conn {conn_id}: closed ({why})"),
				// Refused or failed in the lobby, which logged its own
				// reason on the way past.
				None => log::info!("conn {conn_id}: closed"),
			}
		});
	}
}

/// `Some(reason)` = an admitted connection ended, in the words its one
/// closing log line wants; `None` = it never got past the lobby, which
/// said why on its own way out.
async fn handle_connection(
	conn: Connection,
	conn_id: u64,
	events: &Sender<SimEvent>,
	master: &MasterState,
) -> Option<String> {
	// Everything up to the sim hearing about this connection runs under one
	// deadline. The individual reads have their own, tighter ones; this
	// covers the steps BETWEEN them — the control-stream accept, our own
	// preamble write, and the master verify — none of which were bounded.
	match tokio::time::timeout(LOBBY_TIMEOUT, lobby(&conn, conn_id, events, master)).await {
		Ok(Some((send, recv, buf, frames, out_rx))) => {
			Some(serve_connection(conn, conn_id, events, send, recv, buf, frames, out_rx).await)
		}
		Ok(None) => None,
		Err(_) => {
			log::warn!("conn {conn_id}: lobby timeout");
			None
		}
	}
}

/// A stream-shaped ending, resolved to the CONNECTION's own account of it.
///
/// wtransport's stream errors carry nothing worth reading — `not
/// connected` is the whole of what one says — while the connection knows
/// whether it timed out, was closed by the peer and with what code, or
/// died of a protocol error. That is also the exact half of the story a
/// browser cannot tell: a `WebTransportError` sourced at the session
/// carries an empty message by the spec, so on a phone the server's copy
/// is the only copy there is.
///
/// Bounded, because a stream can fail on a connection that is still up (a
/// peer that reset just this one), and this wait sits in front of the sim
/// hearing about the departure — which is what frees the account to
/// reconnect.
async fn peer_reason(conn: &Connection, what: &str) -> String {
	match tokio::time::timeout(CLOSE_REASON_WAIT, conn.closed()).await {
		Ok(e) => format!("{what}; {e}"),
		Err(_) => format!("{what}; connection still up"),
	}
}

/// The pre-Hello handshake. `Some` = admitted (the sim has been told);
/// `None` = refused or failed, with the reason already logged.
#[allow(clippy::type_complexity)]
async fn lobby(
	conn: &Connection,
	conn_id: u64,
	events: &Sender<SimEvent>,
	master: &MasterState,
) -> Option<(
	SendStream,
	RecvStream,
	Vec<u8>,
	FrameReader,
	tokio::sync::mpsc::Receiver<OutMsg>,
)> {
	// The client opens the control stream immediately after connecting.
	let (mut send, mut recv) = match conn.accept_bi().await {
		Ok(streams) => streams,
		Err(e) => {
			log::warn!("conn {conn_id}: control stream: {e}");
			return None;
		}
	};

	// Version gate BEFORE any postcard decode: the frozen preamble is the
	// one thing every DigBlock build past and future can exchange, so a
	// differently-shaped old Hello can never hit a decode error — it is
	// refused (or dropped) here without ever reaching the frame parser.
	let client_protocol =
		match tokio::time::timeout(PREAMBLE_TIMEOUT, read_preamble(&mut recv)).await {
			Ok(Some(pre)) => match decode_preamble(&pre) {
				Some(v) => v,
				None => {
					log::warn!("conn {conn_id}: bad preamble magic");
					return None;
				}
			},
			Ok(None) => {
				log::warn!("conn {conn_id}: closed before preamble");
				return None;
			}
			Err(_) => {
				log::warn!("conn {conn_id}: preamble timeout");
				return None;
			}
		};
	// Always answer with our own preamble so even a refused client can
	// report the server's version.
	if send
		.write_all(&encode_preamble(PROTOCOL_VERSION))
		.await
		.is_err()
	{
		return None;
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
		return None;
	}

	// Await the Hello and verify its one-time token with the master BEFORE
	// the sim ever hears about this connection — there is no tokenless way
	// onto a server. Leftover bytes buffered past the Hello frame carry
	// into the main loop untouched.
	// The client direction's cap, not the server's: nothing a client sends
	// is large, and the reader's much larger default limit exists for
	// journal batches going the other way.
	let mut frames = FrameReader::with_limit(MAX_CLIENT_FRAME);
	let mut buf = vec![0u8; 64 * 1024];
	let hello = match tokio::time::timeout(
		HELLO_TIMEOUT,
		read_first_frame(&mut recv, &mut frames, &mut buf),
	)
	.await
	{
		Ok(Some(frame)) => match codec::decode::<ClientMsg>(&frame) {
			Ok(msg) => msg,
			Err(e) => {
				log::warn!("conn {conn_id}: bad hello frame: {e}");
				return None;
			}
		},
		Ok(None) => {
			log::warn!("conn {conn_id}: closed before hello");
			return None;
		}
		Err(_) => {
			log::warn!("conn {conn_id}: hello timeout");
			return None;
		}
	};
	let mut hello = hello;
	let ClientMsg::Hello { token, .. } = &hello else {
		log::warn!("conn {conn_id}: first frame was not hello");
		return None;
	};
	let token = *token;
	let verified = match verify_join(master, &token).await {
		Ok(verified) => {
			// The master's answer wins over the claimed name; the sim's
			// sanitize_name still applies downstream.
			if let ClientMsg::Hello { name, .. } = &mut hello {
				*name = verified.display_name.clone();
			}
			verified
		}
		Err(reason) => {
			log::info!("conn {conn_id}: refused ({reason})");
			send_refused(&mut send, reason).await;
			return None;
		}
	};

	let (out_tx, out_rx) = channel::<OutMsg>(OUT_CAP);
	if events
		.send(SimEvent::Connected {
			conn_id,
			out: out_tx,
			addr: conn.remote_address(),
			// Carried on the CONNECTION rather than in the Hello, because
			// the client never said it: it is the master's answer, and
			// putting it on the wire message would invite a future reader
			// into treating it as something the peer claimed.
			account: verified.account,
		})
		.is_err()
	{
		return None; // sim gone (shutdown)
	}
	// Same channel as Connected, so ordering is guaranteed; the sim's
	// lobby reaper remains a harmless backstop.
	let _ = events.send(SimEvent::Msg {
		conn_id,
		msg: hello,
	});
	Some((send, recv, buf, frames, out_rx))
}

/// The admitted connection's pump: inbound frames/datagrams to the sim,
/// outbound messages to the peer. No overall deadline here — a joined
/// player is expected to stay for hours; liveness is QUIC's idle timeout.
///
/// Returns why it stopped. Every exit hands back its own words, because
/// they used to be indistinguishable: all but one broke out silently and
/// that one logged at `debug`, so `conn N: closed` was the entire
/// server-side record of a kick, a goodbye, a peer that stopped reading,
/// an idle timeout and a wire that went away. Which of those it was is
/// the first question asked of any disconnect report, and the server is
/// routinely the only side that can answer it.
#[allow(clippy::too_many_arguments)]
async fn serve_connection(
	conn: Connection,
	conn_id: u64,
	events: &Sender<SimEvent>,
	mut send: SendStream,
	mut recv: RecvStream,
	mut buf: Vec<u8>,
	mut frames: FrameReader,
	mut out_rx: tokio::sync::mpsc::Receiver<OutMsg>,
) -> String {
	let mut window = tokio::time::Instant::now();
	let mut msg_budget = MSG_BUDGET;
	let mut dgram_budget = DGRAM_BUDGET;
	loop {
		// Drain frames already buffered (possibly left over from a window
		// where the budget ran out). Borrowed, not owned: the decode
		// produces a fully owned `ClientMsg`, so copying the frame out of
		// the reassembly buffer first was one Vec per inbound message per
		// client for nothing (the client side already reads this way).
		while msg_budget > 0 {
			match frames.next_frame_ref() {
				Ok(Some(frame)) => {
					msg_budget -= 1;
					match codec::decode::<ClientMsg>(frame) {
						Ok(msg) => {
							let _ = events.send(SimEvent::Msg { conn_id, msg });
						}
						Err(e) => log::warn!("conn {conn_id}: bad frame: {e}"),
					}
				}
				Ok(None) => break,
				Err(e) => return format!("stream corrupt: {e}"),
			}
		}
		tokio::select! {
			out = out_rx.recv() => match out {
				Some(OutMsg::Datagram(bytes)) => {
					// Lossy by contract: errors (e.g. too-early, MTU) drop.
					let _ = conn.send_datagram(bytes);
				}
				Some(OutMsg::Frame(bytes)) => {
					if let Err(e) = send.write_all(&bytes).await {
						break peer_reason(&conn, &format!("control stream write: {e}")).await;
					}
				}
				// The two endings the SERVER chose: a kick or a ban (`Close`),
				// and the sim letting the client's sender go, which is what a
				// reap and an ordinary goodbye both come out as.
				Some(OutMsg::Close) => break "closed by the server".into(),
				None => break "released by the sim".into(),
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
				// Already the connection's own error, so it needs no lookup.
				Err(e) => break e.to_string(),
			},
			// Reliable reads pause while the frame budget is exhausted —
			// QUIC flow control then backpressures the sender.
			read = recv.read(&mut buf), if msg_budget > 0 => match read {
				Ok(Some(n)) => frames.extend(&buf[..n]),
				// A clean FIN is the whole story and asks the connection
				// nothing: it is a page closing its writer on the way out,
				// with the session still up behind it.
				Ok(None) => break "control stream finished by peer".into(),
				Err(e) => break peer_reason(&conn, &format!("control stream read: {e}")).await,
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
async fn verify_join(
	master: &MasterState,
	token: &[u8; 16],
) -> Result<master::VerifiedJoin, String> {
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
		Ok(Ok(Ok(verified))) => {
			master.mark_token_used(*token);
			Ok(verified)
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rotate_cert_swaps_hash_without_rebinding() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let _guard = rt.enter();
		let (endpoint, boot_hash) = make_endpoint(0).expect("bind ephemeral udp");
		let addr = endpoint.local_addr().unwrap();
		let new_hash = rotate_cert(&endpoint).expect("live cert swap");
		assert_ne!(boot_hash, new_hash, "rotation must mint a new certificate");
		assert_eq!(
			addr,
			endpoint.local_addr().unwrap(),
			"socket must survive rotation"
		);
	}
}
