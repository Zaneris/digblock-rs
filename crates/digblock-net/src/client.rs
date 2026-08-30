//! `NetClient`: owns the transport plus the prediction/edit/interp
//! modules, decodes wire messages, and hands the engine plain events —
//! the engine never touches wire bytes.

use crate::codec::{self, FrameReader};
use crate::edits::{PendingEdit, PendingEdits};
use crate::messages::{ClientMsg, Datagram, EditKind, RejectReason, ServerMsg};
use crate::predict::{Predictor, Reconciled};
use crate::protocol::{
	CLIENT_TICK_LEAD, CONNECT_TIMEOUT_SECS, HANDSHAKE_TIMEOUT_SECS, INPUT_SEND_STRIDE,
	MIN_PROTOCOL, PREAMBLE_LEN, PROTOCOL_VERSION, PlayerId, RECV_TIMEOUT_SECS, decode_preamble,
	encode_preamble, protocol_supported, sanitize_name, sim_supported,
};
use crate::remote::{RemotePlayers, RemoteSnap};
use crate::transport::{ConnStatus, NetTransport, TransportEvent};
use glam::Vec3;

/// Engine events one `poll` will produce before stopping and leaving the rest buffered for the
/// next call. A `JournalColumn` frame expands to one event PER CELL, so a budget in frames
/// would not bound anything: a few hundred columns of join sync is six figures of events,
/// materialised inside a single rAF callback on wasm. Matched to the engine's own per-frame
/// apply budget so the backlog cannot outrun its drain either.
const POLL_EVENT_BUDGET: usize = 256;

pub struct NetClientConfig {
	pub name: String,
	pub draw_dist: i32,
	/// One-time join token minted by the master server; sent in `Hello`
	/// and verified server-side before admission. All zeros = no token
	/// (loopback tests only — real servers refuse it).
	pub token: [u8; 16],
	/// The key this machine's pre-account character was filed under, if one is still on disk —
	/// the truncated hash of the old identity secret, computed by the LAUNCHER so the secret
	/// itself stays there. Sent once per connection as `ClaimLegacy`, and to EVERY server,
	/// because each holds its own records and a player migrates on each separately; an
	/// already-claimed or never-played server answers with silence.
	pub legacy_identity: Option<[u8; 16]>,
}

