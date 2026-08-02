//! The authoritative simulation: one instance, one thread, fixed 60 Hz.
//! Socket-free by construction — the net layer (net.rs) feeds it
//! [`SimEvent`]s and it emits pre-encoded [`OutMsg`]s onto per-client
//! channels, so every behavior here is testable with plain `cargo test`.
//!
//! Movement model: full server physics with an adoption envelope. Each
//! client input carries its claimed post-tick state; after simulating the
//! same tick the server adopts the claim when it is close (and sane),
//! re-zeroing honest drift so clients never rubber-band. Out-of-envelope
//! claims keep the server's own state and the next snapshot corrects the
//! client.

mod edits;
mod movement;
mod sync;

#[cfg(test)]
pub(crate) mod harness;

use crate::journal::Journal;
use crate::store::{ChunkStore, ColumnSource};
use digblock_core::fluid::FluidSim;
use digblock_core::player::{self, Player, PlayerIntent};
use digblock_net::codec;
use digblock_net::messages::{ClientMsg, InputBatch, InputTick, RejectReason, ServerMsg};
use digblock_net::protocol::{
	sanitize_name, PlayerId, EDIT_BURST, MIN_PROTOCOL, PROTOCOL_VERSION, SERVER_ID,
	SNAPSHOT_DIVISOR, TICK_RATE,
};
use edits::PendingEditReq;
use glam::Vec3;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::Path;

/// Outbound per-client queue. BOUNDED: a peer that stops draining is a
/// dead peer, not an unbounded heap — `try_send` failure marks the client
/// dead and the connection drops, instead of the sim buffering forever.
pub type OutTx = tokio::sync::mpsc::Sender<OutMsg>;

/// Outbound queue depth per client. Sized so a join sync of a large world
/// fits (one frame per edited column) while a stalled reader still trips
/// quickly.
pub const OUT_CAP: usize = 512;

/// A client whose datagram queue stays full this long is dead (reliable
/// frames kill instantly; snapshots are lossy so brief stalls are fine).
const DGRAM_STALL_SECS: u32 = 5;

/// A connection that passes the preamble but never completes Hello is
/// dropped after this many ticks (5s — mirrors the preamble deadline).
/// Without it a stalled handshake camps in the lobby forever: the
/// server's own keep-alives defeat QUIC's idle timeout, and the 255-
/// player cap applies only post-Hello.
const HELLO_TIMEOUT_TICKS: u32 = 5 * TICK_RATE;

pub enum SimEvent {
	Connected { conn_id: u64, out: OutTx },
	Msg { conn_id: u64, msg: ClientMsg },
	Inputs { conn_id: u64, batch: InputBatch },
	Disconnected { conn_id: u64 },
}

#[derive(Debug)]
pub enum OutMsg {
	Datagram(Vec<u8>),
	Frame(Vec<u8>),
	/// Ask the net layer to close the connection (refused handshake).
	Close,
}

struct Client {
	id: PlayerId,
	name: String,
	out: OutTx,
	/// Set when the outbound queue rejects a reliable frame (or stays
	/// full for datagrams) — reaped into `drop_client` next tick. `Cell`
	/// so shared-borrow broadcast loops can flag it.
	dead: Cell<bool>,
	/// Tick the datagram queue first reported Full, cleared on success.
	dgram_full_since: Cell<Option<u32>>,
	player: Player,
	look: Vec3,
	last_intent: PlayerIntent,
	inputs: BTreeMap<u32, InputTick>,
	last_input_tick: u32,
	/// EWMA of adopted (claim − sim) deltas: the sustained-drift gate.
	drift: Vec3,
	edit_tokens: f32,
	draw_dist: i32,
	synced_columns: FxHashSet<(i32, i32)>,
	/// Run the full edited-column sync scan for this client next tick.
	/// Set on center change and on going live; steady state does ZERO
	/// journal scans (fresh columns arrive via the first-edit broadcast).
	needs_sync_scan: bool,
	live: bool,
	center: (i32, i32),
}

