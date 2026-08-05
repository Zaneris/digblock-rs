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

mod admin;
mod edits;
mod movement;
mod sync;

#[cfg(test)]
pub(crate) mod harness;

use crate::ban_store::BanStore;
use crate::journal::Journal;
use crate::player_store::{hash_identity, IdentityHash, PlayerLayout, PlayerRecord, PlayerStore};
use crate::store::{ChunkStore, ColumnSource};
use digblock_core::fluid::FluidSim;
use digblock_core::hotbar::Hotbar;
use digblock_core::inv_grid::{reconcile_layout, InvGrid};
use digblock_core::inventory::Inventory;
use digblock_core::player::{self, Player, PlayerIntent, PlayerState};
use digblock_net::codec;
use digblock_net::messages::{ClientMsg, InputBatch, InputTick, RejectReason, ServerMsg};
use digblock_net::protocol::{
	sanitize_chat, sanitize_name, PlayerId, CHAT_BURST, EDIT_BURST, MIN_PROTOCOL, PROTOCOL_VERSION,
	RETURN_SPAWN_COOLDOWN_SECS, SERVER_ID, SNAPSHOT_DIVISOR, TICK_RATE, TOD_SYNC_TICKS,
};
use edits::{ClientAction, PendingEditReq};
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

/// Server-side clamp on any client-supplied draw distance (Hello and
/// runtime `SetDrawDist` alike) — bounds the sync radius, not rendering.
const DRAW_DIST_MIN: i32 = 2;
const DRAW_DIST_MAX: i32 = 32;

/// A connection that passes the preamble but never completes Hello is
/// dropped after this many ticks (5s — mirrors the preamble deadline).
/// Without it a stalled handshake camps in the lobby forever: the
/// server's own keep-alives defeat QUIC's idle timeout, and the 255-
/// player cap applies only post-Hello.
const HELLO_TIMEOUT_TICKS: u32 = 5 * TICK_RATE;

pub enum SimEvent {
	Connected {
		conn_id: u64,
		out: OutTx,
		/// Peer address — the ban key (`addr.ip()`).
		addr: std::net::SocketAddr,
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
	out: OutTx,
	/// Peer address from the net layer — the `/ban` key.
	addr: std::net::SocketAddr,
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
	chat_tokens: f32,
	draw_dist: i32,
	synced_columns: FxHashSet<(i32, i32)>,
	/// Run the full edited-column sync scan for this client next tick.
	/// Set on center change and on going live; steady state does ZERO
	/// journal scans (fresh columns arrive via the first-edit broadcast).
	needs_sync_scan: bool,
	live: bool,
	center: (i32, i32),
	/// Authoritative per-player block counts (the client predicts against
	/// its own copy and reconciles).
	inventory: Inventory,
	/// The client's item arrangement (sanitized from `ClientMsg::Layout`,
	/// kept current on the reliable stream) — the enforcement source for
	/// "a place must use the item in its declared slot".
	hotbar: Hotbar,
	grid: InvGrid,
	/// Selected hotbar slot from the newest consumed input — feeds the
	/// held-item field in snapshots (display only; enforcement reads the
	/// edit's own declared slot).
	active_slot: u8,
	/// `Some` = persistent identity (the stored hash, never the secret);
	/// `None` = anonymous, session-only inventory.
	identity: Option<IdentityHash>,
	/// In-progress hold-to-dig, if any (see `edits::DigState`).
	digging: Option<edits::DigState>,
	/// Tick of the last granted return-to-spawn; `None` = never used
	/// (the first request after joining is always allowed).
	last_return_tick: Option<u32>,
}

impl Client {
	/// The held item for snapshots, in hotbar wire encoding.
	fn held_wire(&self) -> u16 {
		use digblock_core::hotbar::{HotbarItem, HOTBAR_SLOTS, WIRE_EMPTY, WIRE_TOOL};
		let slot = (self.active_slot as usize).min(HOTBAR_SLOTS - 1);
		match self.hotbar.slots()[slot] {
			None => WIRE_EMPTY,
			Some(HotbarItem::WaterTool) => WIRE_TOOL,
			Some(HotbarItem::Block(b)) => b as u16,
		}
	}