#[derive(Debug)]
pub enum NetEvent {
	/// Handshake complete: rebuild the world around the server's seed.
	Welcome {
		seed: digblock_core::worldgen::Seed,
		player_id: PlayerId,
		spawn: Vec3,
		server_tick: u32,
		/// The server's world clock — anchor the sun here.
		tod: f32,
		/// Facing to adopt on arrival (`/set-spawn-dir`), or `Vec3::ZERO`
		/// to keep the current view — see `ServerMsg::Welcome`.
		spawn_look: Vec3,
	},
	Refused {
		reason: String,
	},
	/// The journaled columns around the last arrival have landed — releases the engine's hold.
	AreaReady,
	/// Local-player slice of a snapshot (remotes are consumed internally by `remotes`).
	Ack {
		acked_input_tick: u32,
		me: digblock_core::player::PlayerState,
	},
	/// Authoritative cell value (id + state nibble) — it owns the cell, superseding predictions.
	Edit {
		cell: (i32, i32, i32),
		cell_value: u16,
		/// `Some(author)` only for a live broadcast authored by someone
		/// other than us (SERVER_ID = the fluid sim). `None` for join
		/// sync and for the echo of our own edits — the audio layer keys
		/// off this so a join replay never plays a thud storm.
		remote_author: Option<PlayerId>,
	},
	/// One cell of a rejected prediction: revert `.inv_delta`, the cell unless `.superseded`.
	EditRejected {
		edit: PendingEdit,
		/// The server's verdict, for rejections worth explaining to the
		/// player.
		reason: RejectReason,
	},
	/// The server refused a hold-to-dig as it started — abandon the local hold, don't time out.
	DigRefused {
		cell: (i32, i32, i32),
		reason: RejectReason,
	},
	/// Authoritative inventory as raw wire pairs: re-base as `Inventory::from_wire + Σ pending`.
	Inventory {
		counts: Vec<(u16, u32)>,
		/// The predictions still in flight when `counts` was DECODED.
		///
		/// Snapshotted here rather than read from `PendingEdits` at
		/// dispatch time, because those are not the same moment:
		/// `confirm`/`reject`/`on_authoritative` all run inside `poll`,
		/// before the engine dispatches anything from that same poll, and
		/// the engine's queue can park this event behind hundreds of edits
		/// besides. Reading the live set later drops a delta whose edit was
		/// confirmed in between (the count never comes back) or
		/// double-counts one whose edit was rejected (the rejection reverts
		/// it a second time) — either way the client is permanently off by
		/// one until the next snapshot.
		pending: Vec<(digblock_core::block::BlockType, i32)>,
	},
	/// Another player joined. Also delivered as a roster right after Welcome, one event each.
	PlayerJoined {
		id: PlayerId,
		name: String,
	},
	PlayerLeft {
		id: PlayerId,
	},
	/// A chat line, already server-sanitized; `from` may be our own id (the echo confirms it).
	Chat {
		from: PlayerId,
		text: String,
	},
	/// The server teleported us — hard-set the player there; `look` zero = keep the current view.
	Teleported {
		pos: Vec3,
		look: Vec3,
	},
	/// The server's chat-command set (admin only): `(usage, description)` for Tab-autocomplete.
	AdminCommands {
		commands: Vec<(String, String)>,
	},
	/// An admin command's answer, as typed data — the sentence is the client's to compose.
	AdminReply(crate::messages::AdminReply),
	/// The identity's persisted item layout, sent in join sync after the inventory; reconcile it.
	Layout {
		hotbar: Vec<u16>,
		grid: Vec<u16>,
	},
	/// World-clock heartbeat: hard-set the sun's angle, inter-beat drift being below visible.
	TimeOfDay {
		tod: f32,
	},
	/// Full per-skill XP totals `(skill index, total)`, join sync — adopt wholesale, no toasts.
	Skills {
		xp: Vec<(u8, u64)>,
	},
	/// One skill's new authoritative XP total: take the max (reorder-safe), animate the delta.
	XpGain {
		skill: u8,
		total: u64,
	},
	/// SOMEONE ELSE crossed a level (never our own). Presentation only, nothing about state.
	LevelUp {
		player: PlayerId,
		skill: u8,
		level: u32,
	},
	/// The world's resolved waystone sites, join sync — server state the client cannot derive.
	Waystones {
		entries: Vec<(u8, [f32; 3])>,
	},
	/// A cast the server refused, and why — `voice` chooses the words.
	CastRefused {
		reason: crate::messages::CastRefusal,
	},
	/// Conjured light standing in `cell` for `secs_left` more seconds — anyone's, backlog included.
	Kindled {
		cell: [i32; 3],
		secs_left: u16,
	},
	/// A Sphere's channel BEGAN — anyone's, ours included; the cells derive, and are never sent.
	SphereWinding {
		by: PlayerId,
		head: [i32; 3],
		radius: u8,
		span: u32,
		block: u16,
	},
	/// A Sphere's channel ENDED with `landed` cells in the world; `sealed` says whether they stayed.
	SphereEnded {
		by: PlayerId,
		head: [i32; 3],
		radius: u8,
		landed: u32,
		sealed: bool,
		block: u16,
	},
	Disconnected {
		reason: String,
	},
}

pub struct NetClient {
	transport: Box<dyn NetTransport>,
	pub predictor: Predictor,
	pub edits: PendingEdits,
	pub remotes: RemotePlayers,
	my_id: Option<PlayerId>,
	frames: FrameReader,
	config: NetClientConfig,
	scratch: Vec<TransportEvent>,
	/// Server preamble bytes collected so far (the first PREAMBLE_LEN
	/// reliable bytes are the preamble, not frame data).
	pre_buf: Vec<u8>,
	server_protocol: Option<u16>,
	session_token: Option<[u8; 16]>,
	/// We have already said goodbye. Tracked here rather than read off
	/// the transport's status because `NetTransport::close` is allowed to
	/// be a no-op — every exit path may call `disconnect`, so the
	/// idempotence has to hold for all of them, not just the ones whose
	/// transport reports a state change.
	left: bool,
	/// Time spent in `Connecting` / connected-but-no-Welcome, for the
	/// handshake deadlines (sans-IO: time arrives via `poll(dt)`).
	connect_elapsed: f32,
	handshake_elapsed: f32,
	/// Time since ANY inbound traffic, for the post-handshake liveness
	/// backstop (see `RECV_TIMEOUT_SECS`).
	quiet_elapsed: f32,
	/// Refused or timed out: stop emitting further failures.
	failed: bool,
	/// Predictor tick of the last input datagram (send-rate throttle).
	last_input_send_tick: u32,
	/// Staging buffers reused by every send, so steady-state traffic
	/// (reliable frames plus input datagrams, many of each a second)
	/// allocates nothing — allocator churn on the web build's single
	/// thread lands as frame spikes.
	send_buf: Vec<u8>,
	dgram_buf: Vec<u8>,
	stats: StatsAcc,
}