pub struct Sim {
	tick: u32,
	store: ChunkStore,
	journal: Journal,
	fluid: FluidSim,
	flow_accum: f32,
	sim_radius: i32,
	spawn: Vec3,
	clients: FxHashMap<u64, Client>,
	/// Connections that have not completed the Hello handshake, with the
	/// tick they arrived (reaped after HELLO_TIMEOUT_TICKS).
	lobby: FxHashMap<u64, (OutTx, u32)>,
	edit_queue: Vec<PendingEditReq>,
	residency_dirty: bool,
	/// (newly-ready column, resident neighbor) pairs awaiting a budgeted
	/// journal-water rescan, so fluid fronts stalled at a load boundary
	/// resume when the far side arrives.
	face_scans: std::collections::VecDeque<((i32, i32), (i32, i32))>,
	/// Last tick a snapshot-encode failure was logged (rate limit).
	last_oversize_log: Option<u32>,
}

impl Sim {
	pub fn new(
		seed: f64,
		data_dir: &Path,
		sim_radius: i32,
		workers: usize,
	) -> std::io::Result<Self> {
		let store = ChunkStore::new(seed, workers);
		Self::with_store(seed, data_dir, sim_radius, store, player::spawn(seed))
	}

	/// Test seam: inject a synthetic column source (and a spawn that fits
	/// it) so sim tests never pay real worldgen.
	#[doc(hidden)]
	pub fn new_with_source(
		seed: f64,
		data_dir: &Path,
		sim_radius: i32,
		workers: usize,
		source: ColumnSource,
		spawn: Vec3,
	) -> std::io::Result<Self> {
		let store = ChunkStore::new_with_source(seed, workers, source);
		Self::with_store(seed, data_dir, sim_radius, store, spawn)
	}

	fn with_store(
		seed: f64,
		data_dir: &Path,
		sim_radius: i32,
		store: ChunkStore,
		spawn: Vec3,
	) -> std::io::Result<Self> {
		let journal = Journal::open(data_dir, seed)?;
		Ok(Self {
			tick: 0,
			store,
			journal,
			fluid: FluidSim::default(),
			flow_accum: 0.0,
			sim_radius,
			spawn,
			clients: FxHashMap::default(),
			lobby: FxHashMap::default(),
			edit_queue: Vec::new(),
			residency_dirty: true,
			face_scans: std::collections::VecDeque::new(),
			last_oversize_log: None,
		})
	}

	pub fn server_tick(&self) -> u32 {
		self.tick
	}

	/// Test seam: production callers read nothing off Sim but
	/// `server_tick` (lib.rs status line).
	#[cfg(test)]
	pub fn player_count(&self) -> usize {
		self.clients.len()
	}

	/// Run one 60 Hz tick with the events drained since the last one.
	pub fn tick_once(&mut self, events: Vec<SimEvent>) {
		self.reap_dead();
		for ev in events {
			self.handle_event(ev);
		}
		self.update_residency();
		self.step_physics();
		self.process_edits();
		self.step_fluid();
		self.tick += 1;
		if self.tick % SNAPSHOT_DIVISOR == 0 {
			self.send_snapshots();
		}
		self.journal.maintain();
	}

	/// Flush and fsync the journal (graceful shutdown).
	pub fn shutdown(&mut self) {
		self.journal.sync();
	}

	#[cfg(test)]
	fn test_block_at(&self, x: i32, y: i32, z: i32) -> Option<digblock_core::block::BlockType> {
		self.store.block_at(x, y, z)
	}

	#[cfg(test)]
	fn test_player(&self, conn_id: u64) -> Option<digblock_core::player::PlayerState> {
		self.clients.get(&conn_id).map(|c| c.player.state())
	}

	#[cfg(test)]
	fn test_is_live(&self, conn_id: u64) -> bool {
		self.clients.get(&conn_id).is_some_and(|c| c.live)
	}

	#[cfg(test)]
	fn test_last_input_tick(&self, conn_id: u64) -> u32 {
		self.clients.get(&conn_id).map_or(0, |c| c.last_input_tick)
	}

	// ---- events -------------------------------------------------------