	/// The persistence snapshot (written through on edits + disconnect).
	fn record(&self) -> PlayerRecord {
		PlayerRecord {
			inventory: self.inventory.clone(),
			layout: Some(PlayerLayout {
				hotbar: self.hotbar.to_wire(),
				grid: self.grid.to_wire(),
			}),
		}
	}
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
	lobby: FxHashMap<u64, (OutTx, u32, std::net::SocketAddr)>,
	/// Client actions (edits + dig starts/cancels) queued this tick,
	/// processed in wire order by `process_edits`.
	action_queue: Vec<edits::ClientAction>,
	residency_dirty: bool,
	/// (newly-ready column, resident neighbor) pairs awaiting a budgeted
	/// journal-water rescan, so fluid fronts stalled at a load boundary
	/// resume when the far side arrives.
	face_scans: std::collections::VecDeque<((i32, i32), (i32, i32))>,
	/// Last tick a snapshot-encode failure was logged (rate limit).
	last_oversize_log: Option<u32>,
	/// Persistent per-identity player records — inventory + item layout
	/// (separate from the journal).
	player_store: PlayerStore,
	/// Persistent IP bans, enforced at Hello and driven by `/ban`.
	ban_store: BanStore,
	/// The player name (case-insensitive) granted admin chat commands.
	admin_name: Option<String>,
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
		let player_store = PlayerStore::open(data_dir)?;
		let ban_store = BanStore::open(data_dir)?;
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
			action_queue: Vec::new(),
			residency_dirty: true,
			face_scans: std::collections::VecDeque::new(),
			last_oversize_log: None,
			player_store,
			ban_store,
			admin_name: None,
		})
	}

	/// Grant a player name (case-insensitive) the admin chat commands.
	/// Empty counts as unset, like the env fallbacks.
	pub fn set_admin_name(&mut self, name: Option<String>) {
		self.admin_name = name.filter(|n| !n.is_empty());
	}

	pub fn server_tick(&self) -> u32 {
		self.tick
	}

	/// The authoritative world clock: a pure function of the tick
	/// counter, so it needs no state and no persistence (a restart
	/// restarts the day — accepted).
	fn current_tod(&self) -> f32 {
		digblock_core::sun::Sun::tod_of_secs(f64::from(self.tick) / f64::from(TICK_RATE))
	}

	/// Read each tick by `run_sim_loop` into the shared gauge the master
	/// heartbeat reports.
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
		self.verify_digs();
		self.process_edits();
		self.step_fluid();
		self.tick += 1;
		if self.tick % SNAPSHOT_DIVISOR == 0 {
			self.send_snapshots();
		}
		if self.tick % TOD_SYNC_TICKS == 0 {
			let msg = ServerMsg::TimeOfDay {
				tod: self.current_tod(),
			};
			for c in self.clients.values() {
				client_send(c, &msg);
			}
		}
		self.journal.maintain();
		self.player_store.maintain();
	}

	/// Flush and fsync the journal + inventory store (graceful shutdown).
	pub fn shutdown(&mut self) {
		self.journal.sync();
		self.player_store.sync();
	}

	#[cfg(test)]
	fn test_block_at(&self, x: i32, y: i32, z: i32) -> Option<digblock_core::block::BlockType> {
		self.store.block_at(x, y, z)
	}

	#[cfg(test)]
	fn test_cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.store.cell_at(x, y, z)
	}

	#[cfg(test)]
	fn test_grant(&mut self, conn_id: u64, b: digblock_core::block::BlockType, n: u32) {
		let client = self.clients.get_mut(&conn_id).unwrap();
		client.inventory.add(b, n);
		// Like a real acquisition: the granted type lands on the bar, so
		// places (now slot-enforced) validate the way an honest client's
		// would.
		reconcile_layout(&client.inventory, &mut client.hotbar, &mut client.grid);
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
	fn test_look(&self, conn_id: u64) -> Option<Vec3> {
		self.clients.get(&conn_id).map(|c| c.look)
	}

	#[cfg(test)]
	fn test_last_input_tick(&self, conn_id: u64) -> u32 {
		self.clients.get(&conn_id).map_or(0, |c| c.last_input_tick)
	}

	#[cfg(test)]
	fn test_draw_dist(&self, conn_id: u64) -> Option<i32> {
		self.clients.get(&conn_id).map(|c| c.draw_dist)
	}

	#[cfg(test)]
	fn test_inventory_count(&self, conn_id: u64, b: digblock_core::block::BlockType) -> u32 {
		self.clients
			.get(&conn_id)
			.map_or(0, |c| c.inventory.count(b))
	}

	// ---- events -------------------------------------------------------

	fn handle_event(&mut self, ev: SimEvent) {
		match ev {
			SimEvent::Connected { conn_id, out, addr } => {
				self.lobby.insert(conn_id, (out, self.tick, addr));
			}
			SimEvent::Msg { conn_id, msg } => match msg {
				ClientMsg::Hello {
					protocol,
					name,
					draw_dist,
					token: _, // future reconnect-with-state
					sim_version,
					build,
					identity,
				} => self.handle_hello(
					conn_id,
					protocol,
					name,
					draw_dist,
					sim_version,
					build,
					identity,
				),
				ClientMsg::EditRequest {
					seq,
					tick: _,
					origin,
					dir,
					kind,
					cell,
					normal,
					slot,
				} => {
					if let Some(client) = self.clients.get(&conn_id) {
						// Bounded: a flood beyond every client's burst
						// budget is rejected up front, not accumulated.
						let cap = self.clients.len() * EDIT_BURST as usize + 64;
						if self.action_queue.len() >= cap {
							client_send(
								client,
								&ServerMsg::EditRejected {
									client_seq: seq,
									reason: RejectReason::RateLimited,
								},
							);
						} else {
							self.action_queue.push(ClientAction::Edit(PendingEditReq {
								conn_id,
								seq,
								origin: Vec3::from_array(origin),
								dir: Vec3::from_array(dir),
								kind,
								cell: (cell[0], cell[1], cell[2]),
								normal: (normal[0] as i32, normal[1] as i32, normal[2] as i32),
								slot,
							}));
						}
					}
				}
				ClientMsg::Bye => self.drop_client(conn_id),
				// Queued, NOT applied here: a DigStart must not overtake a
				// same-tick completing EditRequest already in the queue
				// (see `ClientAction`). Beyond the queue cap it drops
				// silently.
				ClientMsg::DigStart { cell } => {
					if self.action_queue.len() < self.clients.len() * EDIT_BURST as usize + 64 {
						self.action_queue.push(ClientAction::DigStart {
							conn_id,
							cell: (cell[0], cell[1], cell[2]),
						});
					}
				}
				ClientMsg::DigCancel => {
					if self.action_queue.len() < self.clients.len() * EDIT_BURST as usize + 64 {
						self.action_queue.push(ClientAction::DigCancel { conn_id });
					}
				}
				// Inline, not queued: chat has no ordering relationship
				// with the edit stream. Over-budget or empty-after-
				// sanitize lines drop silently — a talkative client gets
				// throttled, never disconnected.
				ClientMsg::Chat { text } => {
					let Some(client) = self.clients.get_mut(&conn_id) else {
						return; // lobby connections don't get to talk
					};
					let is_command = text.trim_start().starts_with('/');
					let is_admin = self
						.admin_name
						.as_deref()
						.is_some_and(|a| client.name.eq_ignore_ascii_case(a));
					// Admin command lines bypass the token bucket (several
					// commands back-to-back is normal use, and replies are
					// private — no broadcast to flood).
					if !(is_command && is_admin) {
						if client.chat_tokens < 1.0 {
							return;
						}
						client.chat_tokens -= 1.0;
					}
					let from = client.id;
					let Some(text) = sanitize_chat(&text) else {
						return;
					};
					if is_command {
						// Slash lines are never broadcast — commands are a
						// private conversation with the server.
						if is_admin {
							self.handle_admin_command(conn_id, &text);
						} else if let Some(c) = self.clients.get(&conn_id) {
							client_send(c, &admin::server_reply("commands are admin-only"));
						}
						return;
					}
					let msg = ServerMsg::Chat { from, text };
					// Everyone, sender included: the echo is the client's
					// delivery confirmation.
					for c in self.clients.values() {
						client_send(c, &msg);
					}
				}
				// Inline like chat: no ordering relationship with the edit
				// stream (edit raycasts re-validate against the player's
				// position when they run anyway).
				ClientMsg::ReturnToSpawn => self.handle_return_to_spawn(conn_id),
				// Inline like chat: sync radius is read fresh each tick, so
				// the change simply applies from the next sync pass.
				ClientMsg::SetDrawDist { draw_dist } => {
					if let Some(client) = self.clients.get_mut(&conn_id) {
						client.draw_dist = draw_dist.clamp(DRAW_DIST_MIN, DRAW_DIST_MAX);
					}
				}
				// Queued: a craft-then-place in one tick must debit the
				// sub-materials before the place's stock gate runs (wire
				// order). Over-budget requests drop silently.
				ClientMsg::Alchemy { op } => {
					if self.action_queue.len() < self.clients.len() * EDIT_BURST as usize + 64 {
						self.action_queue
							.push(ClientAction::Alchemy { conn_id, op });
					}
				}
				// Queued: an assign-to-bar-then-place in one tick must
				// update the arrangement before the place's slot gate runs
				// (wire order). Over-budget requests drop silently.
				ClientMsg::Layout { hotbar, grid } => {
					if self.action_queue.len() < self.clients.len() * EDIT_BURST as usize + 64 {
						self.action_queue.push(ClientAction::Layout {
							conn_id,
							hotbar,
							grid,
						});
					}
				}
			},
			SimEvent::Inputs { conn_id, batch } => self.handle_inputs(conn_id, batch),
			SimEvent::Disconnected { conn_id } => self.drop_client(conn_id),
		}
	}

	/// Record a hold-to-dig start (called from the ordered queue drain):
	/// capture the cell's CURRENT block — the client never gets to claim
	/// what it is digging. Within a tick the LAST start wins, exactly
	/// like the client's retarget stream — an old first-wins per-tick cap
	/// silently dropped the restart of a cancel+restart pair bunched into
	/// one tick drain, leaving an honest hold with no server record
	/// (DigIncomplete pop-back on completion). Floods can't spin the
	/// timer — every start zeroes progress — and the action-queue cap
	/// bounds the per-tick volume.
	fn handle_dig_start(&mut self, conn_id: u64, cell: (i32, i32, i32)) {
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		let cell_value = self.store.cell_at(cell.0, cell.1, cell.2);
		client.digging = match cell_value {
			Some(v)
				if digblock_core::block::BlockType::from_id(digblock_core::chunk::cell_id(v))
					.dig_hardness(cell.1)
					.is_some() =>
			{
				Some(edits::DigState {
					cell,
					cell_value: v,
					progress: 0,
					off_target: 0,
				})
			}
			// Undiggable/unloaded target: no hold. The completing edit
			// will fail its DigIncomplete check.
			_ => None,
		};
	}

	/// The spawn point with any player-built obstruction cleared. The base
	/// spawn sits above pristine terrain, so only journaled (placed) cells
	/// can occupy it: resident chunks answer from the store (journal
	/// already replayed); non-resident cells fall back to the journal's
	/// in-memory final values — nobody needs to be near spawn for the
	/// scan to see a tower built there.
	fn safe_spawn(&self) -> Vec3 {
		self.clear_landing(self.spawn)
	}

	/// Raise `eye` until the capsule clears both live terrain and
	/// journaled edits — the shared landing rule for spawn and `/tp`.
	fn clear_landing(&self, eye: Vec3) -> Vec3 {
		let cell_at = |x: i32, y: i32, z: i32| {
			if let Some(c) = self.store.cell_at(x, y, z) {
				return Some(c);
			}
			// Raw cells, not `try_from_id`: a state-carrying cell (e.g. a
			// stair) must read as its real, solid block here — decoding to
			// None would let a joiner spawn inside it.
			self.journal
				.index
				.column(x.div_euclid(16), z.div_euclid(16))
				.and_then(|cells| cells.get(&(x, y, z)).copied())
		};
		player::ascend_clear(eye, &cell_at)
	}

	/// Hard-move a client: set state, shed pre-teleport residue (the
	/// drift EWMA and any dig hold describe the old position, and
	/// coasting on the old intent would walk the player off the target),
	/// tell the client, and re-derive residency. `look` zero = keep the
	/// current view.
	fn teleport_client(&mut self, conn_id: u64, pos: Vec3, look: Vec3) {
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		client.player.set_state(PlayerState {
			pos,
			vel: Vec3::ZERO,
		});
		client.drift = Vec3::ZERO;
		client.digging = None;
		client.last_intent = PlayerIntent::default();
		if look != Vec3::ZERO {
			// The client adopts this facing; mirror it so the server's
			// aim raycasts agree until the next input overwrites it.
			client.look = look;
		}
		client_send(
			client,
			&ServerMsg::Teleported {
				pos: pos.to_array(),
				look: look.to_array(),
			},
		);
		// The interest center jumped; re-derive residency + sync scope.
		self.residency_dirty = true;
	}

	/// Teleport a live client back to spawn, subject to the cooldown.
	/// Violations are silently ignored — honest clients gate locally.
	fn handle_return_to_spawn(&mut self, conn_id: u64) {
		let Some(client) = self.clients.get(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		if let Some(t) = client.last_return_tick {
			if self.tick.wrapping_sub(t) < RETURN_SPAWN_COOLDOWN_SECS * TICK_RATE {
				return;
			}
		}
		let pos = self.safe_spawn();
		let tick = self.tick;
		let client = self.clients.get_mut(&conn_id).expect("checked above");
		client.last_return_tick = Some(tick);
		log::info!("player {} ({}) returned to spawn", client.id, client.name);
		self.teleport_client(conn_id, pos, Vec3::ZERO);
	}

	#[allow(clippy::too_many_arguments)]
	fn handle_hello(
		&mut self,
		conn_id: u64,
		protocol: u16,
		name: String,
		draw_dist: i32,
		sim_version: u16,
		build: String,
		identity: [u8; 16],
	) {
		let Some((out, _, addr)) = self.lobby.remove(&conn_id) else {
			return;
		};
		let refuse = |out: &OutTx, reason: String| {
			send_frame(&out.clone(), &ServerMsg::Refused { reason });
			let _ = out.try_send(OutMsg::Close);
		};
		// Ban gate first — a banned peer learns nothing else.
		if self.ban_store.is_banned(addr.ip()) {
			refuse(&out, "banned".into());
			return;
		}
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
		// All-zeros = anonymous (session-only inventory, no dup check).
		// Two live connections sharing one persisted inventory would stomp
		// each other's copy — refuse the second.
		let identity = (identity != [0; 16]).then(|| hash_identity(&identity));
		if let Some(hash) = identity {
			if self.clients.values().any(|c| c.identity == Some(hash)) {
				refuse(&out, "this identity is already connected".into());
				return;
			}
		}
		let record = identity
			.map(|hash| self.player_store.get(&hash))
			.unwrap_or_default();
		let inventory = record.inventory;
		// The item arrangement: the stored layout when one exists, else
		// the deterministic default — the SAME derivation the client runs
		// from the `Layout` echo / its `InventoryFull`, so both sides
		// agree without a round-trip.
		let (mut hotbar, mut grid) = match &record.layout {
			Some(l) => (Hotbar::from_wire(&l.hotbar), InvGrid::from_wire(&l.grid)),
			None => (Hotbar::new(), InvGrid::default()),
		};
		reconcile_layout(&inventory, &mut hotbar, &mut grid);
		let name = sanitize_name(&name);
		let draw_dist = draw_dist.clamp(DRAW_DIST_MIN, DRAW_DIST_MAX);
		// Blocks built at the spawn point since worldgen raise the landing.
		let spawn = self.safe_spawn();
		let mut ok = send_frame(
			&out,
			&ServerMsg::Welcome {
				protocol: PROTOCOL_VERSION,
				seed: self.store.seed(),
				player_id: id,
				spawn: spawn.to_array(),
				server_tick: self.tick,
				snapshot_divisor: SNAPSHOT_DIVISOR,
				// Issued now so resume can be added without a wire change;
				// server-side session storage arrives with that feature.
				session_token: rand::random(),
				sim_version: digblock_core::SIM_VERSION,
				build: env!("CARGO_PKG_VERSION").into(),
				tod: self.current_tod(),
			},
		);

		// Join sync: final cell state for every edited column in range.
		let center = column_of(spawn);
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
		// The authoritative inventory base — explicit even when empty
		// (anonymous joins), so the client never guesses.
		ok &= send_frame(
			&out,
			&ServerMsg::InventoryFull {
				counts: inventory.to_wire(),
			},
		);
		// The item arrangement (stored or derived — always sent, so the
		// client adopts exactly what enforcement will validate against).
		ok &= send_frame(
			&out,
			&ServerMsg::Layout {
				hotbar: hotbar.to_wire().to_vec(),
				grid: grid.to_wire().to_vec(),
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
		// The server's command set, so the chat box can autocomplete —
		// admin only (nobody else can use them, or needs to see them).
		let is_admin = self
			.admin_name
			.as_deref()
			.is_some_and(|a| name.eq_ignore_ascii_case(a));
		if is_admin {
			send_frame(&out, &admin::commands_msg());
		}
		self.clients.insert(
			conn_id,
			Client {
				id,
				name,
				out,
				addr,
				dead: Cell::new(false),
				dgram_full_since: Cell::new(None),
				player: Player::new(spawn),
				look: Vec3::Z,
				last_intent: PlayerIntent::default(),
				inputs: BTreeMap::new(),
				last_input_tick: 0,
				drift: Vec3::ZERO,
				edit_tokens: EDIT_BURST,
				chat_tokens: CHAT_BURST,
				draw_dist,
				synced_columns: synced,
				needs_sync_scan: false,
				live: false,
				center,
				inventory,
				hotbar,
				grid,
				active_slot: 0,
				identity,
				digging: None,
				last_return_tick: None,
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
		self.lobby.retain(|conn_id, (out, since, _addr)| {
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
			if let Some(hash) = client.identity {
				// Durable before the identity can reconnect and re-load.
				self.player_store.put(hash, client.record());
				self.player_store.sync();
			}
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
	fn welcome_anchors_tod_and_heartbeat_advances_it() {
		let mut sim = flat_sim("tod");
		let mut c = connect(&mut sim, 1);
		let welcome_tod = c
			.msgs
			.iter()
			.find_map(|m| match m {
				ServerMsg::Welcome { tod, .. } => Some(*tod),
				_ => None,
			})
			.expect("Welcome");
		// Joined near tick 0: the clock sits at the fresh-world start.
		assert!(
			(welcome_tod - digblock_core::sun::TOD_START).abs() < 0.1,
			"welcome tod {welcome_tod}"
		);
		for _ in 0..super::TOD_SYNC_TICKS {
			sim.tick_once(vec![]);
		}
		c.drain();
		let beat = c
			.msgs
			.iter()
			.find_map(|m| match m {
				ServerMsg::TimeOfDay { tod } => Some(*tod),
				_ => None,
			})
			.expect("heartbeat within one sync period");
		// 10 s at 0.3°/s — the sun moved ~3° toward noon (tod falls).
		assert!(
			(welcome_tod - beat - 3.0).abs() < 0.1,
			"welcome {welcome_tod} → beat {beat}"
		);
	}

	#[test]
	fn set_draw_dist_updates_and_clamps() {
		let mut sim = flat_sim("setdraw");
		let _c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		assert_eq!(sim.test_draw_dist(1), Some(4), "harness Hello value");

		let msg = |draw_dist| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::SetDrawDist { draw_dist },
		};
		sim.tick_once(vec![msg(12)]);
		assert_eq!(sim.test_draw_dist(1), Some(12));
		sim.tick_once(vec![msg(1000)]);
		assert_eq!(sim.test_draw_dist(1), Some(super::DRAW_DIST_MAX));
		sim.tick_once(vec![msg(-3)]);
		assert_eq!(sim.test_draw_dist(1), Some(super::DRAW_DIST_MIN));

		// Unknown connection: silently ignored.
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 99,
			msg: ClientMsg::SetDrawDist { draw_dist: 8 },
		}]);
	}

	#[test]
	fn version_mismatch_refused_and_closed() {
		let refused_with = |protocol: u16, sim_version: u16| -> (Vec<ServerMsg>, bool) {
			let mut sim = flat_sim("refuse");
			let (tx, rx) = channel(super::OUT_CAP);
			sim.tick_once(vec![
				SimEvent::Connected {
					conn_id: 9,
					out: tx,
					addr: test_addr(9),
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
						identity: [0; 16],
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
			addr: test_addr(7),
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
				identity: [0; 16],
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
					slot: 0,
				},
			})
			.collect();
		sim.tick_once(flood);
		assert!(
			sim.action_queue.is_empty(),
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
				addr: test_addr(5),
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
					identity: [0; 16],
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

	/// Inventory persists across reconnect AND across a full server
	/// rebuild on the same data dir, keyed by the hashed identity.
	#[test]
	fn identity_inventory_persists_across_reconnect_and_reboot() {
		use digblock_core::block::BlockType;
		use digblock_net::messages::EditKind;
		use glam::Vec3;

		let dir = temp_dir("persist");
		let identity = [7u8; 16];
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		{
			let mut sim =
				super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
			let _c = connect_with_identity(&mut sim, 1, identity);
			wait_live(&mut sim, 1);
			let mut t = 0;
			dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
			assert_eq!(sim.test_inventory_count(1, BlockType::Stone), 1);

			// Park the Stone in grid slot 17 (a deliberate arrangement —
			// it must survive reconnect and reboot verbatim).
			let mut grid =
				vec![digblock_core::hotbar::WIRE_EMPTY; digblock_core::inv_grid::GRID_SLOTS];
			grid[17] = BlockType::Stone as u16;
			sim.tick_once(vec![SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Layout {
					hotbar: vec![digblock_core::hotbar::WIRE_EMPTY; 8]
						.into_iter()
						.chain([digblock_core::hotbar::WIRE_TOOL])
						.collect(),
					grid: grid.clone(),
				},
			}]);

			// Reconnect within the same server run.
			sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
			let mut c2 = connect_with_identity(&mut sim, 2, identity);
			c2.drain();
			assert!(
				c2.msgs.iter().any(|m| matches!(
					m,
					ServerMsg::InventoryFull { counts }
						if counts == &vec![(BlockType::Stone as u16, 1)]
				)),
				"reconnect carries the credit: {:?}",
				c2.msgs
			);
			assert!(
				c2.msgs.iter().any(|m| matches!(
					m,
					ServerMsg::Layout { grid: g, .. } if g == &grid
				)),
				"reconnect carries the arrangement: {:?}",
				c2.msgs
			);
			sim.tick_once(vec![SimEvent::Disconnected { conn_id: 2 }]);
			sim.shutdown();
		}
		// Full reboot on the same data dir.
		let mut sim =
			super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let mut c3 = connect_with_identity(&mut sim, 3, identity);
		c3.drain();
		assert!(
			c3.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::InventoryFull { counts }
					if counts == &vec![(BlockType::Stone as u16, 1)]
			)),
			"reboot carries the credit: {:?}",
			c3.msgs
		);
		assert!(
			c3.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::Layout { grid, .. }
					if grid.get(17) == Some(&(BlockType::Stone as u16))
			)),
			"reboot carries the arrangement: {:?}",
			c3.msgs
		);
		// A different identity starts empty.
		let mut c4 = connect_with_identity(&mut sim, 4, [9u8; 16]);
		c4.drain();
		assert!(
			c4.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::InventoryFull { counts } if counts.is_empty()
			)),
			"{:?}",
			c4.msgs
		);
	}

	/// A live identity refuses a second connection; anonymous (all-zero)
	/// identities coexist freely.
	#[test]
	fn duplicate_identity_refused_anonymous_pairs_ok() {
		let mut sim = flat_sim("dupid");
		let _a = connect_with_identity(&mut sim, 1, [5u8; 16]);
		let dup = try_connect(&mut sim, 2, [5u8; 16]);
		assert!(
			matches!(
				dup.msgs.first(),
				Some(ServerMsg::Refused { reason }) if reason.contains("identity")
			),
			"{:?}",
			dup.msgs
		);
		// Anonymous pair: both welcomed (connect asserts Welcome).
		let _b = connect(&mut sim, 3);
		let _c = connect(&mut sim, 4);
		assert_eq!(sim.player_count(), 3);

		// The refused identity can join once the holder leaves.
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
		let _d = connect_with_identity(&mut sim, 5, [5u8; 16]);
		assert_eq!(sim.player_count(), 3);
	}

	#[test]
	fn return_to_spawn_teleports_with_cooldown() {
		use digblock_net::protocol::{RETURN_SPAWN_COOLDOWN_SECS, TICK_RATE};
		let request = |sim: &mut super::Sim| {
			sim.tick_once(vec![SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::ReturnToSpawn,
			}]);
		};
		let mut sim = flat_sim("rts");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);

		// Walk away, then return: teleported to spawn (one settle tick of
		// slack — the teleport lands before the tick's physics).
		let mut t = 100;
		for _ in 0..240 {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
			t += 1;
		}
		assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) > 5.0);
		request(&mut sim);
		assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) < 1.0);
		c.drain();
		assert!(
			c.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::Teleported { .. })),
			"client told about the teleport: {:?}",
			c.msgs
		);

		// Inside the cooldown the request is silently ignored.
		for _ in 0..120 {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
			t += 1;
		}
		let away = sim.test_player(1).unwrap().pos;
		assert!(away.distance(FLAT_SPAWN) > 5.0);
		request(&mut sim);
		assert!(
			sim.test_player(1).unwrap().pos.distance(away) < 1.0,
			"cooldown violation ignored"
		);

		// After the window it works again. (Drain as we go — an undrained
		// outbound queue trips the snapshot-stall reaper over 60s.)
		for _ in 0..(RETURN_SPAWN_COOLDOWN_SECS * TICK_RATE) {
			sim.tick_once(vec![]);
			c.drain();
		}
		request(&mut sim);
		assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) < 1.0);
	}

	/// Blocks built at the spawn point raise the landing — for a joiner
	/// AND a returning player — even when nobody is near spawn (the scan
	/// reads the journal's in-memory cells, not resident chunks).
	#[test]
	fn blocked_spawn_raises_the_landing() {
		use digblock_net::messages::BlockEdit;
		let dir = temp_dir("blockedspawn");
		{
			let mut j = crate::journal::Journal::open(&dir, SEED).unwrap();
			// Two stone blocks stacked through the spawn capsule.
			for y in [FLAT_GROUND_TOP as i32 + 1, FLAT_GROUND_TOP as i32 + 2] {
				j.append(
					BlockEdit {
						x: 8,
						y,
						z: 8,
						block: digblock_core::block::BlockType::Stone as u16,
					},
					0,
				);
			}
			j.sync();
		}
		let mut sim =
			super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let c = connect(&mut sim, 1);
		let spawn = c
			.msgs
			.iter()
			.find_map(|m| match m {
				ServerMsg::Welcome { spawn, .. } => Some(*spawn),
				_ => None,
			})
			.expect("Welcome");
		assert!(
			spawn[1] >= FLAT_SPAWN.y + 2.0,
			"join landed on top of the tower: y {} vs base {}",
			spawn[1],
			FLAT_SPAWN.y
		);
		assert_eq!(sim.test_player(1).unwrap().pos.to_array(), spawn);
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

	#[test]
	fn chat_broadcasts_to_all_including_sender() {
		let mut sim = flat_sim("chat");
		let mut a = connect(&mut sim, 1); // id 0
		let mut b = connect(&mut sim, 2); // id 1
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Chat {
				text: "hello world".into(),
			},
		}]);
		for c in [&mut a, &mut b] {
			c.drain();
			assert!(
				c.msgs.iter().any(|m| matches!(
					m,
					ServerMsg::Chat { from: 0, text } if text == "hello world"
				)),
				"chat missing (sender echo included): {:?}",
				c.msgs
			);
		}
	}

	#[test]
	fn chat_flood_is_bounded_by_the_token_bucket() {
		let mut sim = flat_sim("chatflood");
		let mut a = connect(&mut sim, 1);
		let flood: Vec<SimEvent> = (0..20)
			.map(|i| SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Chat {
					text: format!("spam {i}"),
				},
			})
			.collect();
		sim.tick_once(flood);
		a.drain();
		let heard = a
			.msgs
			.iter()
			.filter(|m| matches!(m, ServerMsg::Chat { .. }))
			.count();
		assert_eq!(
			heard,
			digblock_net::protocol::CHAT_BURST as usize,
			"flood must be capped at the burst, silently: {:?}",
			a.msgs
		);
	}

	#[test]
	fn hostile_chat_is_sanitized() {
		let mut sim = flat_sim("chatsan");
		let mut a = connect(&mut sim, 1);
		let long = "y".repeat(1000);
		sim.tick_once(vec![
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Chat {
					text: "a\u{7}b\u{1F600}c".into(),
				},
			},
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Chat {
					text: "   \t  ".into(),
				},
			},
			SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Chat { text: long },
			},
		]);
		a.drain();
		let texts: Vec<&str> = a
			.msgs
			.iter()
			.filter_map(|m| match m {
				ServerMsg::Chat { text, .. } => Some(text.as_str()),
				_ => None,
			})
			.collect();
		// The whitespace-only line vanished (though it cost a token);
		// the others arrive cleaned and capped.
		assert_eq!(texts.len(), 2, "{texts:?}");
		assert_eq!(texts[0], "abc");
		assert_eq!(texts[1].len(), digblock_net::protocol::MAX_CHAT_LEN);
		assert!(texts.iter().all(|t| t.chars().all(|c| !c.is_control())));
	}

	#[test]
	fn chat_before_hello_is_ignored() {
		let mut sim = flat_sim("chatlobby");
		let (tx, mut rx) = channel(super::OUT_CAP);
		sim.tick_once(vec![
			SimEvent::Connected {
				conn_id: 9,
				out: tx,
				addr: test_addr(9),
			},
			SimEvent::Msg {
				conn_id: 9,
				msg: ClientMsg::Chat {
					text: "premature".into(),
				},
			},
		]);
		assert!(
			matches!(
				rx.try_recv(),
				Err(tokio::sync::mpsc::error::TryRecvError::Empty)
			),
			"lobby connection must hear nothing back"
		);
	}
}