/// Rolling traffic and health counters behind [`NetClient::stats`]. Rates come from a fixed
/// one-second bucket rather than an EWMA: a bytes/second figure a player reads off an overlay
/// should mean the last second, not a blend nobody can invert. RTT is smoothed, because a
/// single sample is dominated by where in the snapshot cadence the ack happened to land.
#[derive(Default)]
struct StatsAcc {
	window: f32,
	in_bytes: u32,
	out_bytes: u32,
	snapshots: u32,
	replays: u32,
	/// Last completed second, held until the next one closes.
	published: digblock_core::metrics::NetStats,
}

/// Weight for the smoothed RTT and its deviation; its reciprocal is the
/// window in acks, which arrive at the snapshot rate.
const RTT_ALPHA: f32 = 0.05;

impl StatsAcc {
	fn on_rtt(&mut self, rtt_ms: f32) {
		let p = &mut self.published;
		p.rtt_last_ms = rtt_ms;
		if p.rtt_ms == 0.0 {
			p.rtt_ms = rtt_ms;
		} else {
			let dev = (rtt_ms - p.rtt_ms).abs();
			p.jitter_ms = p.jitter_ms * (1.0 - RTT_ALPHA) + dev * RTT_ALPHA;
			p.rtt_ms = p.rtt_ms * (1.0 - RTT_ALPHA) + rtt_ms * RTT_ALPHA;
		}
	}

	/// Close the second if it has elapsed, converting counts to rates.
	fn tick(&mut self, dt: f32) {
		self.window += dt;
		if self.window < 1.0 {
			return;
		}
		let per_sec = 1.0 / self.window;
		let p = &mut self.published;
		p.down_kbps = self.in_bytes as f32 * per_sec / 1024.0;
		p.up_kbps = self.out_bytes as f32 * per_sec / 1024.0;
		p.snapshots_per_sec = self.snapshots as f32 * per_sec;
		p.replays_per_sec = self.replays as f32 * per_sec;
		self.window = 0.0;
		self.in_bytes = 0;
		self.out_bytes = 0;
		self.snapshots = 0;
		self.replays = 0;
	}
}

impl NetClient {
	pub fn new(transport: Box<dyn NetTransport>, mut config: NetClientConfig) -> Self {
		config.name = sanitize_name(&config.name);
		Self {
			transport,
			predictor: Predictor::new(0),
			edits: PendingEdits::default(),
			remotes: RemotePlayers::default(),
			my_id: None,
			frames: FrameReader::default(),
			config,
			scratch: Vec::new(),
			pre_buf: Vec::with_capacity(PREAMBLE_LEN),
			server_protocol: None,
			session_token: None,
			left: false,
			connect_elapsed: 0.0,
			handshake_elapsed: 0.0,
			quiet_elapsed: 0.0,
			failed: false,
			last_input_send_tick: 0,
			send_buf: Vec::new(),
			dgram_buf: Vec::new(),
			stats: StatsAcc::default(),
		}
	}

	/// Connection health for the debug overlay and the metrics CSV.
	pub fn stats(&self) -> digblock_core::metrics::NetStats {
		digblock_core::metrics::NetStats {
			pending_edits: self.edits.len() as u32,
			remotes: self.remotes.len() as u32,
			tick_lead: self.predictor.tick_lead(),
			// Read through, not accumulated: the transport already smooths
			// it, and re-smoothing a smoothed value would only add lag.
			wire_rtt_ms: self.transport.wire_rtt_ms().unwrap_or(0.0),
			..self.stats.published
		}
	}

	/// Report the outcome of a reconcile. The ENGINE owns that call — it needs the world borrow
	/// for the replay — so the round trip and the replay count come back here. Takes the OUTCOME
	/// rather than a `replayed` bool: a STALE ack is not a round trip, and `Predictor::reconcile`
	/// returns early before it assigns `last_rtt`, so re-sampling re-blends a measurement already
	/// in the average, halving the EWMA's response on exactly the links a player opens it to see.
	pub fn note_reconciled(&mut self, outcome: Reconciled) {
		if matches!(outcome, Reconciled::Replayed { .. }) {
			self.stats.replays += 1;
		}
		if outcome == Reconciled::StaleAck {
			return;
		}
		if let Some(rtt) = self.predictor.last_rtt() {
			self.stats.on_rtt(rtt * 1000.0);
		}
	}

	/// Handshake finished (Welcome received)?
	pub fn ready(&self) -> bool {
		self.my_id.is_some()
	}

	/// Token issued by the server in `Welcome`, for a future resume.
	pub fn session_token(&self) -> Option<[u8; 16]> {
		self.session_token
	}

	pub fn status(&self) -> ConnStatus {
		self.transport.status()
	}