	fn handle_event(&mut self, ev: SimEvent) {
		match ev {
			SimEvent::Connected { conn_id, out } => {
				self.lobby.insert(conn_id, (out, self.tick));
			}
			SimEvent::Msg { conn_id, msg } => match msg {
				ClientMsg::Hello {
					protocol,
					name,
					draw_dist,
					token: _, // future reconnect-with-state
					sim_version,
					build,
				} => self.handle_hello(conn_id, protocol, name, draw_dist, sim_version, build),
				ClientMsg::EditRequest {
					seq,
					tick: _,
					origin,
					dir,
					kind,
					cell,
					normal,
				} => {
					if let Some(client) = self.clients.get(&conn_id) {
						// Bounded: a flood beyond every client's burst
						// budget is rejected up front, not accumulated.
						let cap = self.clients.len() * EDIT_BURST as usize + 64;
						if self.edit_queue.len() >= cap {
							client_send(
								client,
								&ServerMsg::EditRejected {
									client_seq: seq,
									reason: RejectReason::RateLimited,
								},
							);
						} else {
							self.edit_queue.push(PendingEditReq {
								conn_id,
								seq,
								origin: Vec3::from_array(origin),
								dir: Vec3::from_array(dir),
								kind,
								cell: (cell[0], cell[1], cell[2]),
								normal: (normal[0] as i32, normal[1] as i32, normal[2] as i32),
							});
						}
					}
				}
				ClientMsg::Bye => self.drop_client(conn_id),
			},
			SimEvent::Inputs { conn_id, batch } => self.handle_inputs(conn_id, batch),
			SimEvent::Disconnected { conn_id } => self.drop_client(conn_id),
		}
	}

	fn handle_hello(
		&mut self,
		conn_id: u64,
		protocol: u16,
		name: String,
		draw_dist: i32,
		sim_version: u16,
		build: String,
	) {
		let Some((out, _)) = self.lobby.remove(&conn_id) else {
			return;
		};
		let refuse = |out: &OutTx, reason: String| {
			send_frame(&out.clone(), &ServerMsg::Refused { reason });
			let _ = out.try_send(OutMsg::Close);
		};
		// Redundant with the net-layer preamble gate, but the sim is also
		// driven directly (tests, future transports) — keep it authoritative.
		if !(MIN_PROTOCOL..=PROTOCOL_VERSION).contains(&protocol) {
			refuse(
				&out,
				format!("protocol {protocol}, server supports {MIN_PROTOCOL}..={PROTOCOL_VERSION}"),
			);
			return;
		}
		if sim_version != digblock_core::SIM_VERSION {
			refuse(
				&out,
				format!(
					"client sim {sim_version} (build {build}), server sim {} — update the older side",
					digblock_core::SIM_VERSION
				),
			);
			return;
		}
		let Some(id) = self.free_id() else {
			refuse(&out, "server full".into());
			return;
		};
		let name = sanitize_name(&name);
		let draw_dist = draw_dist.clamp(2, 32);
		let mut ok = send_frame(
			&out,
			&ServerMsg::Welcome {
				protocol: PROTOCOL_VERSION,
				seed: self.store.seed(),
				player_id: id,
				spawn: self.spawn.to_array(),
				server_tick: self.tick,
				snapshot_divisor: SNAPSHOT_DIVISOR,
				// Issued now so resume can be added without a wire change;
				// server-side session storage arrives with that feature.
				session_token: rand::random(),
				sim_version: digblock_core::SIM_VERSION,
				build: env!("CARGO_PKG_VERSION").into(),
			},
		);

		// Join sync: final cell state for every edited column in range.
		let center = column_of(self.spawn);
		let mut synced = FxHashSet::default();
		let columns: Vec<(i32, i32)> = self.journal.index.edited_columns().collect();
		let mut synced_count = 0usize;
		for (cx, cz) in columns {
			if ok && in_radius((cx, cz), center, draw_dist) {
				ok &= self.send_journal_column(&out, cx, cz);
				synced.insert((cx, cz));
				synced_count += 1;
			}
		}
		ok &= send_frame(
			&out,
			&ServerMsg::JournalDone {
				through_seq: self.journal.index.next_seq,
			},
		);
		if !ok {
			// A joiner that can't even drain its join sync is not viable.
			// OUT_CAP should comfortably fit any real world's column count
			// (journal compaction keeps this rare) — log enough to tell.
			log::warn!(
				"conn {conn_id}: join sync overflowed the outbound queue \
				 ({synced_count} columns, cap {OUT_CAP}) — dropping"
			);
			return;
		}

		// Roster for the joiner (who's already online), then the join
		// announcement to everyone else — same message both directions.
		for c in self.clients.values() {
			send_frame(
				&out,
				&ServerMsg::PlayerJoined {
					id: c.id,
					name: c.name.clone(),
				},
			);
			client_send(
				c,
				&ServerMsg::PlayerJoined {
					id,
					name: name.clone(),
				},
			);
		}
		log::info!("player {id} ({name}) joined at tick {}", self.tick);
		self.clients.insert(
			conn_id,
			Client {
				id,
				name,
				out,
				dead: Cell::new(false),
				dgram_full_since: Cell::new(None),
				player: Player::new(self.spawn),
				look: Vec3::Z,
				last_intent: PlayerIntent::default(),
				inputs: BTreeMap::new(),
				last_input_tick: 0,
				drift: Vec3::ZERO,
				edit_tokens: EDIT_BURST,
				draw_dist,
				synced_columns: synced,
				needs_sync_scan: false,
				live: false,
				center,
			},
		);
		self.residency_dirty = true;
	}

