//! The authoritative simulation: one instance, one thread, one fixed tick
//! rate (`TICK_RATE`).
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
use crate::player_store::{IdentityHash, PlayerLayout, PlayerRecord, PlayerStore, hash_identity};
use crate::spawn_store::{SpawnLock, SpawnStore};
use crate::store::{ChunkStore, ColumnSource};
use digblock_core::fluid::FluidSim;
use digblock_core::hotbar::Hotbar;
use digblock_core::inv_grid::{InvGrid, reconcile_layout};
use digblock_core::inventory::Inventory;
use digblock_core::player::{self, Player, PlayerIntent, PlayerState};
use digblock_core::protect::Region;
use digblock_net::codec;
use digblock_net::messages::{
	ClientMsg, InputBatch, InputTick, RejectReason, RemoteState, SelfState, ServerMsg,
};
use digblock_net::protocol::{
	CHAT_BURST, EDIT_BURST, MIN_PROTOCOL, PROTOCOL_VERSION, PlayerId, RETURN_SPAWN_COOLDOWN_SECS,
	SERVER_ID, SNAPSHOT_DIVISOR, TICK_RATE, TOD_SYNC_TICKS, sanitize_chat, sanitize_name,
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
/// dropped after this many ticks — mirrors the preamble deadline. Without
/// it a stalled handshake camps in the lobby forever: the server's own
/// keep-alives defeat QUIC's idle timeout, and the player-id cap applies
/// only post-Hello.
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
	/// A length-prefixed reliable frame, SHARED between recipients.
	///
	/// `Arc` rather than `Vec` because the interesting sends are
	/// broadcasts: every landed edit goes to every client whose
	/// `synced_columns` covers it, and fluid alone lands a whole
	/// `FLOW_BUDGET` of edits every flow step. Encoding per recipient meant
	/// several allocations and a fresh postcard pass PER RECIPIENT,
	/// multiplied by the crowd — tens of thousands of allocations a second
	/// around a waterfall, all producing byte-identical output. Now the
	/// fan-out is a refcount bump.
	Frame(std::sync::Arc<[u8]>),
	/// Ask the net layer to close the connection (refused handshake).
	Close,
}

/// Per-client input-path instrumentation. Purely diagnostic — no field
/// here feeds a decision, so sampling or ignoring it changes nothing.
///
/// It exists because the intake path used to drop inputs three different
/// ways in silence (below the high-water mark, evicted at `INPUT_BUF_CAP`,
/// or never delivered at all), which made every claim about jitter
/// unfalsifiable. `late_lost` is the one counter that means an input was
/// lost for good.
#[derive(Default, Clone, Copy, Debug)]
pub struct InputStats {
	/// Inputs actually applied, ever. The netbench divides this by what the
	/// client predicted, to measure how well `INPUT_REDUNDANCY` covers real
	/// datagram loss.
	pub consumed: u64,
	/// Rejected below the high-water mark having already been held once: a
	/// redundancy copy doing its job. Expected and free.
	pub dup: u64,
	/// Rejected below the high-water mark having NEVER been held — the
	/// buffer drained past this tick before it arrived. Lost motion.
	pub late_lost: u64,
	/// The deepest any late-lost input would have needed the buffer to be
	/// (`last_input_tick - input.tick + 1`), i.e. how much depth would have
	/// been required to still be holding it. The tuning number, if the
	/// buffer ever becomes adaptive.
	pub late_want_max: u32,
	/// Ticks the drain found the buffer empty and coasted on the last
	/// intent instead of consuming an input.
	pub underruns: u64,
	/// Ticks the drain deliberately consumed nothing while holding a hole
	/// in the tick sequence open (see `INPUT_HOLD_MAX`). Physically
	/// identical to an underrun — the difference is that the buffer keeps
	/// its contents and the high-water mark does not advance, which is what
	/// lets the late input still be admitted.
	pub held: u64,
	/// Inputs discarded because the client was frozen on an unloaded
	/// column. Deliberately NOT counted as `late_lost`: nothing about the
	/// wire went wrong, the server simply had nowhere to put the player.
	pub frozen_dropped: u64,
	/// Inputs swept by the `INPUT_BUF_CAP` overflow trim.
	pub evicted: u64,
	/// Buffer depth sampled once per drain, as the tick SPAN
	/// (`admit_hi - last_input_tick`) rather than `inputs.len()`: datagram
	/// loss punches holes in the map, so the count under-reads the real
	/// depth precisely when the link is worst.
	pub depth_sum: u64,
	pub depth_samples: u64,
	pub depth_max: u32,
}

impl InputStats {
	/// Mean buffer depth in ticks over the sampled drains.
	pub fn depth_mean(&self) -> f64 {
		if self.depth_samples == 0 {
			0.0
		} else {
			self.depth_sum as f64 / self.depth_samples as f64
		}
	}
}

/// Per-client edit-path instrumentation. Purely diagnostic, like
/// [`InputStats`], and deliberately a separate struct — this measures what
/// a CLICK costs, not what the input buffer does.
///
/// It exists to price the two constants that are sized against how far the
/// server's simulated player trails the client's: [`EDIT_POS_TOLERANCE`]
/// and [`digblock_net::protocol::dig_grace`]. Both are fixed numbers whose
/// remaining headroom has never been measured, and any change that makes
/// the server consume inputs later spends that headroom.
#[derive(Default, Clone, Copy, Debug)]
pub struct EditStats {
	/// `req.origin.distance(player_pos)` at the tolerance check, in blocks
	/// — sampled BEFORE the reject, so a rejected request is counted too.
	/// This is the quantity [`EDIT_POS_TOLERANCE`] bounds.
	pub origin_sum: f64,
	pub origin_samples: u64,
	pub origin_max: f32,
	/// Requests refused by that check specifically, kept apart from the
	/// other `RejectReason::OutOfReach` paths (a missed ray, a bad dir) so
	/// "the tolerance is too tight" is distinguishable from "the client
	/// aimed at nothing".
	pub tol_rejects: u64,
	/// Completing digs the timed-dig gate accepted.
	pub dig_ok: u64,
	/// Completing digs refused for insufficient banked progress, and the
	/// worst shortfall in ticks — the direct read on whether `dig_grace`
	/// still has room to give.
	pub dig_short: u64,
	pub dig_short_max: u32,
}

impl EditStats {
	/// Mean claimed-origin residual in blocks over the sampled requests.
	pub fn origin_mean(&self) -> f64 {
		if self.origin_samples == 0 {
			0.0
		} else {
			self.origin_sum / self.origin_samples as f64
		}
	}
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
	/// Highest input tick ever ADMITTED to the buffer, and a bitmap of the
	/// ticks below it (bit n = `admit_hi - n`). Together they separate an
	/// ordinary redundancy copy of a tick we already held — expected, free,
	/// every tick rides `INPUT_REDUNDANCY` datagrams — from a tick the
	/// buffer had already drained past when it arrived, which is the only
	/// rejection that means an input was lost for good. Diagnostic only:
	/// nothing reads these back into a decision.
	admit_hi: u32,
	admit_seen: u64,
	/// Consecutive ticks the drain has been holding the current hole open.
	/// Reset by any consume, so the `INPUT_HOLD_MAX` cap is per-hole rather
	/// than cumulative — a bad link cannot ratchet into a standing stall.
	hold_ticks: u32,
	stats: InputStats,
	edits: EditStats,
	/// EWMA of adopted (claim − sim) deltas: the sustained-drift gate.
	drift: Vec3,
	edit_tokens: f32,
	chat_tokens: f32,
	/// Bench-extraction bucket, in TICKS: refilled one per tick, capped
	/// at `extract_burst_ticks(level)` (two whole blocks' worth, or one
	/// costliest extraction where that is more — an op no bucket can hold
	/// is an op that is refused forever), and
	/// spent at the extracted block's own `extract_pace_ticks` — the
	/// server-enforced half of the drip (see `process_alchemy`).
	extract_tokens: f32,
	draw_dist: i32,
	synced_columns: FxHashSet<(i32, i32)>,
	/// Journal columns owed to this client, drained a few per tick by
	/// `pump_join_sync`. A join enqueues its whole in-range set here rather
	/// than encoding it inline; `sync_queue_set` is the membership test that
	/// keeps a re-scan from queueing the same column twice.
	sync_queue: std::collections::VecDeque<(i32, i32)>,
	sync_queue_set: FxHashSet<(i32, i32)>,
	/// Run the full edited-column sync scan for this client next tick.
	/// Set on center change and on going live; steady state does ZERO
	/// journal scans (fresh columns arrive via the first-edit broadcast).
	needs_sync_scan: bool,
	live: bool,
	center: (i32, i32),
	/// This client is mid-ARRIVAL — it joined or was teleported and is
	/// holding its world (no render, no player sim) until every journaled
	/// column within `AREA_SYNC_RADIUS` of the new centre has been sent.
	/// Cleared by emitting `ServerMsg::AreaReady`, which is the only thing
	/// that releases it short of the client's own timeout.
	///
	/// Set at the arrival rather than counted at the scan because the two
	/// happen a tick apart: a teleport moves the position now, and
	/// `update_residency` only notices the new centre next tick.
	arrival_hold: bool,
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
	/// Per-skill XP totals — the authoritative copy. Awards happen at
	/// the two chokepoints (dig credit, alchemy) and reach the client as
	/// `ServerMsg::XpGain` totals; levels derive from the shared curve.
	skills: digblock_core::skills::SkillSet,
}