	/// Drain the transport and demux into engine-shaped events. `dt` is the frame delta; it
	/// drives the connect/handshake deadlines. Reliable bytes are always taken from the transport
	/// in full — leaving them there just moves the queue — but DECODING them is budgeted (see
	/// [`POLL_EVENT_BUDGET`]); whatever does not fit stays in the frame reader.
	pub fn poll(&mut self, out: &mut Vec<NetEvent>, dt: f32) {
		self.check_deadlines(out, dt);
		self.predictor.advance(dt);
		self.stats.tick(dt);
		let mut scratch = std::mem::take(&mut self.scratch);
		scratch.clear();
		self.transport.poll(&mut scratch);
		// Any byte from the peer is proof of life, whatever it decodes to.
		let mut inbound = 0u32;
		for e in scratch.iter() {
			if let TransportEvent::Reliable(b) | TransportEvent::Datagram(b) = e {
				inbound += b.len() as u32;
			}
		}
		if inbound > 0 {
			self.quiet_elapsed = 0.0;
		}
		self.stats.in_bytes += inbound;
		// One budget for the whole call, however many events it spans.
		let limit = out.len() + POLL_EVENT_BUDGET;
		for ev in scratch.drain(..) {
			match ev {
				TransportEvent::Connected => self.send_hello(),
				TransportEvent::Reliable(bytes) => {
					if let Some(bytes) = self.consume_preamble(bytes, out) {
						self.frames.extend(&bytes);
					}
				}
				TransportEvent::Datagram(bytes) => self.on_datagram(&bytes, out),
				TransportEvent::Disconnected { reason } => {
					// Deliver what already arrived BEFORE reporting the end.
					// Decoding moved after this loop when it gained a budget,
					// which silently inverted the two: the engine clears its
					// parked queue on a disconnect, so anything still
					// undecoded was being thrown away rather than applied.
					self.drain_frames(out, limit);
					out.push(NetEvent::Disconnected { reason });
				}
			}
		}
		self.scratch = scratch;
		// Then the leftovers, so a poll that brought no new bytes still
		// drains what the last one could not fit.
		self.drain_frames(out, limit);
	}

	/// Decode buffered reliable frames into events, until `out` reaches
	/// `limit`.
	fn drain_frames(&mut self, out: &mut Vec<NetEvent>, limit: usize) {
		while out.len() < limit {
			// Decode out of the borrowed frame and release it before
			// dispatch: `on_server_msg` needs &mut self, and a join sync's
			// thousands of frames must not each cost a Vec.
			let msg = match self.frames.next_frame_ref() {
				Ok(Some(frame)) => match codec::decode::<ServerMsg>(frame) {
					Ok(msg) => msg,
					Err(e) => {
						log::warn!("bad server frame: {e}");
						continue;
					}
				},
				Ok(None) => break,
				Err(e) => {
					log::warn!("reliable stream corrupt: {e}");
					self.failed = true;
					out.push(NetEvent::Disconnected {
						reason: format!("stream corrupt: {e}"),
					});
					break;
				}
			};
			self.on_server_msg(msg, out);
		}
	}

	fn check_deadlines(&mut self, out: &mut Vec<NetEvent>, dt: f32) {
		if self.failed {
			return;
		}
		if self.ready() {
			// Past the handshake the only deadline is silence. The
			// transport normally reports a death itself; this catches the
			// ones it cannot see.
			self.quiet_elapsed += dt;
			if self.quiet_elapsed > RECV_TIMEOUT_SECS {
				self.failed = true;
				out.push(NetEvent::Disconnected {
					reason: format!("no response from server for {RECV_TIMEOUT_SECS:.0}s"),
				});
			}
			return;
		}
		match self.transport.status() {
			ConnStatus::Connecting => {
				self.connect_elapsed += dt;
				if self.connect_elapsed > CONNECT_TIMEOUT_SECS {
					self.failed = true;
					out.push(NetEvent::Disconnected {
						reason: "connect timeout".into(),
					});
				}
			}
			ConnStatus::Connected => {
				self.handshake_elapsed += dt;
				if self.handshake_elapsed > HANDSHAKE_TIMEOUT_SECS {
					self.failed = true;
					out.push(NetEvent::Disconnected {
						reason: "handshake timeout (incompatible server?)".into(),
					});
				}
			}
			ConnStatus::Closed => {}
		}
	}