	/// Clients flagged dead by a refused queue push get a real disconnect;
	/// lobby members that never completed Hello time out.
	fn reap_dead(&mut self) {
		let dead: Vec<u64> = self
			.clients
			.iter()
			.filter(|(_, c)| c.dead.get())
			.map(|(&id, _)| id)
			.collect();
		for conn_id in dead {
			// Dropping the Client drops the last Sender: the net layer's
			// recv ends and the connection closes.
			self.drop_client(conn_id);
		}
		let tick = self.tick;
		self.lobby.retain(|conn_id, (out, since)| {
			let keep = tick.wrapping_sub(*since) < HELLO_TIMEOUT_TICKS;
			if !keep {
				log::info!("conn {conn_id}: no Hello within the deadline — dropping");
				// Close ends the net task; dropping the Sender is the
				// backstop if the queue is already full.
				let _ = out.try_send(OutMsg::Close);
			}
			keep
		});
	}

	fn drop_client(&mut self, conn_id: u64) {
		if let Some(client) = self.clients.remove(&conn_id) {
			log::info!("player {} ({}) left", client.id, client.name);
			for c in self.clients.values() {
				client_send(c, &ServerMsg::PlayerLeft { id: client.id });
			}
			self.residency_dirty = true;
		}
		self.lobby.remove(&conn_id);
	}

	fn free_id(&self) -> Option<PlayerId> {
		let used: FxHashSet<PlayerId> = self.clients.values().map(|c| c.id).collect();
		(0..SERVER_ID).find(|id| !used.contains(id))
	}
}
fn column_of(pos: Vec3) -> (i32, i32) {
	(
		(pos.x.floor() as i32).div_euclid(16),
		(pos.z.floor() as i32).div_euclid(16),
	)
}

fn in_radius(col: (i32, i32), center: (i32, i32), r: i32) -> bool {
	let (dx, dz) = (col.0 - center.0, col.1 - center.1);
	dx * dx + dz * dz <= r * r
}

/// Encode + queue one reliable frame. `false` = the bounded queue refused
/// (full or closed): the peer is not draining and must be dropped.
fn send_frame(out: &OutTx, msg: &ServerMsg) -> bool {
	let mut buf = Vec::new();
	match codec::write_frame(msg, &mut buf) {
		Ok(()) => out.try_send(OutMsg::Frame(buf)).is_ok(),
		Err(e) => {
			// Encode failure is a server bug, not a peer problem.
			log::error!("frame encode: {e}");
			true
		}
	}
}

/// Reliable send to a joined client; queue refusal marks it dead (reaped
/// into `drop_client` at the next tick).
fn client_send(client: &Client, msg: &ServerMsg) {
	if !client.dead.get() && !send_frame(&client.out, msg) {
		log::info!(
			"player {} ({}): outbound queue refused a frame — dropping",
			client.id,
			client.name
		);
		client.dead.set(true);
	}
}

