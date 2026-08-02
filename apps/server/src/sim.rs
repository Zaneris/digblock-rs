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

use crate::journal::Journal;
use crate::store::{ChunkStore, StoreWorld};
use digblock_core::block::BlockType;
use digblock_core::fluid::{FluidSim, FLOW_BUDGET, FLOW_STEP_SECS};
use digblock_core::player::{self, Player, PlayerIntent, PlayerState, TICK};
use digblock_core::raycast::{raycast, REACH};
use digblock_net::codec;
use digblock_net::messages::{
	BlockEdit, ClientMsg, Datagram, EditKind, InputBatch, InputTick, NetPlayerState, RejectReason,
	ServerMsg, Snapshot,
};
use digblock_net::protocol::{
	PlayerId, DRIFT_ALPHA, DRIFT_TOLERANCE, EDIT_BURST, EDIT_POS_TOLERANCE, EDIT_RATE_PER_SEC,
	JITTER_BUFFER_TICKS, MAX_SPEED, POS_TOLERANCE, PROTOCOL_VERSION, SERVER_ID, SNAPSHOT_DIVISOR,
	TICK_RATE,
};
use glam::Vec3;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::path::Path;
use tokio::sync::mpsc::UnboundedSender;

/// Jitter-buffer hard cap (~half a second of inputs).
const INPUT_BUF_CAP: usize = 30;
/// Extra slack on the per-tick displacement clamp for float noise.
const DISPLACEMENT_MARGIN: f32 = 0.05;
/// Columns beyond `draw_dist + slack` fall out of a client's synced set,
/// so re-entry re-syncs (clients evict remote chunks too).
const SYNC_SLACK: i32 = 2;

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

struct PendingEditReq {
	conn_id: u64,
	seq: u32,
	origin: Vec3,
	dir: Vec3,
	kind: EditKind,
	cell: (i32, i32, i32),
	normal: (i32, i32, i32),
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
}