	/// The first PREAMBLE_LEN reliable bytes are the server's preamble.
	/// Returns the remaining frame bytes once it has been consumed and
	/// validated, or None while still collecting / after failure.
	fn consume_preamble(&mut self, bytes: Vec<u8>, out: &mut Vec<NetEvent>) -> Option<Vec<u8>> {
		if self.failed {
			return None;
		}
		if self.server_protocol.is_some() {
			return Some(bytes);
		}
		let need = PREAMBLE_LEN - self.pre_buf.len();
		let take = need.min(bytes.len());
		self.pre_buf.extend_from_slice(&bytes[..take]);
		if self.pre_buf.len() < PREAMBLE_LEN {
			return None;
		}
		let pre: [u8; PREAMBLE_LEN] = self.pre_buf[..].try_into().expect("preamble len");
		let Some(version) = decode_preamble(&pre) else {
			self.failed = true;
			out.push(NetEvent::Disconnected {
				reason: "not a DigBlock server (bad preamble)".into(),
			});
			return None;
		};
		self.server_protocol = Some(version);
		if !protocol_supported(version) {
			// Newer servers may still Refuse us politely after this; going
			// through Refused locally keeps one code path either way.
			self.failed = true;
			out.push(NetEvent::Refused {
				reason: format!(
					"server protocol {version}, client supports {MIN_PROTOCOL}..={PROTOCOL_VERSION}"
				),
			});
			return None;
		}
		Some(bytes[take..].to_vec())
	}

	fn send_hello(&mut self) {
		self.send_buf.clear();
		self.send_buf
			.extend_from_slice(&encode_preamble(PROTOCOL_VERSION));
		let hello = ClientMsg::Hello {
			protocol: PROTOCOL_VERSION,
			name: self.config.name.clone(),
			draw_dist: self.config.draw_dist,
			token: self.config.token,
			sim_version: digblock_core::SIM_VERSION,
			build: env!("CARGO_PKG_VERSION").into(),
		};
		if codec::write_frame(&hello, &mut self.send_buf).is_ok() {
			self.transport.send_reliable(&self.send_buf);
		}
		// Before anything else on the stream, so it lands before the first
		// snapshot the server would otherwise aim at a channel this client
		// does not have.
		if !self.transport.datagrams_available() {
			log::info!("no datagram channel — unreliable traffic rides the reliable stream");
			self.send_msg(&ClientMsg::NoDatagrams);
		}
		// Immediately after, on the same ordered stream, so the server has
		// already run the Hello and knows which account is asking. Sent
		// here rather than on `Welcome` because both then land in the same
		// server tick: the claim's corrected inventory follows the join
		// sync in the same outbound batch, instead of a round trip later
		// with an empty bar on screen in between.
		if let Some(legacy) = self.config.legacy_identity {
			self.send_msg(&ClientMsg::ClaimLegacy { legacy });
		}
	}

	/// One encoded [`Datagram`], however it arrived — as a real datagram,
	/// or wrapped in `ServerMsg::Unreliable` because this transport has no
	/// datagram channel. One decode site, so the fallback cannot come to
	/// mean anything different from the thing it stands in for.
	fn on_datagram(&mut self, bytes: &[u8], out: &mut Vec<NetEvent>) {
		match codec::decode_datagram(bytes) {
			Ok(Datagram::Snapshot(snap)) => {
				self.stats.snapshots += 1;
				self.on_snapshot(snap, out);
			}
			Ok(Datagram::Input(_)) => {} // server-bound; ignore
			Err(e) => log::debug!("bad datagram: {e}"),
		}
	}