#[cfg(test)]
mod tests {
	use super::harness::*;
	use super::{
		ClientMsg, OutMsg, RejectReason, ServerMsg, SimEvent, MIN_PROTOCOL, PROTOCOL_VERSION,
	};
	use digblock_net::codec::FrameReader;
	use tokio::sync::mpsc::channel;

	#[test]
	fn version_mismatch_refused_and_closed() {
		let refused_with = |protocol: u16, sim_version: u16| -> (Vec<ServerMsg>, bool) {
			let mut sim = flat_sim("refuse");
			let (tx, rx) = channel(super::OUT_CAP);
			sim.tick_once(vec![
				SimEvent::Connected {
					conn_id: 9,
					out: tx,
				},
				SimEvent::Msg {
					conn_id: 9,
					msg: ClientMsg::Hello {
						protocol,
						name: "old".into(),
						draw_dist: 4,
						token: [0; 16],
						sim_version,
						build: "old-build".into(),
					},
				},
			]);
			let mut c = TestClient {
				rx,
				frames: FrameReader::default(),
				msgs: Vec::new(),
				snaps: Vec::new(),
			};
			let mut closed = false;
			while let Ok(out) = c.rx.try_recv() {
				match out {
					OutMsg::Frame(bytes) => {
						c.frames.extend(&bytes);
						while let Some(f) = c.frames.next_frame().unwrap() {
							c.msgs.push(digblock_net::codec::decode(&f).unwrap());
						}
					}
					OutMsg::Close => closed = true,
					OutMsg::Datagram(_) => {}
				}
			}
			(c.msgs, closed)
		};

		// Old protocol → Refused naming the supported range, then Close.
		let (msgs, closed) = refused_with(MIN_PROTOCOL - 1, digblock_core::SIM_VERSION);
		assert!(
			matches!(&msgs[..], [ServerMsg::Refused { reason }] if reason.contains("protocol")),
			"{msgs:?}"
		);
		assert!(closed);

		// Same protocol, older sim behavior → Refused naming both sims.
		let (msgs, closed) = refused_with(PROTOCOL_VERSION, digblock_core::SIM_VERSION + 1);
		assert!(
			matches!(&msgs[..], [ServerMsg::Refused { reason }] if reason.contains("sim")),
			"{msgs:?}"
		);
		assert!(closed);
	}