impl Client {
	/// The held item for snapshots, in hotbar wire encoding. The tool
	/// reports its charge too — a bucket is a different thing full than
	/// empty, and the difference is the whole point of holding one.
	///
	/// WHICH tool, though, is the second axis: the slot only ever says
	/// `WaterTool`, so reporting the slot drew every remote player's
	/// Resonant Pick as a bucket — or, if their bucket happened to be
	/// charged, as a cube of water. The encoding needed nothing new for
	/// it: tools are real block ids, so this sends the tool's own id and
	/// keeps the sentinels for the bucket, exactly as `Hotbar::to_wire`
	/// does on the Layout wire.
	fn held_wire(&self) -> u16 {
		use digblock_core::block::BlockType;
		use digblock_core::hotbar::{HOTBAR_SLOTS, HotbarItem, WIRE_EMPTY, WIRE_TOOL};
		let slot = (self.active_slot as usize).min(HOTBAR_SLOTS - 1);
		let tool = self.hotbar.active_tool();
		match self.hotbar.slots()[slot] {
			None => WIRE_EMPTY,
			Some(HotbarItem::WaterTool) if tool != BlockType::Bucket => tool as u16,
			Some(HotbarItem::WaterTool) if self.inventory.bucket_full() => {
				digblock_net::messages::HELD_TOOL_FULL
			}
			Some(HotbarItem::WaterTool) => WIRE_TOOL,
			Some(HotbarItem::Block(b)) => b as u16,
		}
	}