	fn on_server_msg(&mut self, msg: ServerMsg, out: &mut Vec<NetEvent>) {
		if self.failed {
			return; // refused/timed out: nothing after that counts
		}
		match msg {
			ServerMsg::Refused { reason } => {
				self.failed = true;
				out.push(NetEvent::Refused { reason });
			}
			ServerMsg::Welcome {
				protocol: _, // gated by the preamble before any decode
				seed,
				player_id,
				spawn,
				server_tick,
				snapshot_divisor: _,
				session_token,
				sim_version,
				build,
				tod,
				spawn_look,
				terrain,
			} => {
				// The world names its generator and this build either carries it or does not. A
				// refusal here is the client being too old — the world is intact and somebody
				// else can reach it, which is why the reason says so rather than blaming the seed.
				let Some(terrain) = digblock_core::worldgen::TerrainVersion::from_wire(terrain)
				else {
					self.failed = true;
					out.push(NetEvent::Refused {
						reason: format!(
							"server world uses terrain {terrain} (build {build}); this client does not carry it - update"
						),
					});
					return;
				};
				if !sim_supported(sim_version) {
					self.failed = true;
					out.push(NetEvent::Refused {
						reason: format!(
							"server sim {sim_version} (build {build}), client sim {} - update the older side",
							digblock_core::SIM_VERSION
						),
					});
					return;
				}
				let Some(spawn) = finite_pos(spawn) else {
					self.failed = true;
					out.push(NetEvent::Refused {
						reason: "server sent a non-finite spawn position".into(),
					});
					return;
				};
				self.my_id = Some(player_id);
				self.session_token = Some(session_token);
				self.predictor = Predictor::new(server_tick + CLIENT_TICK_LEAD);
				self.last_input_send_tick = 0;
				out.push(NetEvent::Welcome {
					seed: digblock_core::worldgen::Seed::new(seed, terrain),
					player_id,
					spawn,
					server_tick,
					tod,
					// `normalize_or_zero` already maps a non-finite look to
					// zero, which the engine reads as "keep the current view".
					spawn_look: Vec3::from_array(spawn_look).normalize_or_zero(),
				});
			}
			// A column of the journal as it stands. Not news about any one
			// cell — a restatement, which the server also repeats on a slow
			// rotation as its only way of healing a column a client somehow
			// lacks. So a cell we have a prediction in flight on is left
			// alone: the server's own answer for it is already on its way
			// and settles it either way, while restating the old value here
			// would bounce the block back for a round trip.
			ServerMsg::JournalColumn { cells, .. } => {
				for c in cells {
					let cell = (c.x, c.y, c.z);
					if self.edits.pending_at(cell) {
						continue;
					}
					if let Some(cell_value) = valid_cell(c.block) {
						out.push(NetEvent::Edit {
							cell,
							cell_value,
							remote_author: None,
						});
					}
				}
			}
			ServerMsg::JournalDone { .. } => {}
			// The arrival hold's release. Ordered behind the
			// `JournalColumn`s it reports on, because both ride the
			// reliable stream — so a client that has this has the
			// columns.
			ServerMsg::AreaReady => out.push(NetEvent::AreaReady),
			// A snapshot for a client with no datagram channel; the payload
			// is the datagram's own bytes, so it decodes where every other
			// snapshot does.
			ServerMsg::Unreliable(bytes) => self.on_datagram(&bytes, out),
			ServerMsg::EditApplied {
				by,
				client_seq,
				edit,
				..
			} => {
				// A companion write (`NO_CLIENT_SEQ`) is authored by us but
				// answers no request — confirming it would drop a real
				// pending seq whose verdict is still coming.
				if Some(by) == self.my_id && client_seq != crate::protocol::NO_CLIENT_SEQ {
					self.edits.confirm(client_seq);
				}
				let cell = (edit.x, edit.y, edit.z);
				self.edits.on_authoritative(cell);
				// Unknown/hostile cell from a newer server: not applied;
				// any superseded predictions park until their verdicts.
				if let Some(cell_value) = valid_cell(edit.block) {
					out.push(NetEvent::Edit {
						cell,
						cell_value,
						remote_author: (Some(by) != self.my_id).then_some(by),
					});
				}
			}
			ServerMsg::DigRefused { cell, reason } => {
				out.push(NetEvent::DigRefused {
					cell: (cell[0], cell[1], cell[2]),
					reason,
				});
			}
			ServerMsg::EditRejected { client_seq, reason } => {
				log::info!("edit {client_seq} rejected: {reason:?}");
				for e in self.edits.reject(client_seq) {
					out.push(NetEvent::EditRejected { edit: e, reason });
				}
			}
			// Remote positions come from snapshots; these only carry names.
			ServerMsg::PlayerJoined { id, name } => out.push(NetEvent::PlayerJoined { id, name }),
			ServerMsg::PlayerLeft { id } => {
				self.remotes.remove(id);
				out.push(NetEvent::PlayerLeft { id });
			}
			ServerMsg::Chat { from, text } => out.push(NetEvent::Chat { from, text }),
			ServerMsg::Teleported { pos, look } => {
				// Drop the whole teleport rather than the coordinate: hard-
				// setting the player to a partly-garbage position is worse
				// than ignoring a message an honest server never sends.
				let Some(pos) = finite_pos(pos) else {
					log::warn!("ignoring teleport to a non-finite position");
					return;
				};
				self.predictor.on_teleport();
				out.push(NetEvent::Teleported {
					pos,
					look: Vec3::from_array(look).normalize_or_zero(),
				});
			}
			ServerMsg::TimeOfDay { tod } => out.push(NetEvent::TimeOfDay { tod }),
			ServerMsg::Admin(reply) => out.push(NetEvent::AdminReply(reply)),
			ServerMsg::AdminCommands { commands } => {
				out.push(NetEvent::AdminCommands { commands });
			}
			ServerMsg::Waystones { entries } => out.push(NetEvent::Waystones { entries }),
			ServerMsg::CastRefused { reason } => out.push(NetEvent::CastRefused { reason }),
			ServerMsg::Kindled { cell, secs_left } => {
				out.push(NetEvent::Kindled { cell, secs_left });
			}
			ServerMsg::SphereWinding {
				by,
				head,
				radius,
				span,
				block,
			} => out.push(NetEvent::SphereWinding {
				by,
				head,
				radius,
				span,
				block,
			}),
			ServerMsg::SphereEnded {
				by,
				head,
				radius,
				landed,
				sealed,
				block,
			} => out.push(NetEvent::SphereEnded {
				by,
				head,
				radius,
				landed,
				sealed,
				block,
			}),
			ServerMsg::InventoryFull { counts } => {
				// Raw pairs through to the engine — `Inventory::from_wire`
				// does the strict decode (unknown ids dropped, flagged
				// sub-material families handled). The pending deltas are
				// captured HERE, with the counts they have to be summed
				// against; see `NetEvent::Inventory`.
				let pending = self.edits.iter().filter_map(|e| e.inv_delta).collect();
				out.push(NetEvent::Inventory { counts, pending });
			}
			ServerMsg::Layout { hotbar, grid } => {
				out.push(NetEvent::Layout { hotbar, grid });
			}
			ServerMsg::Skills { xp } => out.push(NetEvent::Skills { xp }),
			ServerMsg::XpGain { skill, total } => out.push(NetEvent::XpGain { skill, total }),
			ServerMsg::LevelUp {
				player,
				skill,
				level,
			} => out.push(NetEvent::LevelUp {
				player,
				skill,
				level,
			}),
		}
	}