	#[test]
	fn lobby_without_hello_times_out() {
		let mut sim = flat_sim("lobby-timeout");
		let (tx, mut rx) = channel(super::OUT_CAP);
		sim.tick_once(vec![SimEvent::Connected {
			conn_id: 7,
			out: tx,
		}]);
		// One tick short of the deadline: still in the lobby, no Close.
		for _ in 0..super::HELLO_TIMEOUT_TICKS - 1 {
			sim.tick_once(vec![]);
		}
		assert!(matches!(
			rx.try_recv(),
			Err(tokio::sync::mpsc::error::TryRecvError::Empty)
		));
		// Crossing it: reaped with a Close so the net task ends.
		sim.tick_once(vec![]);
		let mut closed = false;
		while let Ok(out) = rx.try_recv() {
			closed |= matches!(out, OutMsg::Close);
		}
		assert!(closed, "lobby member closed after the Hello deadline");
		// A late Hello on the reaped connection is simply ignored.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 7,
			msg: ClientMsg::Hello {
				protocol: PROTOCOL_VERSION,
				name: "late".into(),
				draw_dist: 4,
				token: [0; 16],
				sim_version: digblock_core::SIM_VERSION,
				build: "late-build".into(),
			},
		}]);
		assert_eq!(sim.player_count(), 0);
	}

	#[test]
	fn closed_outbound_queue_reaps_client_and_frees_id() {
		let mut sim = flat_sim("reap");
		let a = connect(&mut sim, 1); // id 0
		let mut b = connect(&mut sim, 2); // id 1
		drop(a); // client 1's receiver gone: next push fails Closed

		// Any broadcast to 1 flags it dead; the next tick reaps it.
		let _c = connect(&mut sim, 3); // join announcement hits client 1
		sim.tick_once(vec![]);
		assert_eq!(sim.player_count(), 2, "dead client reaped");
		b.drain();
		assert!(
			b.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::PlayerLeft { id: 0 })),
			"others heard the forced leave: {:?}",
			b.msgs
		);

		// Its id is reusable by a fresh join.
		let mut d = connect(&mut sim, 4);
		d.drain();
		let welcome_id = d.msgs.iter().find_map(|m| match m {
			ServerMsg::Welcome { player_id, .. } => Some(*player_id),
			_ => None,
		});
		assert_eq!(welcome_id, Some(0), "freed id reused");
	}

	#[test]
	fn stalled_datagram_queue_escalates_to_drop() {
		let mut sim = flat_sim("stall");
		let c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		// Never drain: snapshots fill the bounded queue, then the stall
		// clock runs. OUT_CAP fills after ~3*OUT_CAP ticks of snapshots,
		// then DGRAM_STALL_SECS of Full escalates to dead.
		let enough = 3 * super::OUT_CAP as u32 + (super::DGRAM_STALL_SECS + 2) * 60;
		for _ in 0..enough {
			sim.tick_once(vec![]);
			if sim.player_count() == 0 {
				break;
			}
		}
		assert_eq!(sim.player_count(), 0, "stalled reader dropped");
		drop(c);
	}

	#[test]
	fn edit_request_flood_is_bounded_and_rejected() {
		let mut sim = flat_sim("flood");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let flood: Vec<SimEvent> = (0..1000)
			.map(|seq| SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq,
					tick: 0,
					origin: [0.0; 3],
					dir: [0.0, -1.0, 0.0],
					kind: digblock_net::messages::EditKind::Dig,
					cell: [0, 0, 0],
					normal: [0, 0, 0],
				},
			})
			.collect();
		sim.tick_once(flood);
		assert!(
			sim.edit_queue.is_empty(),
			"queue fully drained after the tick"
		);
		c.drain();
		let rejected = c
			.msgs
			.iter()
			.filter(|m| {
				matches!(
					m,
					ServerMsg::EditRejected {
						reason: RejectReason::RateLimited,
						..
					}
				)
			})
			.count();
		assert!(rejected > 0, "flood produced rate-limit rejections");
	}

	#[test]
	fn hostile_names_are_sanitized() {
		let mut sim = flat_sim("names");
		let (tx, _rx) = channel(super::OUT_CAP);
		let long_name = "x".repeat(1000) + "\n\u{7}";
		sim.tick_once(vec![
			SimEvent::Connected {
				conn_id: 5,
				out: tx,
			},
			SimEvent::Msg {
				conn_id: 5,
				msg: ClientMsg::Hello {
					protocol: PROTOCOL_VERSION,
					name: long_name,
					draw_dist: 4,
					token: [0; 16],
					sim_version: digblock_core::SIM_VERSION,
					build: "t".into(),
				},
			},
		]);
		let mut b = connect(&mut sim, 6);
		b.drain();
		let roster_name = b
			.msgs
			.iter()
			.find_map(|m| match m {
				ServerMsg::PlayerJoined { id: 0, name } => Some(name.clone()),
				_ => None,
			})
			.expect("roster entry for the hostile name");
		assert_eq!(roster_name.len(), digblock_net::protocol::MAX_NAME_LEN);
		assert!(roster_name.chars().all(|c| !c.is_control()));
	}

	#[test]
	fn join_roster_and_announcements() {
		let mut sim = flat_sim("roster");
		let mut a = connect(&mut sim, 1); // id 0
		let b = connect(&mut sim, 2); // id 1

		// The joiner gets a roster entry for the player already online,
		// and never an announcement about itself.
		assert!(
			b.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::PlayerJoined { id: 0, name } if name.as_str() == "t1"
			)),
			"joiner missing roster entry: {:?}",
			b.msgs
		);
		assert!(!b
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::PlayerJoined { id: 1, .. })));

		// The player already online hears the join...
		a.drain();
		assert!(a.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::PlayerJoined { id: 1, name } if name.as_str() == "t2"
		)));

		// ...and the leave.
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 2 }]);
		a.drain();
		assert!(a
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::PlayerLeft { id: 1 })));
	}
}