impl Sim {
	pub fn new(
		seed: f64,
		data_dir: &Path,
		sim_radius: i32,
		workers: usize,
	) -> std::io::Result<Self> {
		let journal = Journal::open(data_dir, seed)?;
		let store = ChunkStore::new(seed, workers);
		let spawn = player::spawn(seed);
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
	fn test_block_at(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		self.store.block_at(x, y, z)
	}

	#[cfg(test)]
	fn test_player(&self, conn_id: u64) -> Option<PlayerState> {
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

	fn handle_inputs(&mut self, conn_id: u64, batch: InputBatch) {
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		for input in batch.inputs {
			if input.tick <= client.last_input_tick && client.last_input_tick != 0 {
				continue;
			}
			let Some(clean) = sanitize(input) else {
				continue;
			};
			client.inputs.entry(clean.tick).or_insert(clean);
		}
		while client.inputs.len() > INPUT_BUF_CAP {
			client.inputs.pop_first();
		}
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

	// ---- residency + late sync ---------------------------------------

	fn update_residency(&mut self) {
		for client in self.clients.values_mut() {
			let center = column_of(client.player.pos);
			if center != client.center {
				client.center = center;
				self.residency_dirty = true;
			}
		}
		if self.residency_dirty {
			let centers: Vec<(i32, i32)> = self.clients.values().map(|c| c.center).collect();
			self.store.set_residency(&centers, self.sim_radius);
			self.residency_dirty = false;
		}
		self.store.drain(&self.journal.index);

		// Late journal sync + liveness flips.
		let journal = &self.journal;
		let store = &self.store;
		for client in self.clients.values_mut() {
			if !client.live && store.column_ready(client.center.0, client.center.1) {
				client.live = true;
				log::debug!("player {} live", client.id);
			}
			client
				.synced_columns
				.retain(|&col| in_radius(col, client.center, client.draw_dist + SYNC_SLACK));
			for (cx, cz) in journal.index.edited_columns() {
				if in_radius((cx, cz), client.center, client.draw_dist)
					&& !client.synced_columns.contains(&(cx, cz))
				{
					send_journal_column_to(journal, &client.out, cx, cz);
					client.synced_columns.insert((cx, cz));
				}
			}
		}
	}

	fn send_journal_column(&self, out: &UnboundedSender<OutMsg>, cx: i32, cz: i32) {
		send_journal_column_to(&self.journal, out, cx, cz);
	}

	// ---- physics + adoption ------------------------------------------

	fn step_physics(&mut self) {
		let store = &self.store;
		for client in self.clients.values_mut() {
			if !client.live {
				continue;
			}
			// Drain toward the target jitter-buffer depth: normally one
			// input per tick; two when the buffer has backed up.
			let consume = if client.inputs.len() > (JITTER_BUFFER_TICKS + 2) as usize {
				2
			} else {
				1
			};
			for _ in 0..consume {
				let input = client.inputs.pop_first().map(|(_, i)| i);
				step_client(client, input, &|x, y, z| store.block_at(x, y, z));
			}
		}
	}

	// ---- edits --------------------------------------------------------

	fn process_edits(&mut self) {
		// Refill token buckets.
		for client in self.clients.values_mut() {
			client.edit_tokens =
				(client.edit_tokens + EDIT_RATE_PER_SEC as f32 / TICK_RATE as f32).min(EDIT_BURST);
		}
		let queue = std::mem::take(&mut self.edit_queue);
		for req in queue {
			self.process_one_edit(req);
		}
	}

	fn process_one_edit(&mut self, req: PendingEditReq) {
		// Copy what we need out of the client entry so the &mut borrow
		// ends before the all-players InsidePlayer scan below.
		let (id, out, live, player_pos, has_token) = {
			let Some(client) = self.clients.get_mut(&req.conn_id) else {
				return;
			};
			let has_token = client.edit_tokens >= 1.0;
			if client.live && has_token {
				client.edit_tokens -= 1.0;
			}
			(
				client.id,
				client.out.clone(),
				client.live,
				client.player.pos,
				has_token,
			)
		};
		let reject = |reason: RejectReason| {
			send_frame(
				&out,
				&ServerMsg::EditRejected {
					client_seq: req.seq,
					reason,
				},
			);
		};

		if !live {
			return reject(RejectReason::NotLoaded);
		}
		if !has_token {
			return reject(RejectReason::RateLimited);
		}

		let place_block = match req.kind {
			EditKind::Dig => None,
			EditKind::Place { block } => {
				if block == BlockType::Air as u8 || block as usize >= BlockType::COUNT {
					return reject(RejectReason::BadBlock);
				}
				Some(BlockType::from_u8(block))
			}
		};
		if !req.origin.is_finite() || !req.dir.is_finite() {
			return reject(RejectReason::OutOfReach);
		}
		let dir = req.dir.normalize_or_zero();
		if dir == Vec3::ZERO {
			return reject(RejectReason::OutOfReach);
		}
		if req.origin.distance(player_pos) > EDIT_POS_TOLERANCE {
			return reject(RejectReason::OutOfReach);
		}

		// Re-cast the client's ray against the server world: an honest
		// client computed the identical result.
		let store = &self.store;
		let Some(hit) = raycast(req.origin, dir, REACH, &|x, y, z| store.block_at(x, y, z)) else {
			return reject(RejectReason::OutOfReach);
		};
		if (hit.cell.0, hit.cell.1, hit.cell.2) != req.cell {
			return reject(RejectReason::Occluded);
		}

		let (target, block) = match place_block {
			None => (req.cell, BlockType::Air),
			Some(b) => {
				if hit.normal == (0, 0, 0) {
					return reject(RejectReason::Occluded);
				}
				if (hit.normal.0, hit.normal.1, hit.normal.2) != req.normal {
					return reject(RejectReason::Occluded);
				}
				let target = (
					req.cell.0 + hit.normal.0,
					req.cell.1 + hit.normal.1,
					req.cell.2 + hit.normal.2,
				);
				// Mirrors the local-engine rejection (eye/body cell) but
				// for every live player.
				for c in self.clients.values() {
					if !c.live {
						continue;
					}
					let eye = cell_of(c.player.pos);
					let body = cell_of(c.player.pos - Vec3::new(0.0, 1.25, 0.0));
					if target == eye || target == body {
						return reject(RejectReason::InsidePlayer);
					}
				}
				(target, b)
			}
		};

		let Some(prev) = self.store.set_block(target.0, target.1, target.2, block) else {
			return reject(RejectReason::NotLoaded);
		};
		let edit = BlockEdit {
			x: target.0,
			y: target.1,
			z: target.2,
			block: block as u8,
		};
		let seq = self.journal.append(edit, id);
		self.fluid.schedule_around(target.0, target.1, target.2);
		self.broadcast_edit(seq, id, req.seq, edit, prev as u8);
	}

	fn broadcast_edit(&self, seq: u64, by: PlayerId, client_seq: u32, edit: BlockEdit, prev: u8) {
		let col = (edit.x.div_euclid(16), edit.z.div_euclid(16));
		let msg = ServerMsg::EditApplied {
			seq,
			tick: self.tick,
			by,
			client_seq,
			edit,
			prev,
		};
		for c in self.clients.values() {
			if c.synced_columns.contains(&col) || c.id == by {
				send_frame(&c.out, &msg);
			}
		}
	}

	// ---- fluid --------------------------------------------------------

	fn step_fluid(&mut self) {
		self.flow_accum = (self.flow_accum + TICK).min(1.0);
		while self.flow_accum >= FLOW_STEP_SECS {
			self.flow_accum -= FLOW_STEP_SECS;
			let mut edits = Vec::new();
			let mut world = StoreWorld {
				store: &mut self.store,
				edits: &mut edits,
			};
			self.fluid.step(&mut world, FLOW_BUDGET);
			for edit in edits {
				let prev = 0; // recorded value is what fluid overwrote; not tracked
				let seq = self.journal.append(edit, SERVER_ID);
				self.broadcast_edit(seq, SERVER_ID, 0, edit, prev);
			}
		}
	}

	// ---- snapshots ----------------------------------------------------

	fn send_snapshots(&mut self) {
		let players: Vec<NetPlayerState> = self
			.clients
			.values()
			.filter(|c| c.live)
			.map(|c| {
				let s = c.player.state();
				NetPlayerState {
					id: c.id,
					pos: s.pos.to_array(),
					vel: s.vel.to_array(),
					look: c.look.to_array(),
				}
			})
			.collect();
		for client in self.clients.values() {
			if !client.live {
				continue;
			}
			let snap = Snapshot {
				server_tick: self.tick,
				your_last_input_tick: client.last_input_tick,
				players: players.clone(),
			};
			if let Ok(bytes) = codec::encode_datagram(&Datagram::Snapshot(snap)) {
				let _ = client.out.send(OutMsg::Datagram(bytes));
			}
		}
	}
}

/// One client sim tick: consume `input` (or coast on the last intent),
/// then run the adoption envelope against the claimed post-state.
fn step_client(
	client: &mut Client,
	input: Option<InputTick>,
	block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>,
) {
	let before = client.player.pos;
	match input {
		Some(input) => {
			let intent = PlayerIntent {
				move_axis: glam::Vec2::from_array(input.move_axis),
				jump: input.jump,
			};
			client.look = Vec3::from_array(input.look).normalize_or_zero();
			client.last_intent = PlayerIntent {
				jump: false,
				..intent
			};
			client.player.tick(TICK, client.look, intent, block_at);
			client.last_input_tick = input.tick;

			// Adoption envelope.
			let claimed = PlayerState {
				pos: Vec3::from_array(input.claimed_pos),
				vel: Vec3::from_array(input.claimed_vel),
			};
			let max_step = MAX_SPEED * TICK + DISPLACEMENT_MARGIN;
			let sane = claimed.pos.is_finite()
				&& claimed.vel.is_finite()
				&& claimed.pos.distance(client.player.pos) <= POS_TOLERANCE
				&& claimed.pos.distance(before) <= max_step
				&& !probe_solid(claimed.pos, block_at);
			if sane {
				// Second gate: sustained drift. The per-tick envelope
				// bounds instantaneous divergence; the delta-vector EWMA
				// bounds it over time. Honest deltas are zero-mean float
				// noise (identical physics from the same adopted state)
				// and cancel; riding the envelope edge needs a persistent
				// directional bias that pumps the average instead. Insane
				// claims never feed the average — one wild claim at the
				// streaming edge must not lock an honest client out.
				let delta = claimed.pos - client.player.pos;
				client.drift = client.drift * (1.0 - DRIFT_ALPHA) + delta * DRIFT_ALPHA;
				if client.drift.length() <= DRIFT_TOLERANCE {
					client.player.set_state(claimed);
				}
			} else {
				client.drift *= 1.0 - DRIFT_ALPHA;
			}
		}
		None => {
			// Underrun: coast on the last intent, never adopt.
			let intent = client.last_intent;
			client.player.tick(TICK, client.look, intent, block_at);
			client.drift *= 1.0 - DRIFT_ALPHA;
		}
	}
}

/// True if the claimed position's collision probes sit inside solid
/// blocks (a claim we never adopt).
fn probe_solid(pos: Vec3, block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>) -> bool {
	for probe in [pos, pos - Vec3::new(0.0, 1.25, 0.0)] {
		let c = cell_of(probe);
		if matches!(block_at(c.0, c.1, c.2), Some(b) if b.props().collides) {
			return true;
		}
	}
	false
}

fn sanitize(mut input: InputTick) -> Option<InputTick> {
	let axis = glam::Vec2::from_array(input.move_axis);
	let look = Vec3::from_array(input.look);
	if !axis.is_finite() || !look.is_finite() {
		return None;
	}
	if look.normalize_or_zero() == Vec3::ZERO {
		return None;
	}
	if axis.length_squared() > 1.0 {
		input.move_axis = axis.normalize().to_array();
	}
	Some(input)
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

fn send_journal_column_to(journal: &Journal, out: &UnboundedSender<OutMsg>, cx: i32, cz: i32) {
	let Some(cells) = journal.index.column(cx, cz) else {
		return;
	};
	let cells = cells
		.iter()
		.map(|(&(x, y, z), &block)| BlockEdit { x, y, z, block })
		.collect();
	send_frame(out, &ServerMsg::JournalColumn { cx, cz, cells });
}

#[cfg(test)]
mod tests {
	use super::*;
	use digblock_net::codec::FrameReader;
	use std::path::PathBuf;
	use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

	const SEED: f64 = 42.0;

	fn temp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("dbsim-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	struct TestClient {
		rx: UnboundedReceiver<OutMsg>,
		frames: FrameReader,
		msgs: Vec<ServerMsg>,
		snaps: Vec<Snapshot>,
	}

	impl TestClient {
		fn drain(&mut self) {
			while let Ok(out) = self.rx.try_recv() {
				match out {
					OutMsg::Frame(bytes) => {
						self.frames.extend(&bytes);
						while let Some(frame) = self.frames.next_frame().unwrap() {
							self.msgs.push(codec::decode(&frame).unwrap());
						}
					}
					OutMsg::Datagram(bytes) => {
						if let Datagram::Snapshot(s) = codec::decode_datagram(&bytes).unwrap() {
							self.snaps.push(s);
						}
					}
					OutMsg::Close => {}
				}
			}
		}
	}

	fn connect(sim: &mut Sim, conn_id: u64) -> TestClient {
		let (tx, rx) = unbounded_channel();
		sim.tick_once(vec![
			SimEvent::Connected { conn_id, out: tx },
			SimEvent::Msg {
				conn_id,
				msg: ClientMsg::Hello {
					protocol: PROTOCOL_VERSION,
					name: format!("t{conn_id}"),
					draw_dist: 4,
				},
			},
		]);
		let mut c = TestClient {
			rx,
			frames: FrameReader::default(),
			msgs: Vec::new(),
			snaps: Vec::new(),
		};
		c.drain();
		assert!(
			matches!(c.msgs.first(), Some(ServerMsg::Welcome { .. })),
			"expected Welcome, got {:?}",
			c.msgs.first()
		);
		c
	}

	fn wait_live(sim: &mut Sim, conn_id: u64) {
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
		while !sim.test_is_live(conn_id) {
			assert!(
				std::time::Instant::now() < deadline,
				"client never went live"
			);
			sim.tick_once(vec![]);
			std::thread::sleep(std::time::Duration::from_millis(1));
		}
	}

	fn input(tick: u32, forward: bool, claimed: PlayerState) -> InputTick {
		InputTick {
			tick,
			move_axis: [0.0, if forward { 1.0 } else { 0.0 }],
			jump: false,
			look: [0.0, 0.0, 1.0],
			claimed_pos: claimed.pos.to_array(),
			claimed_vel: claimed.vel.to_array(),
		}
	}

	/// A claim that is never adopted (far outside the envelope), for
	/// tests that want pure server physics.
	fn no_claim() -> PlayerState {
		PlayerState {
			pos: Vec3::new(0.0, -500.0, 0.0),
			vel: Vec3::ZERO,
		}
	}

	fn batch(inputs: Vec<InputTick>, conn_id: u64) -> SimEvent {
		SimEvent::Inputs {
			conn_id,
			batch: InputBatch { inputs },
		}
	}

	#[test]
	fn join_roster_and_announcements() {
		let dir = temp_dir("roster");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
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

	#[test]
	fn honest_claims_adopted_and_echoed() {
		let dir = temp_dir("honest");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Honest local twin: same physics over the same (server) world,
		// one tick ahead of the server consuming it. Delivered one input
		// per tick, the buffer never backs up, so consumed inputs and
		// server ticks stay 1:1.
		let mut local = Player::new(sim.spawn);
		let mut history = Vec::new(); // local post-state per input tick
		let mut t = 100;
		for _ in 0..60 {
			let intent = PlayerIntent {
				move_axis: glam::Vec2::new(0.0, 1.0),
				jump: false,
			};
			local.tick(TICK, Vec3::Z, intent, &|x, y, z| sim.test_block_at(x, y, z));
			let claim = local.state();
			history.push(claim.pos);
			sim.tick_once(vec![batch(vec![input(t, true, claim)], 1)]);
			t += 1;
		}

		let server = sim.test_player(1).unwrap();
		assert_eq!(server.pos, local.pos, "honest claim adopted verbatim");
		c.drain();
		// The last snapshot echoes our own claim at its acked tick.
		let last = c.snaps.last().expect("snapshots flowed");
		assert!(last.your_last_input_tick >= 100);
		let echoed = Vec3::from_array(last.players[0].pos);
		assert_eq!(echoed, history[(last.your_last_input_tick - 100) as usize]);
	}

	#[test]
	fn off_envelope_claim_keeps_server_state() {
		let dir = temp_dir("offclaim");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let before = sim.test_player(1).unwrap();

		let mut t = 100;
		for _ in 0..30 {
			// Claim teleports 3 blocks sideways: outside POS_TOLERANCE.
			let claim = PlayerState {
				pos: before.pos + Vec3::new(3.0, 0.0, 0.0),
				vel: Vec3::ZERO,
			};
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
		}
		let server = sim.test_player(1).unwrap();
		assert!(
			(server.pos.x - before.pos.x).abs() < 0.01,
			"server kept its own x: {} vs claim +3",
			server.pos.x
		);
	}

	#[test]
	fn speed_hack_claims_rejected() {
		let dir = temp_dir("speed");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let start = sim.test_player(1).unwrap();

		// Each claim is 2 blocks further: near the *previous claim* but
		// far beyond MAX_SPEED * TICK per tick.
		let mut t = 100;
		for i in 1..=30 {
			let claim = PlayerState {
				pos: start.pos + Vec3::new(2.0 * i as f32, 0.0, 0.0),
				vel: Vec3::ZERO,
			};
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
		}
		let server = sim.test_player(1).unwrap();
		assert!(
			(server.pos.x - start.pos.x).abs() < 0.5,
			"teleport chain not adopted: {} vs start {}",
			server.pos.x,
			start.pos.x
		);
	}

	#[test]
	fn inside_solid_claim_rejected() {
		let dir = temp_dir("solid");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let start = sim.test_player(1).unwrap();

		// Claim eye 2 blocks under the surface (inside stone/dirt), but
		// within... no — that's > POS_TOLERANCE. Claim just below the
		// feet instead: within tolerance, but the body probe lands in
		// the ground block.
		let claim = PlayerState {
			pos: start.pos - Vec3::new(0.0, 0.7, 0.0),
			vel: Vec3::ZERO,
		};
		let mut t = 100;
		for _ in 0..10 {
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
		}
		let server = sim.test_player(1).unwrap();
		assert!(
			(server.pos.y - start.pos.y).abs() < 0.2,
			"in-ground claim not adopted: {} vs {}",
			server.pos.y,
			start.pos.y
		);
	}

	#[test]
	fn sustained_edge_riding_locked_out_then_recovers() {
		let dir = temp_dir("drift");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let start = sim.test_player(1).unwrap();

		// Fly cheat riding the per-tick envelope: every claim +0.4 blocks
		// up from the previous claim — each one individually within
		// POS_TOLERANCE and the displacement clamp while adopted, but the
		// delta EWMA is pure +y bias and trips DRIFT_TOLERANCE within a
		// handful of ticks; after that the sim stops following and the
		// marching claims fall out of the hard envelope too.
		let mut t = 100;
		for i in 1..=60 {
			let claim = PlayerState {
				pos: start.pos + Vec3::new(0.0, 0.4 * i as f32, 0.0),
				vel: Vec3::ZERO,
			};
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
		}
		let cheated = sim.test_player(1).unwrap();
		let gained = cheated.pos.y - start.pos.y;
		assert!(
			gained < 5.0,
			"attempted 24 blocks of fly, kept {gained} (drift gate)"
		);

		// Honest again: out-of-envelope garbage claims decay the EWMA
		// (they never feed it), the player falls back and re-grounds.
		for _ in 0..150 {
			sim.tick_once(vec![batch(vec![input(t, false, no_claim())], 1)]);
			t += 1;
		}
		// Adoption works again: a tiny honest offset is taken verbatim.
		let s = sim.test_player(1).unwrap();
		let claim = PlayerState {
			pos: s.pos + Vec3::new(0.01, 0.0, 0.0),
			vel: s.vel,
		};
		sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
		let after = sim.test_player(1).unwrap();
		assert_eq!(after.pos, claim.pos, "drift decayed, adoption resumed");
	}

	#[test]
	fn zero_mean_jitter_stays_adopted() {
		let dir = temp_dir("jitter");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Settle onto the ground first (garbage claims, pure server sim).
		let mut t = 100;
		for _ in 0..90 {
			sim.tick_once(vec![batch(vec![input(t, false, no_claim())], 1)]);
			t += 1;
		}
		let settled = sim.test_player(1).unwrap();

		// Honest client whose claims wobble ±5 mm around the sim —
		// alternating sign, so the delta EWMA cancels vectorially and
		// every single claim keeps being adopted, even over many ticks.
		let mut last_claim = settled;
		for i in 0..240 {
			let jitter = if i % 2 == 0 { 0.005 } else { -0.005 };
			let claim = PlayerState {
				pos: Vec3::new(settled.pos.x + jitter, settled.pos.y, settled.pos.z),
				vel: Vec3::ZERO,
			};
			sim.tick_once(vec![batch(vec![input(t, false, claim)], 1)]);
			t += 1;
			last_claim = claim;
		}
		let server = sim.test_player(1).unwrap();
		assert_eq!(server.pos, last_claim.pos, "noise never trips the gate");
	}

	#[test]
	fn edits_validate_apply_and_journal() {
		let dir = temp_dir("edits");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;

		// The ground block straight down is in reach.
		let down = Vec3::NEG_Y;
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_block_at(x, y, z))
			.expect("ground under spawn");

		let dig = |seq: u32, origin: Vec3, cell: (i32, i32, i32)| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 100,
				origin: origin.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [cell.0, cell.1, cell.2],
				normal: [0, 0, 0],
			},
		};

		// 1. Wrong cell → Occluded.
		sim.tick_once(vec![dig(1, pos, (hit.cell.0 + 1, hit.cell.1, hit.cell.2))]);
		// 2. Origin 3 blocks off → OutOfReach.
		sim.tick_once(vec![dig(2, pos + Vec3::new(3.0, 0.0, 0.0), hit.cell)]);
		// 3. Honest dig → applied.
		sim.tick_once(vec![dig(3, pos, hit.cell)]);
		c.drain();

		let rejected: Vec<(u32, RejectReason)> = c
			.msgs
			.iter()
			.filter_map(|m| match m {
				ServerMsg::EditRejected { client_seq, reason } => Some((*client_seq, *reason)),
				_ => None,
			})
			.collect();
		assert!(
			rejected.contains(&(1, RejectReason::Occluded)),
			"{rejected:?}"
		);
		assert!(
			rejected.contains(&(2, RejectReason::OutOfReach)),
			"{rejected:?}"
		);
		let applied = c.msgs.iter().find_map(|m| match m {
			ServerMsg::EditApplied {
				client_seq, edit, ..
			} if *client_seq == 3 => Some(*edit),
			_ => None,
		});
		let edit = applied.expect("dig 3 applied");
		assert_eq!((edit.x, edit.y, edit.z), hit.cell);
		assert_eq!(edit.block, BlockType::Air as u8);
		assert_eq!(
			sim.test_block_at(edit.x, edit.y, edit.z),
			Some(BlockType::Air)
		);
		assert!(sim
			.journal
			.index
			.column(edit.x.div_euclid(16), edit.z.div_euclid(16))
			.is_some_and(|col| col.get(&(edit.x, edit.y, edit.z)) == Some(&(BlockType::Air as u8))));
	}

	#[test]
	fn place_into_player_and_rate_limit() {
		let dir = temp_dir("place");
		let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::NEG_Y;
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_block_at(x, y, z)).unwrap();

		// Placing on the ground's top face targets our own body cell.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 100,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Place {
					block: BlockType::Dirt as u8,
				},
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 1,
					reason: RejectReason::InsidePlayer
				}
			)),
			"{:?}",
			c.msgs
		);

		// Hammer digs: the token bucket runs dry.
		let digs: Vec<SimEvent> = (10..30)
			.map(|seq| SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::EditRequest {
					seq,
					tick: 100,
					origin: pos.to_array(),
					dir: down.to_array(),
					kind: EditKind::Dig,
					cell: [hit.cell.0, hit.cell.1, hit.cell.2],
					normal: [0, 0, 0],
				},
			})
			.collect();
		sim.tick_once(digs);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::RateLimited,
					..
				}
			)),
			"rate limiter engaged"
		);
	}

	#[test]
	fn determinism_under_datagram_loss() {
		let run = |tag: &str, lossy: bool| -> Vec3 {
			let dir = temp_dir(tag);
			let mut sim = Sim::new(SEED, &dir, 1, 2).unwrap();
			let _c = connect(&mut sim, 1);
			wait_live(&mut sim, 1);

			// 60 forward inputs. Clean: one batch per tick. Lossy: every
			// other delivery dropped, survivors carry the last 4 ticks
			// (the wire redundancy window).
			let inputs: Vec<InputTick> =
				(0..60).map(|i| input(100 + i, true, no_claim())).collect();
			for (i, _) in inputs.iter().enumerate() {
				let events = if lossy {
					if i % 2 == 1 {
						let lo = i.saturating_sub(3);
						vec![batch(inputs[lo..=i].to_vec(), 1)]
					} else {
						vec![]
					}
				} else {
					vec![batch(vec![inputs[i]], 1)]
				};
				sim.tick_once(events);
			}
			// Settle: let the jitter buffer drain completely.
			for _ in 0..40 {
				sim.tick_once(vec![]);
			}
			// Every input tick reached the server: redundancy defeated
			// the datagram loss (this is the real assertion; position
			// equality alone would also pass with dropped inputs since
			// coasting reuses the last intent).
			assert_eq!(sim.test_last_input_tick(1), 159, "all inputs consumed");
			sim.test_player(1).unwrap().pos
		};

		// The hard guarantee (asserted inside `run`): every input tick is
		// consumed exactly once despite half the deliveries vanishing.
		// Positions are close but not bit-equal: when the buffer backs
		// up the server deliberately double-ticks to catch up, so the
		// server-tick↔player-tick mapping is elastic under loss.
		let clean = run("det-clean", false);
		let lossy = run("det-lossy", true);
		assert!(
			clean.distance(lossy) < 1.0,
			"paths stayed close: {clean:?} vs {lossy:?}"
		);
	}
}