	fn on_snapshot(&mut self, snap: crate::messages::Snapshot, out: &mut Vec<NetEvent>) {
		// A non-finite entry is dropped, not clamped. Reconcile would take
		// the replay path on it (NaN fails every comparison), set the
		// player's state to NaN, and the next aim ray would spin in the DDA
		// loop forever — `t > max_dist` is its only exit and NaN never
		// satisfies it. Remote entries feed the interpolator, which would
		// smear the NaN across every later sample. (Remote velocity is
		// quantized and cannot be non-finite; position still can.)
		if let Some(me) = snap.me {
			match (finite_pos(me.pos), finite_pos(me.vel)) {
				(Some(pos), Some(vel)) => out.push(NetEvent::Ack {
					acked_input_tick: snap.your_last_input_tick,
					me: digblock_core::player::PlayerState { pos, vel },
				}),
				_ => log::debug!("dropping non-finite self state from snapshot"),
			}
		}
		for p in &snap.others {
			let Some(pos) = finite_pos(p.pos) else {
				log::debug!("dropping non-finite snapshot entry for player {}", p.id);
				continue;
			};
			self.remotes.push(
				p.id,
				RemoteSnap {
					server_tick: snap.server_tick,
					pos,
					vel: Vec3::new(
						crate::messages::vel_from_wire(p.vel[0]),
						crate::messages::vel_from_wire(p.vel[1]),
						crate::messages::vel_from_wire(p.vel[2]),
					),
					yaw: p.yaw,
					held: p.held,
					fx: p.fx,
					mastery: p.mastery,
				},
			);
		}
	}

	/// One datagram carrying the current input redundancy window. Called
	/// every frame; actually sends at most once per INPUT_SEND_STRIDE
	/// predicted ticks — a high-refresh display no longer transmits one per
	/// frame, most of them carrying an unchanged window.
	pub fn send_inputs(&mut self) {
		if !self.ready() {
			return;
		}
		let cur = self.predictor.current_tick();
		if cur.wrapping_sub(self.last_input_send_tick) < INPUT_SEND_STRIDE {
			return;
		}
		let batch = self.predictor.input_batch();
		if batch.inputs.is_empty() {
			self.predictor.recycle_batch(batch);
			return;
		}
		let dg = Datagram::Input(batch);
		match codec::encode_datagram_into(&dg, &mut self.dgram_buf) {
			Ok(bytes) => {
				self.stats.out_bytes += bytes.len() as u32;
				// The same payload either way — the fallback wraps it, it
				// never re-encodes it, so there is no second shape of an
				// input batch anywhere for the two paths to disagree about.
				if self.transport.datagrams_available() {
					self.transport.send_datagram(bytes);
				} else {
					let wrapped = ClientMsg::Unreliable(bytes.to_vec());
					self.send_msg(&wrapped);
				}
				self.last_input_send_tick = cur;
			}
			Err(e) => log::warn!("input datagram: {e}"),
		}
		// Unwrap the batch back out and return its Vec to the predictor —
		// serializing needed the owned `Datagram`, not a copy.
		if let Datagram::Input(batch) = dg {
			self.predictor.recycle_batch(batch);
		}
	}

	#[allow(clippy::too_many_arguments)]
	pub fn send_edit(
		&mut self,
		seq: u32,
		tick: u32,
		origin: Vec3,
		dir: Vec3,
		kind: EditKind,
		cell: (i32, i32, i32),
		normal: (i32, i32, i32),
		slot: u8,
	) {
		self.send_msg(&ClientMsg::EditRequest {
			seq,
			tick,
			origin: origin.to_array(),
			dir: dir.to_array(),
			kind,
			cell: [cell.0, cell.1, cell.2],
			normal: [normal.0 as i8, normal.1 as i8, normal.2 as i8],
			slot,
		});
	}