	/// The persistence snapshot (written through on edits + disconnect).
	///
	/// Position rides along so a reconnect resumes in place rather than
	/// handing out a free trip to spawn, and the return-to-spawn tick with
	/// it so the cooldown can't be reset by rejoining either.
	fn record(&self) -> PlayerRecord {
		PlayerRecord {
			inventory: self.inventory.clone(),
			layout: Some(PlayerLayout {
				hotbar: self.hotbar.to_wire(),
				grid: self.grid.to_wire(),
			}),
			pos: Some(self.player.pos.to_array()),
			last_return_tick: self.last_return_tick,
			skills: self.skills.clone(),
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
	/// The seed-derived spawn, fixed for the life of the world. Every
	/// resolve measures from here, never from the last answer, so a
	/// griefer can push spawn aside but never walk it across the map.
	spawn_anchor: Vec3,
	/// False for the synthetic-column test seam: the edit-aware search
	/// reads real worldgen, which a flat test world does not agree with,
	/// so those sims keep their injected spawn verbatim.
	spawn_search: bool,
	/// Persistent spawn lock (`/lock-spawn`).
	spawn_store: SpawnStore,
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
	/// Scratch for `send_snapshots`, reused across ticks: the shared
	/// roster, the per-client candidate list, the selected-player Vec, and
	/// the postcard encode buffer. The pass runs every `SNAPSHOT_DIVISOR`
	/// ticks over every client, so building these fresh was the sim
	/// thread's largest steady-state allocation source.
	snap_roster: Vec<(RemoteState, SelfState, (i32, i32))>,
	snap_nearby: Vec<(i64, usize)>,
	snap_players: Vec<RemoteState>,
	snap_buf: Vec<u8>,
	/// Persistent per-identity player records — inventory + item layout
	/// (separate from the journal).
	player_store: PlayerStore,
	/// Persistent IP bans, enforced at Hello and driven by `/ban`.
	ban_store: BanStore,
	/// The player name (case-insensitive) granted admin chat commands.
	admin_name: Option<String>,
	/// Seconds added to the tick clock by `/time` — same lifetime and
	/// persistence policy as the tick counter it offsets (none).
	tod_offset_secs: f64,
}

impl Sim {
	pub fn new(
		seed: f64,
		data_dir: &Path,
		sim_radius: i32,
		workers: usize,
	) -> std::io::Result<Self> {
		let store = ChunkStore::new(seed, workers);
		let mut sim = Self::with_store(seed, data_dir, sim_radius, store, player::spawn(seed))?;
		sim.spawn_search = true;
		Ok(sim)
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
		let spawn_store = SpawnStore::open(data_dir)?;
		Ok(Self {
			tick: 0,
			store,
			journal,
			fluid: FluidSim::default(),
			flow_accum: 0.0,
			sim_radius,
			spawn_anchor: spawn,
			spawn_search: false,
			spawn_store,
			clients: FxHashMap::default(),
			lobby: FxHashMap::default(),
			action_queue: Vec::new(),
			residency_dirty: true,
			face_scans: std::collections::VecDeque::new(),
			last_oversize_log: None,
			snap_roster: Vec::new(),
			snap_nearby: Vec::new(),
			snap_players: Vec::new(),
			snap_buf: Vec::new(),
			player_store,
			ban_store,
			admin_name: None,
			tod_offset_secs: 0.0,
		})
	}

	/// Grant a player name (case-insensitive) the admin chat commands.
	/// Empty counts as unset, like the env fallbacks.
	///
	/// A name that could never be a real display name is REFUSED rather
	/// than stored: it can only ever match nobody, so keeping it would
	/// leave a server with admin silently disabled while its boot log said
	/// otherwise. See [`admin_name_is_usable`].
	pub fn set_admin_name(&mut self, name: Option<String>) {
		self.admin_name = name
			.filter(|n| !n.is_empty())
			.filter(|n| admin_name_is_usable(n).is_ok());
	}

	pub fn server_tick(&self) -> u32 {
		self.tick
	}

	/// The authoritative world clock: a pure function of the tick
	/// counter plus the `/time` offset, so it needs no persistence (a
	/// restart restarts the day — accepted).
	fn current_tod(&self) -> f32 {
		digblock_core::sun::Sun::tod_of_secs(
			f64::from(self.tick) / f64::from(TICK_RATE) + self.tod_offset_secs,
		)
	}

	/// Read each tick by `run_sim_loop` into the shared gauge the master
	/// heartbeat reports.
	pub fn player_count(&self) -> usize {
		self.clients.len()
	}

	/// Run one tick with the events drained since the last one.
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
		if self.tick.is_multiple_of(SNAPSHOT_DIVISOR) {
			self.send_snapshots();
		}
		if self.tick.is_multiple_of(TOD_SYNC_TICKS) {
			let msg = ServerMsg::TimeOfDay {
				tod: self.current_tod(),
			};
			self.broadcast(&msg);
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

	/// Read a resident cell. Public for the netbench, whose client has to
	/// predict against the same world the server is simulating.
	pub fn cell_at(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.store.cell_at(x, y, z)
	}

	/// A client's authoritative position — the netbench compares its
	/// predicted position against this to measure divergence.
	pub fn player_pos(&self, conn_id: u64) -> Option<Vec3> {
		self.clients.get(&conn_id).map(|c| c.player.pos)
	}

	/// A client's full authoritative state. The netbench re-seeds its local
	/// prediction from this each tick, the way a real client rewinds to the
	/// server on reconcile.
	pub fn player_state(&self, conn_id: u64) -> Option<PlayerState> {
		self.clients.get(&conn_id).map(|c| c.player.state())
	}

	/// Inputs this client has had applied, ever.
	pub fn inputs_consumed(&self, conn_id: u64) -> u64 {
		self.clients.get(&conn_id).map_or(0, |c| c.stats.consumed)
	}

	/// This client's input-path counters. See [`InputStats`].
	pub fn input_stats(&self, conn_id: u64) -> InputStats {
		self.clients
			.get(&conn_id)
			.map_or_else(Default::default, |c| c.stats)
	}

	/// This client's edit-path counters. See [`EditStats`].
	pub fn edit_stats(&self, conn_id: u64) -> EditStats {
		self.clients
			.get(&conn_id)
			.map_or_else(Default::default, |c| c.edits)
	}

	/// Record what the claimed edit origin cost against the server's own
	/// position. Called at BOTH `EDIT_POS_TOLERANCE` sites (the edit path
	/// and the interact path) before either rejects, so the sample is the
	/// distribution the constant is actually bounding.
	fn note_origin_residual(&mut self, conn_id: u64, residual: f32, rejected: bool) {
		let Some(c) = self.clients.get_mut(&conn_id) else {
			return;
		};
		c.edits.origin_sum += f64::from(residual);
		c.edits.origin_samples += 1;
		c.edits.origin_max = c.edits.origin_max.max(residual);
		c.edits.tol_rejects += u64::from(rejected);
	}

	/// Record the timed-dig gate's verdict. `short` is how many banked
	/// aim-verified ticks the completion was missing after `dig_grace`;
	/// zero means it was accepted.
	fn note_dig_gate(&mut self, conn_id: u64, short: u32) {
		let Some(c) = self.clients.get_mut(&conn_id) else {
			return;
		};
		if short == 0 {
			c.edits.dig_ok += 1;
		} else {
			c.edits.dig_short += 1;
			c.edits.dig_short_max = c.edits.dig_short_max.max(short);
		}
	}

	/// Write a cell straight into the resident store, bypassing edit
	/// validation — for staging terrain a test needs to exist.
	#[cfg(test)]
	fn test_set_cell(&mut self, x: i32, y: i32, z: i32, cell: u16) {
		self.store.set_cell(x, y, z, cell);
	}

	/// Put a client somewhere, bypassing the admin command — for staging a
	/// spatial arrangement a test needs, without walking there.
	#[cfg(test)]
	fn test_teleport(&mut self, conn_id: u64, pos: Vec3) {
		if let Some(c) = self.clients.get_mut(&conn_id) {
			c.player.set_state(digblock_core::player::PlayerState {
				pos,
				vel: Vec3::ZERO,
			});
			c.center = column_of(pos);
		}
	}

	/// The hand-picked spawn (`/set-spawn`), if one is stored.
	#[cfg(test)]
	fn test_spawn_point(&self) -> Option<Vec3> {
		self.spawn_store.point().map(Vec3::from_array)
	}

	/// The stored spawn facing (`/set-spawn-dir`), if one is set.
	#[cfg(test)]
	fn test_spawn_dir(&self) -> Option<Vec3> {
		self.spawn_store.dir().map(Vec3::from_array)
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

	/// The client's Mining level (the base level for an unknown conn).
	#[cfg(test)]
	pub(crate) fn test_mining_level(&self, conn_id: u64) -> u32 {
		self.clients
			.get(&conn_id)
			.map_or(1, |c| c.skills.level(digblock_core::skills::Skill::Mining))
	}

	/// Raise a client's Mining to `level` (total-XP floor — never lowers,
	/// like the wire's max-total rule). Harness/bench support: edit-path
	/// digs must clear the strength wall without earning through it (the
	/// gate's own test drives DigStart raw). Not #[cfg(test)] because
	/// netbench — a bin, not a test — drives a real in-process Sim whose
	/// flat world is stone.
	pub fn bench_set_mining_level(&mut self, conn_id: u64, level: u32) {
		use digblock_core::skills::{Skill, total_xp_for};
		if let Some(c) = self.clients.get_mut(&conn_id) {
			c.skills.set_total(Skill::Mining, total_xp_for(level));
		}
	}

	/// Put a tool in a client's hand. The tool slot's Layout entry is the
	/// second axis (`Hotbar::active_tool`), so granting the item is not
	/// the same as holding it — which is exactly the distinction the
	/// siphon gate is about.
	#[cfg(test)]
	pub(crate) fn bench_set_active_tool(
		&mut self,
		conn_id: u64,
		tool: digblock_core::block::BlockType,
	) {
		if let Some(c) = self.clients.get_mut(&conn_id) {
			// Owning it is a precondition for holding it — `reconcile_layout`
			// takes back a tool the counts don't cover — so the harness has
			// to hand over the item as well as the slot, exactly like the
			// craft that would have earned it.
			c.inventory.add(tool, 1);
			c.hotbar.cycle_tool(&[tool]);
		}
	}

	/// Lock the spawn region around `eye`, so protection tests can put an
	/// immutable cell wherever they need one.
	#[cfg(test)]
	pub(crate) fn bench_lock_spawn_at(&mut self, eye: Vec3) {
		self.spawn_store
			.set_lock(Some(crate::spawn_store::SpawnLock {
				eye: eye.to_array(),
			}));
	}

	/// A client's XP total in one skill — the two-sided award tests read
	/// it, since a dual award is only interesting as a pair.
	#[cfg(test)]
	pub(crate) fn test_skill_xp(&self, conn_id: u64, skill: digblock_core::skills::Skill) -> u64 {
		self.clients.get(&conn_id).map_or(0, |c| c.skills.xp(skill))
	}

	/// Raise a client's Alchemy to `level` — the craft gate's counterpart
	/// of [`Self::bench_set_mining_level`], for edit-path tests that dig
	/// craft-only blocks (slabs, stairs, doors, torches).
	#[cfg(test)]
	pub(crate) fn bench_set_alchemy_level(&mut self, conn_id: u64, level: u32) {
		use digblock_core::skills::{Skill, total_xp_for};
		if let Some(c) = self.clients.get_mut(&conn_id) {
			c.skills.set_total(Skill::Alchemy, total_xp_for(level));
		}
	}

	/// The cell a client currently holds a dig on, if any.
	#[cfg(test)]
	fn test_digging(&self, conn_id: u64) -> Option<(i32, i32, i32)> {
		self.clients
			.get(&conn_id)
			.and_then(|c| c.digging.as_ref())
			.map(|d| d.cell)
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

	/// Inputs sitting in the jitter buffer right now — how much the drain
	/// has yet to work off.
	#[cfg(test)]
	fn test_input_backlog(&self, conn_id: u64) -> usize {
		self.clients.get(&conn_id).map_or(0, |c| c.inputs.len())
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
					// Sanitize BEFORE deciding what the line IS. These used
					// to read different strings — `is_command` looked at the
					// raw text while everything downstream ran on the
					// sanitized one — and they disagree for any character
					// the sanitizer strips that `trim_start` does not count
					// as whitespace. `U+200B` is exactly that: not
					// `White_Space`, not inside `TEXT_RANGES`. So
					// `"\u{200B}/ban griefer"` read as ordinary chat, then
					// sanitized to `/ban griefer` and was BROADCAST to
					// everyone instead of being privately refused.
					let Some(text) = sanitize_chat(&text) else {
						return;
					};
					// `sanitize_chat` already trimmed.
					let is_command = text.starts_with('/');
					let Some(client) = self.clients.get_mut(&conn_id) else {
						return; // lobby connections don't get to talk
					};
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
					self.broadcast(&msg);
				}
				// Inline like chat: no ordering relationship with the edit
				// stream (edit raycasts re-validate against the player's
				// position when they run anyway).
				ClientMsg::ReturnToSpawn => self.handle_return_to_spawn(conn_id),
				// Inline like chat: sync radius is read fresh each tick, so
				// the change simply applies from the next sync pass.
				//
				// Growing the radius must also REQUEST a scan. The full
				// edited-column walk is event-driven off `needs_sync_scan`,
				// which otherwise only rises when the center column moves —
				// so a player who widened their draw distance standing
				// still got no `JournalColumn` for the newly visible ring,
				// and no live edits there either (`broadcast_edit` gates on
				// `synced_columns`). Other players' builds rendered as
				// pristine terrain until they walked across a column
				// border. Shrinking needs no scan: the prune rides the same
				// flag and costs nothing to defer.
				ClientMsg::SetDrawDist { draw_dist } => {
					if let Some(client) = self.clients.get_mut(&conn_id) {
						let want = draw_dist.clamp(DRAW_DIST_MIN, DRAW_DIST_MAX);
						if want > client.draw_dist {
							client.needs_sync_scan = true;
						}
						client.draw_dist = want;
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

	/// Has this cell ever been written? The ore reveal's anti-dupe gate
	/// (SKILLS-DESIGN §5.3), answered from the journal — the complete and
	/// correct source, fluid included. `JournalIndex::record` only ever
	/// inserts, so the answer is monotone: a spent vein cannot be
	/// un-spent by putting stone back.
	pub(super) fn cell_edited(&self, x: i32, y: i32, z: i32) -> bool {
		self.journal
			.index
			.column(x.div_euclid(16), z.div_euclid(16))
			.is_some_and(|c| c.contains_key(&(x, y, z)))
	}

	/// What a dig on this cell is FOR this client — the same question the
	/// client predicted with, asked of the server's own world.
	pub(super) fn ore_dig_mode(
		&self,
		conn_id: u64,
		cell: (i32, i32, i32),
	) -> digblock_core::worldgen::ore::DigMode {
		use digblock_core::worldgen::ore::DigMode;
		let Some(client) = self.clients.get(&conn_id) else {
			return DigMode::Break;
		};
		let Some(v) = self.store.cell_at(cell.0, cell.1, cell.2) else {
			return DigMode::Break;
		};
		digblock_core::worldgen::ore::dig_mode(
			v,
			cell,
			self.store.seed(),
			client.skills.level(digblock_core::skills::Skill::Mining),
			self.cell_edited(cell.0, cell.1, cell.2),
			client.hotbar.active_tool(),
		)
	}

	/// The EXTRA cells a Resonant Pick hold breaks around this hit, from
	/// the SERVER's own raycast and its own world. Derived rather than
	/// declared: the client sends one `EditRequest` naming one cell, and
	/// this end decides what that swing actually touched.
	pub(super) fn pick_plane(
		&self,
		conn_id: u64,
		hit: &digblock_core::raycast::RayHit,
		dir: [f32; 3],
	) -> Vec<(i32, i32, i32)> {
		let Some(client) = self.clients.get(&conn_id) else {
			return Vec::new();
		};
		if client.hotbar.active_tool() != digblock_core::block::BlockType::ResonantPick {
			return Vec::new();
		}
		let level = client.skills.level(digblock_core::skills::Skill::Mining);
		let id = digblock_core::chunk::cell_id(hit.cell_value);
		// A LOCKED cell is deliberately not filtered here: the assembled
		// writes are scanned for protection and the whole swing is refused,
		// which is what `a_protected_cell_refuses_the_whole_pick_swing`
		// pins ("nothing partially breaks"). The world floor is a different
		// matter and is handled inside the rule — it is not a policy, it is
		// a cell no dig of any kind can touch, and letting the plane reach
		// it made the pick refuse every swing at the bottom of a mine.
		let mut out = digblock_core::pick::cells(
			hit.cell,
			hit.normal,
			dir,
			digblock_core::unlock::pick_cells(level),
			|c| {
				self.store.cell_at(c.0, c.1, c.2).is_some_and(|v| {
					digblock_core::pick::breaks_as_extra(id, v, c, self.ore_dig_mode(conn_id, c))
				})
			},
		);
		out.retain(|&c| c != hit.cell);
		out
	}

	/// Record a hold-to-dig start (called from the ordered queue drain):
	/// capture the cell's CURRENT block — the client never gets to claim
	/// what it is digging. Within a tick the LAST start wins, exactly
	/// like the client's retarget stream — an old first-wins per-tick cap
	/// silently dropped the restart of a cancel+restart pair bunched into
	/// one tick drain, leaving an honest hold with no server record
	/// (DigIncomplete pop-back on completion). Floods still can't spin
	/// the timer: a start on a new cell zeroes progress, and one on the
	/// same cell can only ever RE-READ what the decay has left (see
	/// below) — never add to it. The action-queue cap bounds the
	/// per-tick volume.
	fn handle_dig_start(&mut self, conn_id: u64, cell: (i32, i32, i32)) {
		// Before the client borrow: `spawn_lock` reads all of `self`.
		let lock = self.spawn_lock();
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		if !client.live {
			return;
		}
		// Refuse a protected cell up front. The completing edit would
		// reject it anyway, but only after the player held the button
		// through the block's full dig time for nothing — the refusal is
		// only useful if it arrives when the hold starts.
		if digblock_core::protect::immutable(cell.0, cell.1, cell.2, lock.as_ref()) {
			client.digging = None;
			client_send(
				client,
				&ServerMsg::DigRefused {
					cell: [cell.0, cell.1, cell.2],
					reason: RejectReason::Protected,
				},
			);
			return;
		}
		let cell_value = self.store.cell_at(cell.0, cell.1, cell.2);
		// The skill gates (§5.1's strength wall, v13's craft gate), refused
		// at the hold's start for the same reason protection is: the
		// refusal is useless after the player held the button through a
		// full dig timer. Honest clients predict it and never send this.
		if let Some(v) = cell_value {
			let b = digblock_core::block::BlockType::from_id(digblock_core::chunk::cell_id(v));
			// One question, the same one the client predicted with; the
			// wire reason is all this end adds.
			if let Some(req) = digblock_core::unlock::dig_blocked(b, &client.skills) {
				use digblock_core::unlock::Kind;
				client.digging = None;
				client_send(
					client,
					&ServerMsg::DigRefused {
						cell: [cell.0, cell.1, cell.2],
						reason: match req.kind {
							Kind::Strength => RejectReason::SkillGated,
							Kind::Recipe => RejectReason::NotUnlocked,
							Kind::Tool => RejectReason::NeedsTool,
						},
					},
				);
				return;
			}
		}
		// A revealed vein with the wrong tool refuses here too, for
		// exactly the reason the skill gates do — a refusal that arrives
		// after a two-second hold is not a refusal, it is a punishment.
		// Below the reveal this never fires: that player's world has no
		// vein in it, so the cell digs as the stone it appears to be.
		if self.ore_dig_mode(conn_id, cell) == digblock_core::worldgen::ore::DigMode::NeedsSiphon {
			if let Some(client) = self.clients.get_mut(&conn_id) {
				client.digging = None;
				client_send(
					client,
					&ServerMsg::DigRefused {
						cell: [cell.0, cell.1, cell.2],
						reason: RejectReason::NeedsTool,
					},
				);
			}
			return;
		}
		// The resume's `required` has to be the SAME number the completion
		// gate will check against, or the decay clamps a resumed hold at
		// one figure while the client's bar bleeds from another. That
		// means the ore, not the stone the cell still holds — this arm
		// asked `dig_block` nothing at all and read Stone's hardness for a
		// seam several times harder. Resolved before the client borrow,
		// since it reads the store.
		let mode = self.ore_dig_mode(conn_id, cell);
		let Some(client) = self.clients.get_mut(&conn_id) else {
			return;
		};
		// Re-opening a hold on the SAME unchanged cell resumes it: it is
		// worth whatever the decay has left, which is the number the
		// client's own bar is showing. Everything else starts at zero.
		//
		// `off_target` is deliberately carried across the resume rather
		// than cleared. It is what still limits a start FLOOD: a client
		// spamming DigStart to freeze the decay on a nearly-broken block
		// while looking elsewhere goes on accumulating misses, trips
		// `DIG_OFF_TARGET_RESET`, and bleeds out anyway. An honest
		// resumer is on target, so its first verify tick zeroes this.
		let level = client.skills.level(digblock_core::skills::Skill::Mining);
		let tool = client.hotbar.active_tool();
		let resumed = client
			.digging
			.as_ref()
			.filter(|d| d.cell == cell && Some(d.cell_value) == cell_value)
			.map(|d| {
				let b = digblock_core::worldgen::ore::dig_block(
					mode,
					digblock_core::block::BlockType::from_id(digblock_core::chunk::cell_id(
						d.cell_value,
					)),
				);
				let need = b.dig_hardness(cell.1).map_or(0, |h| {
					digblock_core::skills::dig_required_with(b, h, level, tool)
				});
				(d.banked(need), d.off_target)
			});
		client.digging = match cell_value {
			Some(v)
				if digblock_core::block::BlockType::from_id(digblock_core::chunk::cell_id(v))
					.dig_hardness(cell.1)
					.is_some() =>
			{
				let (progress, off_target) = resumed.unwrap_or((0, 0));
				Some(edits::DigState {
					cell,
					cell_value: v,
					progress,
					off_target,
					idle: 0,
				})
			}
			// Undiggable/unloaded target: no hold. The completing edit
			// will fail its DigIncomplete check.
			_ => None,
		};
	}

	/// The active spawn lock's protected region, if any.
	pub(super) fn spawn_lock(&self) -> Option<Region> {
		self.spawn_store
			.lock()
			.map(|l| digblock_core::spawn::lock_region(Vec3::from_array(l.eye)))
	}

	/// The spawn point to use right now.
	///
	/// Either operator override short-circuits the search. A hand-picked
	/// point (`/set-spawn`) is an explicit decision and stays exactly where
	/// it was put; a locked region is unmodifiable, so it cannot have
	/// stopped being valid and there is nothing to search for. Otherwise
	/// the anchor is re-validated against the journal and the search
	/// relocates sideways if players have dug it out. Stateless on purpose
	/// — a transient hole that happens to be open when someone joins costs
	/// that one player an offset, and the next join snaps back to the
	/// anchor rather than inheriting a stored detour.
	fn spawn_point(&self) -> Vec3 {
		if let Some(eye) = self.spawn_store.pinned() {
			return Vec3::from_array(eye);
		}
		if !self.spawn_search {
			return self.spawn_anchor;
		}
		digblock_core::spawn::resolve(self.spawn_anchor, self.store.seed(), &|x, y, z| {
			self.journal_cell(x, y, z)
		})
	}

	/// Whether `eye` passes every spawn rule as the world stands.
	///
	/// Vacuously true for the synthetic-column test seam: those worlds are
	/// not described by real worldgen, so the pristine half of the test
	/// would reject a spawn that is perfectly good in the world the sim
	/// actually has.
	///
	/// Also vacuously true for a hand-picked spawn, which this predicate
	/// cannot judge and does not need to. It guards `/lock-spawn` against
	/// pinning a spawn the *search* only returned because it gave up; a
	/// point an admin aimed at has no give-up case, was checked against the
	/// real world by [`digblock_core::spawn::stands_clear`] when it was
	/// set, and would be rejected out of hand here for sitting on ground
	/// pristine worldgen never made level.
	fn spawn_valid(&self, eye: Vec3) -> bool {
		self.spawn_store.point().is_some()
			|| !self.spawn_search
			|| digblock_core::spawn::valid(eye, self.store.seed(), &|x, y, z| {
				self.journal_cell(x, y, z)
			})
	}

	/// The world as it actually is at one cell: the resident chunk (which
	/// already carries its journal overlay) falling back to the journal for
	/// columns that are not loaded. `None` = neither can answer.
	///
	/// Raw cells, not `try_from_id`: a state-carrying cell (a stair, a
	/// slab) must read as its real, solid block, not decode to nothing.
	fn world_cell(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.store
			.cell_at(x, y, z)
			.or_else(|| self.journal_cell(x, y, z))
	}

	/// The journal's final value for a cell, or `None` if no player has
	/// ever edited it. Journal ONLY — never the chunk store, which answers
	/// `None` for anything non-resident and would make unloaded columns
	/// read as void. The journal is the complete record: every landed
	/// write, fluid included, is appended to it.
	fn journal_cell(&self, x: i32, y: i32, z: i32) -> Option<u16> {
		self.journal
			.index
			.column(x.div_euclid(16), z.div_euclid(16))
			.and_then(|cells| cells.get(&(x, y, z)).copied())
	}

	/// The facing a spawning player adopts (`/set-spawn-dir`), or
	/// `Vec3::ZERO` for "keep whatever view you have" — the same
	/// convention `Teleported.look` and `Welcome.spawn_look` carry.
	fn spawn_look(&self) -> Vec3 {
		self.spawn_store
			.dir()
			.map(Vec3::from_array)
			.unwrap_or(Vec3::ZERO)
	}

	/// The resolved spawn with any residual obstruction stepped over.
	/// The search already guarantees a clear capsule, so this only bites
	/// on the give-up path where nowhere within range was intact.
	fn safe_spawn(&self) -> Vec3 {
		self.clear_landing(self.spawn_point())
	}

	/// Raise `eye` until the capsule clears both live terrain and
	/// journaled edits — the shared landing rule for spawn and `/tp`.
	fn clear_landing(&self, eye: Vec3) -> Vec3 {
		player::ascend_clear(eye, &|x, y, z| self.world_cell(x, y, z))
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
		// A teleport is an arrival: the destination's journaled columns are
		// no more synced than a join's, and the client would otherwise land
		// on pristine worldgen and be corrected off it. `needs_sync_scan` is
		// set here rather than left to the centre-change check, so a hop
		// within one column still resolves (its scan finds nothing owed and
		// the hold releases the same tick).
		client.arrival_hold = true;
		client.needs_sync_scan = true;
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
		if let Some(t) = client.last_return_tick
			&& self.tick.wrapping_sub(t) < RETURN_SPAWN_COOLDOWN_SECS * TICK_RATE
		{
			return;
		}
		let pos = self.safe_spawn();
		let look = self.spawn_look();
		let tick = self.tick;
		let client = self.clients.get_mut(&conn_id).expect("checked above");
		client.last_return_tick = Some(tick);
		log::info!("player {} ({}) returned to spawn", client.id, client.name);
		self.teleport_client(conn_id, pos, look);
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
		if let Some(hash) = identity
			&& self.clients.values().any(|c| c.identity == Some(hash))
		{
			refuse(&out, "this identity is already connected".into());
			return;
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
		// Resume where they left off. Rejoining used to be a free trip to
		// spawn that sidestepped the return-to-spawn cooldown entirely;
		// stored positions close that, and `last_return_tick` (restored
		// below) stops a rejoin resetting the timer as well.
		//
		// The landing rule still applies: the world may have changed while
		// they were away — someone built on the spot, or an admin locked a
		// region around it — so a resumed position is stepped clear the
		// same way a spawn is. Anonymous joins and pre-v3 records have no
		// stored position and start at spawn.
		// The spawn facing rides along only when this join really is a
		// spawn: it describes which way to look when you arrive THERE, and
		// a player resuming in their own house has already chosen a view.
		let (spawn, spawn_look) = match record.pos {
			Some(p) => (self.clear_landing(Vec3::from_array(p)), Vec3::ZERO),
			None => (self.safe_spawn(), self.spawn_look()),
		};
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
				spawn_look: spawn_look.to_array(),
			},
		);

		// Join sync: final cell state for every edited column in range,
		// QUEUED rather than sent. Encoding them all here meant thousands
		// of frames and megabytes inside one tick's budget on a mature
		// world — a hitch for everyone else on the server, every time anyone
		// joined — and past OUT_CAP the joiner simply lost the race and got
		// dropped. `pump_join_sync` drains the queue a few per tick against
		// the room the outbound queue actually has.
		//
		// Region-indexed, so this is O(columns near the spawn) rather than
		// O(every edited column in the world); the radius test still does
		// the real filtering.
		let center = column_of(spawn);
		let mut cols: Vec<(i32, i32)> = self
			.journal
			.index
			.columns_near(center, draw_dist)
			.filter(|&col| in_radius(col, center, draw_dist))
			.collect();
		// Nearest first. The order used to be whatever the region index
		// yielded, which on a mature world can leave the spawn column
		// itself until late in a stream of thousands — so the ground the
		// player is actually standing on was among the last to arrive.
		// That is what the arrival hold waits on, and sorting is what
		// keeps the wait proportional to the radius rather than to how
		// edited the whole draw distance happens to be.
		cols.sort_by_key(|&col| radius_key(col, center));
		let sync_queue: std::collections::VecDeque<(i32, i32)> = cols.into();
		let sync_queue_set: FxHashSet<(i32, i32)> = sync_queue.iter().copied().collect();
		let queued = sync_queue.len();
		// `JournalDone` carries no ordering obligation the client acts on
		// (it ignores the message), and the inventory and layout below do
		// not depend on the columns — so the handshake completes now and
		// the terrain fills in over the next few ticks.
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
		// Per-skill XP totals — always sent, even empty (a fresh
		// identity sits at the base level everywhere), so the client never
		// guesses.
		ok &= send_frame(
			&out,
			&ServerMsg::Skills {
				xp: record.skills.to_wire(),
			},
		);
		if !ok {
			// The handshake itself no longer competes with the column
			// stream for queue space, so this is now a genuinely broken
			// peer rather than a big world.
			log::warn!(
				"conn {conn_id}: handshake overflowed the outbound queue \
				 (cap {OUT_CAP}) — dropping"
			);
			return;
		}
		if queued > 0 {
			log::debug!("conn {conn_id}: {queued} journal columns queued for sync");
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
				admit_hi: 0,
				admit_seen: 0,
				hold_ticks: 0,
				stats: InputStats::default(),
				edits: EditStats::default(),
				drift: Vec3::ZERO,
				edit_tokens: EDIT_BURST,
				chat_tokens: CHAT_BURST,
				extract_tokens: digblock_core::alchemy::extract_burst_ticks(
					record.skills.level(digblock_core::skills::Skill::Alchemy),
				),
				draw_dist,
				synced_columns: FxHashSet::default(),
				sync_queue,
				sync_queue_set,
				needs_sync_scan: false,
				live: false,
				center,
				// A join is the first arrival. The queue above is already
				// sorted nearest-first and `live` gates the release, so
				// this holds until the spawn's own columns have landed —
				// or releases on the first pump if the spawn is untouched.
				arrival_hold: true,
				inventory,
				hotbar,
				grid,
				active_slot: 0,
				identity,
				digging: None,
				// Restored, not reset: otherwise reconnecting clears the
				// cooldown and the timer is bypassed by rejoining.
				last_return_tick: record.last_return_tick,
				skills: record.skills,
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

	/// Encode once, fan out to every client. Use this for any message going
	/// to more than one recipient — `client_send` re-encodes per call, and
	/// the fan-out paths (edits, chat, time-of-day) are the hot ones.
	fn broadcast(&self, msg: &ServerMsg) {
		self.broadcast_where(msg, |_| true);
	}

	/// Tell everyone BUT the author that they crossed a level — the cue
	/// for the confetti over their head. The author is excluded here
	/// rather than filtered client-side: they already have the `XpGain`
	/// total that implies it, and celebrate off that.
	pub(super) fn announce_level_up(
		&self,
		by: PlayerId,
		skill: digblock_core::skills::Skill,
		level: u32,
	) {
		self.broadcast_where(
			&ServerMsg::LevelUp {
				player: by,
				skill: skill as u8,
				level,
			},
			|c| c.id != by,
		);
	}

	/// [`Self::broadcast`] restricted to clients matching `want`.
	fn broadcast_where(&self, msg: &ServerMsg, want: impl Fn(&Client) -> bool) {
		let Some(frame) = encode_frame(msg) else {
			return;
		};
		for c in self.clients.values() {
			if want(c) {
				client_send_frame(c, &frame);
			}
		}
	}

	fn drop_client(&mut self, conn_id: u64) {
		if let Some(client) = self.clients.remove(&conn_id) {
			log::info!("player {} ({}) left", client.id, client.name);
			if let Some(hash) = client.identity {
				// The record lands in memory immediately, which is what a
				// reconnect reads — `get` never touches the file. Durability
				// rides the ordinary coalesced flush: forcing one here meant
				// a whole-file rebuild + fsync on the sim thread PER
				// disconnect, so a server emptying out paid several inside
				// one tick.
				self.player_store.put(hash, client.record());
				self.player_store.sync_soon();
			}
			self.broadcast(&ServerMsg::PlayerLeft { id: client.id });
			self.residency_dirty = true;
		}
		self.lobby.remove(&conn_id);
	}

	fn free_id(&self) -> Option<PlayerId> {
		let used: FxHashSet<PlayerId> = self.clients.values().map(|c| c.id).collect();
		(0..SERVER_ID).find(|id| !used.contains(id))
	}
}
/// Can this string ever be a player's display name, and therefore ever
/// match the admin check?
///
/// Admin is granted by comparing against `Client::name`, which the master
/// mints and `validate_name` constrains to a short run of `a-z A-Z 0-9 -
/// _`. `--admin-name "big boss"` or a name past that length can therefore
/// never match ANYONE — the server booted with admin quietly disabled
/// while logging a reassuring `admin: big boss`. The launcher now refuses
/// the argument outright, which is what an operator wants from a typo in a
/// security-relevant flag.
///
/// (This validates the SHAPE of the name, nothing more. Names are still
/// self-asserted at the master, so admin remains impersonable until the
/// login flow lands — see NET-AUDIT-2.md §1.1.)
pub fn admin_name_is_usable(name: &str) -> Result<(), &'static str> {
	digblock_net::protocol::validate_name(name)
}

fn column_of(pos: Vec3) -> (i32, i32) {
	(
		(pos.x.floor() as i32).div_euclid(16),
		(pos.z.floor() as i32).div_euclid(16),
	)
}

/// Squared-distance test in i64: `col` is any edited column in the journal
/// and `center` is any client's, so the separation is bounded only by
/// `TP_LIMIT`. In i32 the product overflows well inside that range —
/// release wraps, and a wrapped negative sum passes the test, which syncs
/// a client columns it is nowhere near and force-drops it when the queue
/// refuses; debug panics. The subtraction is widened too:
/// `col.0 - center.0` alone overflows at the i32 extremes.
/// Squared column distance — the ordering key `in_radius` tests against,
/// shared so "near" means the same thing to the sort and to the gate.
fn radius_key(col: (i32, i32), center: (i32, i32)) -> i64 {
	let (dx, dz) = (
		col.0 as i64 - center.0 as i64,
		col.1 as i64 - center.1 as i64,
	);
	dx * dx + dz * dz
}

fn in_radius(col: (i32, i32), center: (i32, i32), r: i32) -> bool {
	let (dx, dz) = (
		col.0 as i64 - center.0 as i64,
		col.1 as i64 - center.1 as i64,
	);
	let r = r as i64;
	dx * dx + dz * dz <= r * r
}

/// Encode one reliable frame into a shareable payload, or `None` if it
/// could not be encoded (a server bug — logged, and treated as "sent" so a
/// peer is never dropped for it).
fn encode_frame(msg: &ServerMsg) -> Option<std::sync::Arc<[u8]>> {
	let mut buf = Vec::new();
	match codec::write_frame(msg, &mut buf) {
		Ok(()) => Some(buf.into()),
		Err(e) => {
			log::error!("frame encode: {e}");
			None
		}
	}
}

/// Queue an already-encoded frame. `false` = the bounded queue refused
/// (full or closed): the peer is not draining and must be dropped.
fn queue_frame(out: &OutTx, frame: &std::sync::Arc<[u8]>) -> bool {
	out.try_send(OutMsg::Frame(frame.clone())).is_ok()
}

/// Encode + queue one reliable frame. `false` = the bounded queue refused
/// (full or closed): the peer is not draining and must be dropped.
///
/// For a message going to more than one client, encode ONCE with
/// [`encode_frame`] and fan out with [`queue_frame`] — see `broadcast`.
fn send_frame(out: &OutTx, msg: &ServerMsg) -> bool {
	match encode_frame(msg) {
		Some(frame) => queue_frame(out, &frame),
		None => true,
	}
}

/// Reliable send to a joined client; queue refusal marks it dead (reaped
/// into `drop_client` at the next tick).
fn client_send(client: &Client, msg: &ServerMsg) {
	if let Some(frame) = encode_frame(msg) {
		client_send_frame(client, &frame);
	}
}

/// The fan-out half of [`client_send`]: queue a payload encoded once for
/// the whole broadcast.
fn client_send_frame(client: &Client, frame: &std::sync::Arc<[u8]>) {
	if !client.dead.get() && !queue_frame(&client.out, frame) {
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
		ClientMsg, MIN_PROTOCOL, OutMsg, PROTOCOL_VERSION, RejectReason, ServerMsg, SimEvent,
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
		// One `TOD_SYNC_TICKS` period at the sun's rate — it has moved
		// toward noon (tod falls).
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

	/// Growing the radius must request a sync scan. The edited-column walk
	/// is event-driven off `needs_sync_scan`, which otherwise only rises
	/// when the center column moves — so a player who widened their draw
	/// distance standing still got no `JournalColumn` for the ring that
	/// just became visible, and (because `broadcast_edit` gates on
	/// `synced_columns`) no live edits there either.
	#[test]
	fn growing_draw_dist_syncs_the_newly_visible_ring() {
		// A pre-journaled edit in a column outside the harness Hello's
		// draw distance and inside the widened one. The client spawns in
		// the origin column and never moves, so `needs_sync_scan` can only
		// come from the SetDrawDist itself.
		let dir = temp_dir("drawgrow");
		let col = (6, 0);
		{
			let mut j = crate::journal::Journal::open(&dir, SEED).unwrap();
			j.append(
				digblock_net::messages::BlockEdit {
					x: col.0 * 16 + 8,
					y: 127,
					z: 8,
					block: digblock_core::block::BlockType::Stone as u16,
				},
				0,
			);
			j.sync();
		}
		let mut sim =
			super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		for _ in 0..5 {
			sim.tick_once(vec![]);
		}
		c.drain();
		let synced = |c: &TestClient| {
			c.msgs
				.iter()
				.filter(
					|m| matches!(m, ServerMsg::JournalColumn { cx, cz, .. } if (*cx, *cz) == col),
				)
				.count()
		};
		assert_eq!(synced(&c), 0, "column sits outside draw_dist 4");

		c.msgs.clear();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::SetDrawDist { draw_dist: 8 },
		}]);
		for _ in 0..5 {
			sim.tick_once(vec![]);
		}
		c.drain();
		assert_eq!(
			synced(&c),
			1,
			"widening the radius synced the column without moving: {:?}",
			c.msgs
		);
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
		// clock runs. OUT_CAP fills at one snapshot per SNAPSHOT_DIVISOR
		// ticks, then DGRAM_STALL_SECS of Full escalates to dead.
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
			// The rock rule: mined stone lands as Rubble.
			assert_eq!(sim.test_inventory_count(1, BlockType::Rubble), 1);

			// Park the Rubble deep in the grid (a deliberate arrangement —
			// it must survive reconnect and reboot verbatim).
			let mut grid =
				vec![digblock_core::hotbar::WIRE_EMPTY; digblock_core::inv_grid::GRID_SLOTS];
			grid[17] = BlockType::Rubble as u16;
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
						if counts == &vec![(BlockType::Rubble as u16, 1)]
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
					if counts == &vec![(BlockType::Rubble as u16, 1)]
			)),
			"reboot carries the credit: {:?}",
			c3.msgs
		);
		assert!(
			c3.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::Layout { grid, .. }
					if grid.get(17) == Some(&(BlockType::Rubble as u16))
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

	/// The strength wall, server half: a DigStart on stone below the Rock
	/// family's wall refuses with `SkillGated` and opens no hold (refused
	/// at the START — the player must not hold the button through a dig
	/// timer for nothing); at the gate level the hold opens normally.
	#[test]
	fn gated_rock_refuses_the_dig_start() {
		use digblock_core::raycast::{REACH, raycast};
		use glam::Vec3;
		let mut sim = flat_sim("rockgate");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::DigRefused {
					reason: digblock_net::messages::RejectReason::SkillGated,
					..
				}
			)),
			"level-1 stone dig refused at the start: {:?}",
			c.msgs
		);
		assert_eq!(sim.test_digging(1), None, "no hold opened");

		sim.bench_set_mining_level(
			1,
			digblock_core::unlock::mine(digblock_core::BlockType::Stone),
		);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			},
		}]);
		assert_eq!(sim.test_digging(1), Some(hit.cell), "gate cleared at 5");
	}

	/// SKILLS-DESIGN §5.3, server half — the whole ore loop in one test,
	/// because the three halves only mean anything together:
	///
	/// 1. Below the reveal, a vein is stone. The hold opens, the dig
	///    completes, and the player gets rubble and never knows: you dig
	///    what you can see, and spending an unseen seam is the cost of
	///    tunnelling through country you have not levelled into.
	/// 2. At the reveal WITHOUT the siphon, `DigStart` refuses with
	///    `NeedsTool` and opens no hold — if you can see the seam, the
	///    game will not let you spend it by accident.
	/// 3. With the siphon, the completing dig writes DepletedStone rather
	///    than air, credits the ORE (not the cell's Rubble), and pays BOTH
	///    skills (§5.4's dual award).
	#[test]
	fn a_revealed_vein_needs_the_siphon_and_pays_two_skills() {
		use digblock_core::block::BlockType;
		use digblock_core::skills::Skill;
		use digblock_net::messages::EditKind;
		use glam::Vec3;
		let (cell, eye, ore) = super::harness::a_vein_under_spawn();
		let reveal = digblock_core::unlock::reveal(ore).expect("an ore");

		// 1. Below the reveal: stone all the way down.
		let mut sim = super::harness::flat_sim_at("ore-blind", eye);
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.bench_set_mining_level(1, reveal - 1);
		let mut t = 100;
		let down = Vec3::new(0.0, -1.0, 0.0);
		let hold = BlockType::Stone.hardness_ticks().unwrap() as u32 + 8;
		let dug = dig_hold(&mut sim, 1, down, hold, 1, EditKind::Dig, &mut t);
		c.drain();
		assert_eq!(dug, cell, "dug the cell the vein is in");
		assert_eq!(sim.test_cell_at(cell.0, cell.1, cell.2), Some(0), "a hole");
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Rubble),
			1,
			"rubble, and no idea what was lost"
		);
		assert_eq!(sim.test_inventory_count(1, ore), 0);

		// 2. At the reveal, bare-handed: refused before the hold opens.
		let mut sim = super::harness::flat_sim_at("ore-refuse", eye);
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.bench_set_mining_level(1, reveal);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [cell.0, cell.1, cell.2],
			},
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::DigRefused {
					reason: digblock_net::messages::RejectReason::NeedsTool,
					..
				}
			)),
			"a revealed vein refuses a bare hand: {:?}",
			c.msgs
		);
		assert_eq!(sim.test_digging(1), None, "no hold opened");

		// 3. With the siphon: a draw, not a dig.
		let mut sim = super::harness::flat_sim_at("ore-draw", eye);
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.bench_set_mining_level(1, reveal);
		sim.test_grant(1, BlockType::EssenceSiphon, 1);
		sim.bench_set_active_tool(1, BlockType::EssenceSiphon);
		let before = (
			sim.test_skill_xp(1, Skill::Mining),
			sim.test_skill_xp(1, Skill::Alchemy),
		);
		let mut t = 100;
		// Timed as the ORE, which is slower than the stone it sits in.
		let hold = ore.hardness_ticks().unwrap() as u32 + 8;
		let dug = dig_hold(&mut sim, 1, down, hold, 1, EditKind::Dig, &mut t);
		c.drain();
		assert_eq!(dug, cell);
		assert_eq!(
			sim.test_cell_at(cell.0, cell.1, cell.2),
			Some(BlockType::DepletedStone as u16),
			"a tapped face, not a hole — mining ore never excavates"
		);
		assert_eq!(sim.test_inventory_count(1, ore), 1, "the ore itself");
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Rubble),
			0,
			"never the stone's own drop"
		);
		let (mining_xp, alch_xp) = digblock_core::skills::ore_field_xp(ore).expect("an ore");
		assert_eq!(
			sim.test_skill_xp(1, Skill::Mining) - before.0,
			mining_xp,
			"full Mining at the rock face"
		);
		assert_eq!(
			sim.test_skill_xp(1, Skill::Alchemy) - before.1,
			alch_xp,
			"and the Alchemy cut — neither skill levels alone"
		);

		// 4. And it is timed as the ore on THIS end too. Case 3 alone
		//    cannot show that: it holds for the ore's own length, which
		//    satisfies a server timing the draw by either number. An ore is
		//    several times Stone's hardness, so a server reading the cell's
		//    hardness would hand out an Aetherock for a fraction of the
		//    intended hold — a gap `dig_grace` comes nowhere near, and one
		//    a modified client can simply take.
		let mut sim = super::harness::flat_sim_at("ore-short", eye);
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.bench_set_mining_level(1, reveal);
		sim.test_grant(1, BlockType::EssenceSiphon, 1);
		sim.bench_set_active_tool(1, BlockType::EssenceSiphon);
		let mut t = 100;
		let stone_hold = BlockType::Stone.hardness_ticks().unwrap() as u32 + 8;
		assert!(
			stone_hold < ore.hardness_ticks().unwrap() as u32,
			"the vein has to out-last its stone or this proves nothing"
		);
		let dug = dig_hold(&mut sim, 1, down, stone_hold, 1, EditKind::Dig, &mut t);
		c.drain();
		assert_eq!(
			sim.test_cell_at(dug.0, dug.1, dug.2),
			Some(BlockType::Stone as u16),
			"a stone-length hold does not draw the vein"
		);
		assert_eq!(sim.test_inventory_count(1, ore), 0, "and credits nothing");
	}

	/// SKILLS-DESIGN §5.6, server half: the Resonant Pick breaks the
	/// PLANE of the face it hits, derived from the server's own raycast
	/// and its own world — the client names one cell and this end decides
	/// what the swing touched.
	///
	/// The three properties that matter, in one test because they are one
	/// mechanism: the width follows the level, every extra cell holds the
	/// same id, and the credit is one per BLOCK broken — which is what
	/// `begin_multi`'s single `inv_delta` describes.
	#[test]
	fn the_resonant_pick_breaks_the_plane_the_server_derives() {
		use digblock_core::block::BlockType;
		use digblock_net::messages::EditKind;
		use glam::Vec3;
		for (level, want) in [(54u32, 1usize), (55, 3), (85, 9)] {
			let mut sim = flat_sim(&format!("pick{level}"));
			let mut c = connect(&mut sim, 1);
			wait_live(&mut sim, 1);
			sim.bench_set_mining_level(1, level);
			sim.test_grant(1, BlockType::ResonantPick, 1);
			sim.bench_set_active_tool(1, BlockType::ResonantPick);
			let mut t = 100;
			// Straight down at the flat stone floor: the plane is
			// horizontal, so every cell of it is stone and qualifies.
			let down = Vec3::new(0.0, -1.0, 0.0);
			let hold = BlockType::Stone.hardness_ticks().unwrap() as u32 * 3 + 16;
			let dug = dig_hold(&mut sim, 1, down, hold, 1, EditKind::Dig, &mut t);
			c.drain();
			let broken = (-1..=1)
				.flat_map(|dx| (-1..=1).map(move |dz| (dx, dz)))
				.filter(|&(dx, dz)| sim.test_cell_at(dug.0 + dx, dug.1, dug.2 + dz) == Some(0))
				.count();
			assert_eq!(broken, want, "level {level} breaks {want}");
			assert_eq!(
				sim.test_inventory_count(1, BlockType::Rubble),
				want as u32,
				"level {level}: one credit per block, not per swing"
			);
			// Nothing behind the face: a pick widens a wall, it does not
			// bore through one.
			assert_ne!(
				sim.test_cell_at(dug.0, dug.1 - 1, dug.2),
				Some(0),
				"level {level} tunnelled"
			);
		}
	}

	/// A protected cell ANYWHERE in the pick's plane refuses the whole
	/// swing — proved rather than cited, because "the check already runs
	/// over the assembled `writes`" is exactly the kind of claim that
	/// stops being true the moment somebody assembles the writes
	/// differently. Nothing partially breaks: a refused action leaves the
	/// aimed cell standing too.
	#[test]
	fn a_protected_cell_refuses_the_whole_pick_swing() {
		use digblock_core::block::BlockType;
		use digblock_core::raycast::{REACH, raycast};
		use digblock_net::messages::EditKind;
		use glam::Vec3;
		let mut sim = flat_sim("pick-protect");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.bench_set_mining_level(1, 85);
		sim.test_grant(1, BlockType::ResonantPick, 1);
		sim.bench_set_active_tool(1, BlockType::ResonantPick);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.0, -1.0, 0.0);
		let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		// Lock a region whose NEAR edge is the aimed cell's +x neighbour,
		// so the aimed cell itself is free and the refusal can only come
		// from a cell the swing reached. (`lock_region` is a box CLEAR
		// out from the eye's column, floored just below the eye.)
		let lock_x = hit.cell.0 + 1 + digblock_core::spawn::CLEAR;
		sim.bench_lock_spawn_at(Vec3::new(
			lock_x as f32 + 0.5,
			hit.cell.1 as f32 + 2.5,
			hit.cell.2 as f32 + 0.5,
		));
		assert!(
			digblock_core::protect::immutable(
				hit.cell.0 + 1,
				hit.cell.1,
				hit.cell.2,
				sim.spawn_lock().as_ref()
			) && !digblock_core::protect::immutable(
				hit.cell.0,
				hit.cell.1,
				hit.cell.2,
				sim.spawn_lock().as_ref()
			),
			"the lock has to straddle the plane, not swallow it"
		);
		let mut t = 100;
		let hold = BlockType::Stone.hardness_ticks().unwrap() as u32 * 3 + 16;
		dig_hold(&mut sim, 1, down, hold, 1, EditKind::Dig, &mut t);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					reason: digblock_net::messages::RejectReason::Protected,
					..
				}
			)),
			"the swing reached a locked cell: {:?}",
			c.msgs
		);
		assert_ne!(
			sim.test_cell_at(hit.cell.0, hit.cell.1, hit.cell.2),
			Some(0),
			"nothing partially broke"
		);
	}

	/// SIM v13's craft gate, server half: a DigStart on a craft-only block
	/// below the Alchemy level that makes it refuses with `NotUnlocked` and
	/// opens no hold — you cannot mine your way to a torch you can't yet
	/// transmute, in your own world or through somebody else's build. The
	/// natural block beside it stays freely diggable at the same level.
	#[test]
	fn craft_only_blocks_refuse_the_dig_start() {
		use digblock_core::block::BlockType;
		use digblock_core::raycast::{REACH, raycast};
		use glam::Vec3;
		let mut sim = flat_sim("craftgate");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let pos = sim.test_player(1).unwrap().pos;
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let ground = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
		// Stand a torch on the flat world's surface, clear of the capsule
		// (DigStart validates the gates and the cell, never the reach). The
		// world is stone, so clear the strength wall too — this test is about
		// the other one.
		sim.bench_set_mining_level(1, digblock_core::unlock::mine(BlockType::Stone));
		let cell = (ground.cell.0 + 3, ground.cell.1 + 1, ground.cell.2);
		sim.test_set_cell(cell.0, cell.1, cell.2, BlockType::Torch as u16);
		let start = |sim: &mut super::Sim| {
			sim.tick_once(vec![SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::DigStart {
					cell: [cell.0, cell.1, cell.2],
				},
			}]);
		};

		start(&mut sim);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::DigRefused {
					reason: digblock_net::messages::RejectReason::NotUnlocked,
					..
				}
			)),
			"level-1 torch dig refused at the start: {:?}",
			c.msgs
		);
		assert_eq!(sim.test_digging(1), None, "no hold opened");

		// One level short is still short — the gate is the recipe's level.
		sim.bench_set_alchemy_level(1, 7);
		start(&mut sim);
		assert_eq!(sim.test_digging(1), None, "still gated at Alchemy 7");

		sim.bench_set_alchemy_level(1, 8);
		start(&mut sim);
		assert_eq!(sim.test_digging(1), Some(cell), "gate cleared at 8");

		// The stone underneath was never the craft gate's business.
		sim.test_set_cell(cell.0, cell.1, cell.2, BlockType::Air as u16);
		let mut c2 = connect(&mut sim, 2);
		wait_live(&mut sim, 2);
		sim.bench_set_mining_level(2, digblock_core::unlock::mine(BlockType::Stone));
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 2,
			msg: ClientMsg::DigStart {
				cell: [ground.cell.0, ground.cell.1, ground.cell.2],
			},
		}]);
		c2.drain();
		assert_eq!(
			sim.test_digging(2),
			Some(ground.cell),
			"natural stone digs at Alchemy 1: {:?}",
			c2.msgs
		);
	}

	/// XP is server-computed at the two chokepoints and lands as new
	/// TOTALS: a stone dig pays the block's `dig_xp` on top of the
	/// harness's gate grant, converting the rubble pays its `convert_xp`
	/// in Alchemy, and the totals survive reconnect and a full server
	/// reboot via the identity store (join sync always carries a `Skills`
	/// frame, empty for a fresh identity).
	#[test]
	fn xp_awards_on_dig_and_alchemy_and_persists() {
		use digblock_core::block::BlockType;
		use digblock_net::messages::{AlchemyOp, EditKind};
		use glam::Vec3;

		let dir = temp_dir("xp");
		let identity = [8u8; 16];
		// dig_hold grants whatever the target's gates ask — for stone that
		// is the Rock family's strength wall — and the dig pays its own
		// `dig_xp` on top of that level's total. Derived, so a retune of
		// the wall or of the dig curve moves both halves together.
		let granted =
			digblock_core::skills::total_xp_for(digblock_core::unlock::mine(BlockType::Stone));
		let after = granted + digblock_core::skills::dig_xp(BlockType::Stone);
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		{
			let mut sim =
				super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
			let mut c = connect_with_identity(&mut sim, 1, identity);
			c.drain();
			assert!(
				c.msgs
					.iter()
					.any(|m| matches!(m, ServerMsg::Skills { xp } if xp.is_empty())),
				"fresh identity syncs empty skills: {:?}",
				c.msgs
			);
			wait_live(&mut sim, 1);
			let mut t = 0;
			dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
			c.drain();
			assert!(
				c.msgs
					.iter()
					.any(|m| matches!(m, ServerMsg::XpGain { skill: 0, total } if *total == after)),
				"stone dig pays 15 Mining on the granted {granted}: {:?}",
				c.msgs
			);

			// Convert the rubble the dig credited: its `convert_xp` in
			// Alchemy, in wire order with the InventoryFull rebase.
			sim.tick_once(vec![SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Alchemy {
					op: AlchemyOp::Convert {
						block: BlockType::Rubble as u16,
						count: 1,
					},
				},
			}]);
			c.drain();
			assert!(
				c.msgs
					.iter()
					.any(|m| matches!(m, ServerMsg::XpGain { skill: 1, total: 8 })),
				"conversion pays 8 Alchemy: {:?}",
				c.msgs
			);

			// Reconnect in the same run: the totals ride the Skills sync.
			sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
			let mut c2 = connect_with_identity(&mut sim, 2, identity);
			c2.drain();
			assert!(
				c2.msgs.iter().any(|m| matches!(
					m,
					ServerMsg::Skills { xp } if xp == &vec![(0, after), (1, 8)]
				)),
				"reconnect carries the totals: {:?}",
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
				ServerMsg::Skills { xp } if xp == &vec![(0, after), (1, 8)]
			)),
			"reboot carries the totals: {:?}",
			c3.msgs
		);
	}

	/// §8.1's anti-cheat half: Convert ops draw from the per-player
	/// extraction bucket, charged PER BLOCK. A smuggled bulk op the
	/// bucket can't cover refuses with no debit and no XP, and a flood
	/// of single ops beyond the burst refuses the same way — sustained
	/// throughput can never beat one block per pace interval.
	#[test]
	fn extraction_pace_is_server_enforced() {
		use digblock_core::block::BlockType;
		use digblock_net::messages::{AlchemyOp, EditKind};
		use glam::Vec3;

		let mut sim = flat_sim("pace");
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		// Derived from the constants, never spelled out: the bucket's
		// capacity is no longer a flat count of reference blocks — it also
		// has to cover the single costliest extraction, or the deepest ores
		// are refused forever
		// (`the_bucket_can_always_afford_the_costliest_block`). A test
		// naming that count pins the sizing instead of the RULE, and the
		// rule is what this is about: a full bucket admits a bounded bunch
		// and not one block more.
		let pace = digblock_core::alchemy::extract_pace_ticks(BlockType::Rubble, 1);
		let admits = (digblock_core::alchemy::extract_burst_ticks(1) / pace as f32) as u32;
		let over = admits + 1;
		assert!(admits >= 2, "a burst that admits fewer than two is not one");
		assert!(
			(over * pace) as f32 > digblock_core::alchemy::extract_burst_ticks(1),
			"the over-budget op has to actually be over budget"
		);
		let down = Vec3::new(0.45, -1.0, 0.0).normalize();
		let mut t = 0;
		for seq in 1..=over {
			dig_hold(&mut sim, 1, down, 90, seq, EditKind::Dig, &mut t);
		}
		assert_eq!(sim.test_inventory_count(1, BlockType::Rubble), over);
		let convert = |count: u32| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Alchemy {
				op: AlchemyOp::Convert {
					block: BlockType::Rubble as u16,
					count,
				},
			},
		};
		// One op per tick throughout: an Alchemy message also spends an
		// EDIT token, so bunching several into one tick measures that
		// bucket instead of this one.
		//
		// A bulk op costs per block, so one block past the burst is one
		// block too many — refused outright, nothing debited.
		sim.tick_once(vec![convert(over)]);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Rubble),
			over,
			"bulk convert beyond the burst refused with no debit"
		);
		// Exactly the burst, though, lands — and empties the bucket.
		sim.tick_once(vec![convert(admits)]);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Rubble),
			1,
			"a bulk op the bucket CAN cover goes through"
		);
		// Which leaves nothing for the next one: the burst is spent and a
		// single tick's refill is not a block.
		sim.tick_once(vec![convert(1)]);
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Rubble),
			1,
			"the drip is what is left once the burst is gone"
		);
		// One pace interval refills exactly one block's worth.
		for _ in 0..pace {
			sim.tick_once(vec![]);
		}
		sim.tick_once(vec![convert(1)]);
		assert_eq!(sim.test_inventory_count(1, BlockType::Rubble), 0);
		// Refused ops never paid XP: the final total reflects exactly the
		// landed blocks, at the rubble's own `convert_xp` each.
		c.drain();
		let last_alch = c.msgs.iter().rev().find_map(|m| match m {
			ServerMsg::XpGain { skill: 1, total } => Some(*total),
			_ => None,
		});
		assert_eq!(last_alch, Some(u64::from(over) * 8), "XP only for landed");
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
		// outbound queue trips the snapshot-stall reaper.)
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
		assert!(
			!b.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::PlayerJoined { id: 1, .. }))
		);

		// The player already online hears the join...
		a.drain();
		assert!(a.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::PlayerJoined { id: 1, name } if name.as_str() == "t2"
		)));

		// ...and the leave.
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 2 }]);
		a.drain();
		assert!(
			a.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::PlayerLeft { id: 1 }))
		);
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

	/// A slash line prefixed with an invisible character is a COMMAND, not
	/// chat. `is_command` used to be decided on the raw text while the
	/// handler ran on the sanitized one, and `U+200B` splits them: it is
	/// not `White_Space` (so `trim_start` leaves it) and not inside
	/// `TEXT_RANGES` (so the sanitizer removes it). The line therefore read
	/// as ordinary chat and was BROADCAST to every player as
	/// "/ban griefer" — a non-admin publishing a command, or an admin
	/// leaking one they meant to run.
	#[test]
	fn an_invisibly_prefixed_command_is_not_broadcast_as_chat() {
		let mut sim = flat_sim("zwsp");
		let mut a = connect(&mut sim, 1);
		let mut b = connect(&mut sim, 2);
		wait_live(&mut sim, 1);
		wait_live(&mut sim, 2);
		a.drain();
		b.drain();
		a.msgs.clear();
		b.msgs.clear();

		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Chat {
				text: "\u{200B}/ban griefer".into(),
			},
		}]);
		a.drain();
		b.drain();

		// Nobody sees it as chat...
		assert!(
			!b.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::Chat { from, text } if *from == 0 && text.contains("/ban")
			)),
			"the command leaked into the broadcast: {:?}",
			b.msgs
		);
		// ...and the sender is privately told it is admin-only, which is
		// what any non-admin slash line gets.
		assert!(
			a.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::Chat { text, .. } if text.contains("admin-only")
			)),
			"treated as a command and refused: {:?}",
			a.msgs
		);
	}

	/// `--admin-name` is matched against display names, which the master
	/// constrains to `validate_name`. A value that can never be one grants
	/// admin to nobody, so it is refused rather than stored — a server
	/// must not boot logging "admin: big boss" with admin disabled.
	#[test]
	fn an_unusable_admin_name_is_refused_not_stored() {
		let mut sim = flat_sim("adminname");
		for bad in ["big boss", "ab", "abcdefghijk", "hé", "bang!"] {
			sim.set_admin_name(Some(bad.into()));
			assert_eq!(sim.admin_name, None, "{bad:?} should not grant admin");
			assert!(super::admin_name_is_usable(bad).is_err(), "{bad:?}");
		}
		for good in ["boss", "Ted_L-99", "abc"] {
			sim.set_admin_name(Some(good.into()));
			assert_eq!(sim.admin_name.as_deref(), Some(good));
			assert!(super::admin_name_is_usable(good).is_ok(), "{good:?}");
		}
		sim.set_admin_name(None);
		assert_eq!(sim.admin_name, None);
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

	/// Reconnecting resumes where the player left off. Before this, a
	/// rejoin was a free trip to spawn — the whole point of the
	/// return-to-spawn cooldown, sidestepped by a disconnect.
	#[test]
	fn reconnect_resumes_at_the_last_position() {
		let dir = temp_dir("resume");
		let identity = [11u8; 16];
		let mut sim =
			super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let _c = connect_with_identity(&mut sim, 1, identity);
		wait_live(&mut sim, 1);

		// Walk well away from spawn.
		for t in (100..).take(240) {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
		}
		let away = sim.test_player(1).unwrap().pos;
		assert!(away.distance(FLAT_SPAWN) > 5.0, "test needs to be away");

		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
		let c2 = connect_with_identity(&mut sim, 2, identity);
		let spawn = c2
			.msgs
			.iter()
			.find_map(|m| match m {
				ServerMsg::Welcome { spawn, .. } => Some(glam::Vec3::from_array(*spawn)),
				_ => None,
			})
			.expect("Welcome");
		assert!(
			spawn.distance(away) < 1.0,
			"resumed at {spawn} instead of {away}"
		);
		assert_eq!(sim.test_player(2).unwrap().pos, spawn);
	}

	/// ...and the cooldown itself survives the reconnect, so rejoining
	/// can't reset the timer either.
	#[test]
	fn reconnect_does_not_reset_the_spawn_cooldown() {
		let dir = temp_dir("resumecd");
		let identity = [12u8; 16];
		let mut sim =
			super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let _c = connect_with_identity(&mut sim, 1, identity);
		wait_live(&mut sim, 1);

		// Walk away and use the return, consuming the cooldown.
		let mut t = 100;
		for _ in 0..240 {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
			t += 1;
		}
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::ReturnToSpawn,
		}]);
		assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) < 1.0);

		// Walk away again, then reconnect and immediately re-request.
		for _ in 0..240 {
			sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
			t += 1;
		}
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
		let _c2 = connect_with_identity(&mut sim, 2, identity);
		wait_live(&mut sim, 2);
		let away = sim.test_player(2).unwrap().pos;
		assert!(away.distance(FLAT_SPAWN) > 5.0, "resumed away from spawn");
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 2,
			msg: ClientMsg::ReturnToSpawn,
		}]);
		assert!(
			sim.test_player(2).unwrap().pos.distance(away) < 1.0,
			"rejoining reset the return-to-spawn cooldown"
		);
	}
}
