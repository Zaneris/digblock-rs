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
use digblock_net::messages::{ClientMsg, InputBatch, InputTick, ServerMsg};
use digblock_net::protocol::{PlayerId, EDIT_BURST, PROTOCOL_VERSION, SERVER_ID, SNAPSHOT_DIVISOR};
use edits::PendingEditReq;
use glam::Vec3;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::path::Path;
use tokio::sync::mpsc::UnboundedSender;

pub enum SimEvent {
	Connected {
		conn_id: u64,
		out: UnboundedSender<OutMsg>,
	},
	Msg {
		conn_id: u64,
		msg: ClientMsg,
	},
	Inputs {
		conn_id: u64,
		batch: InputBatch,
	},
	Disconnected {
		conn_id: u64,
	},
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
	out: UnboundedSender<OutMsg>,
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
	/// Connections that have not completed the Hello handshake.
	lobby: FxHashMap<u64, UnboundedSender<OutMsg>>,
	edit_queue: Vec<PendingEditReq>,
	residency_dirty: bool,
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
			last_oversize_log: None,
		})
	}

	pub fn server_tick(&self) -> u32 {
		self.tick
	}

	pub fn player_count(&self) -> usize {
		self.clients.len()
	}

	/// Run one 60 Hz tick with the events drained since the last one.
	pub fn tick_once(&mut self, events: Vec<SimEvent>) {
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
				self.lobby.insert(conn_id, out);
			}
			SimEvent::Msg { conn_id, msg } => match msg {
				ClientMsg::Hello {
					protocol,
					name,
					draw_dist,
				} => self.handle_hello(conn_id, protocol, name, draw_dist),
				ClientMsg::EditRequest {
					seq,
					tick: _,
					origin,
					dir,
					kind,
					cell,
					normal,
				} => {
					if self.clients.contains_key(&conn_id) {
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
				ClientMsg::Bye => self.drop_client(conn_id),
			},
			SimEvent::Inputs { conn_id, batch } => self.handle_inputs(conn_id, batch),
			SimEvent::Disconnected { conn_id } => self.drop_client(conn_id),
		}
	}

	fn handle_hello(&mut self, conn_id: u64, protocol: u16, name: String, draw_dist: i32) {
		let Some(out) = self.lobby.remove(&conn_id) else {
			return;
		};
		if protocol != PROTOCOL_VERSION {
			send_frame(
				&out,
				&ServerMsg::Refused {
					reason: format!("protocol {protocol}, server wants {PROTOCOL_VERSION}"),
				},
			);
			let _ = out.send(OutMsg::Close);
			return;
		}
		let Some(id) = self.free_id() else {
			send_frame(
				&out,
				&ServerMsg::Refused {
					reason: "server full".into(),
				},
			);
			let _ = out.send(OutMsg::Close);
			return;
		};
		let draw_dist = draw_dist.clamp(2, 32);
		send_frame(
			&out,
			&ServerMsg::Welcome {
				protocol: PROTOCOL_VERSION,
				seed: self.store.seed(),
				player_id: id,
				spawn: self.spawn.to_array(),
				server_tick: self.tick,
				snapshot_divisor: SNAPSHOT_DIVISOR,
			},
		);

		// Join sync: final cell state for every edited column in range.
		let center = column_of(self.spawn);
		let mut synced = FxHashSet::default();
		let columns: Vec<(i32, i32)> = self.journal.index.edited_columns().collect();
		for (cx, cz) in columns {
			if in_radius((cx, cz), center, draw_dist) {
				self.send_journal_column(&out, cx, cz);
				synced.insert((cx, cz));
			}
		}
		send_frame(
			&out,
			&ServerMsg::JournalDone {
				through_seq: self.journal.index.next_seq,
			},
		);

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
			send_frame(
				&c.out,
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
				player: Player::new(self.spawn),
				look: Vec3::Z,
				last_intent: PlayerIntent::default(),
				inputs: BTreeMap::new(),
				last_input_tick: 0,
				drift: Vec3::ZERO,
				edit_tokens: EDIT_BURST,
				draw_dist,
				synced_columns: synced,
				live: false,
				center,
			},
		);
		self.residency_dirty = true;
	}

	fn drop_client(&mut self, conn_id: u64) {
		if let Some(client) = self.clients.remove(&conn_id) {
			log::info!("player {} ({}) left", client.id, client.name);
			for c in self.clients.values() {
				send_frame(&c.out, &ServerMsg::PlayerLeft { id: client.id });
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

fn cell_of(pos: Vec3) -> (i32, i32, i32) {
	(
		pos.x.floor() as i32,
		pos.y.floor() as i32,
		pos.z.floor() as i32,
	)
}

fn in_radius(col: (i32, i32), center: (i32, i32), r: i32) -> bool {
	let (dx, dz) = (col.0 - center.0, col.1 - center.1);
	dx * dx + dz * dz <= r * r
}

fn send_frame(out: &UnboundedSender<OutMsg>, msg: &ServerMsg) {
	let mut buf = Vec::new();
	if codec::write_frame(msg, &mut buf).is_ok() {
		let _ = out.send(OutMsg::Frame(buf));
	}
}

#[cfg(test)]
mod tests {
	use super::harness::*;
	use super::{ServerMsg, SimEvent};

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