	/// Announce a hold-to-dig beginning on `cell` (reliable, in-order
	/// with the completing `send_edit` — the server times the interval).
	/// `slot` is the hand it is being made with, declared here for the
	/// same reason `send_edit` declares one.
	pub fn send_dig_start(&mut self, cell: (i32, i32, i32), slot: u8) {
		self.send_msg(&ClientMsg::DigStart {
			cell: [cell.0, cell.1, cell.2],
			slot,
		});
	}

	/// The hold was released or retargeted before completing.
	pub fn send_dig_cancel(&mut self) {
		self.send_msg(&ClientMsg::DigCancel);
	}

	/// Ask the server to teleport us back to spawn. The engine gates on
	/// the shared cooldown; the server enforces it authoritatively.
	pub fn send_return_to_spawn(&mut self) {
		self.send_msg(&ClientMsg::ReturnToSpawn);
	}

	/// A completed channel asks for its spell — the server validates the
	/// unlock and the price, which are the real gates.
	pub fn send_cast(&mut self, kind: crate::messages::CastKind) {
		self.send_msg(&ClientMsg::Cast(kind));
	}

	/// The Sphere channel broke — the server unwinds the climb.
	pub fn send_cast_break(&mut self) {
		self.send_msg(&ClientMsg::CastBreak);
	}

	/// Take the causeway up before its span runs out. No refund — the
	/// fare bought the span, not the seconds spent.
	pub fn send_causeway_end(&mut self) {
		self.send_msg(&ClientMsg::CausewayEnd);
	}

	/// Tell the server the draw distance changed (settings slider), so
	/// the edit/player sync radius follows without a reconnect.
	pub fn send_set_draw_dist(&mut self, draw_dist: i32) {
		self.config.draw_dist = draw_dist;
		self.send_msg(&ClientMsg::SetDrawDist { draw_dist });
	}

	/// Send a chat line. The server sanitizes and rate-limits; the local
	/// truncation only keeps a pathological buffer off the wire.
	pub fn send_chat(&mut self, text: &str) {
		let text: String = text.chars().take(crate::protocol::MAX_CHAT_LEN).collect();
		if !text.trim().is_empty() {
			self.send_msg(&ClientMsg::Chat { text });
		}
	}

	/// Send an alchemy op (convert/craft). Not client-predicted: the
	/// server answers success or failure with an `InventoryFull` rebase.
	pub fn send_alchemy(&mut self, op: crate::messages::AlchemyOp) {
		self.send_msg(&ClientMsg::Alchemy { op });
	}

	/// Send the item layout (hotbar + grid wire arrangements) after any
	/// change — reliable and in-order with edits, so a bar assignment is
	/// on the server before the next place validates against it.
	pub fn send_layout(&mut self, hotbar: &[u16], grid: &[u16]) {
		self.send_msg(&ClientMsg::Layout {
			hotbar: hotbar.to_vec(),
			grid: grid.to_vec(),
		});
	}

	/// Say goodbye, and give it a moment to leave the machine. Call this on every path where the
	/// player is knowingly going: without a `Bye` the server waits out QUIC's idle timeout, and
	/// with one account holding one live connection the player being held is the one trying to
	/// get back in. The pairing is why this is ONE method — `Bye` alone only reaches a stream
	/// buffer, and dropping the transport a line later takes it with it. Best effort throughout.
	pub fn disconnect(&mut self) {
		if self.left || self.transport.status() == crate::transport::ConnStatus::Closed {
			return;
		}
		self.left = true;
		self.send_msg(&ClientMsg::Bye);
		self.transport.close();
	}

	fn send_msg(&mut self, msg: &ClientMsg) {
		self.send_buf.clear();
		if codec::write_frame(msg, &mut self.send_buf).is_ok() {
			self.stats.out_bytes += self.send_buf.len() as u32;
			self.transport.send_reliable(&self.send_buf);
		}
	}
}

/// Strict wire-cell validation: known id, spare bits clear, and a state
/// the id's shape defines. Hostile or newer-server cells decode to None
/// and degrade to inventory-revert-only handling.
fn valid_cell(v: u16) -> Option<u16> {
	digblock_core::shapes::state_valid(v).then_some(v)
}

/// The float half of the same contract: no NaN and no infinity crosses the wire boundary into
/// the engine. The server has always checked this on ITS inputs (`sim/edits.rs`); the client
/// had no counterpart, and every consumer of a wire position fed it straight to `camera.pos`.
/// One NaN there hangs the process: `raycast`'s DDA exits only on `t > max_dist`, and with NaN
/// every comparison is false, so the loop steps a cell at a time forever.
fn finite_pos(v: [f32; 3]) -> Option<Vec3> {
	let v = Vec3::from_array(v);
	v.is_finite().then_some(v)
}
