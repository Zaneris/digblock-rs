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
mod causeway;
mod clients;
mod edits;
mod handshake;
mod movement;
mod spawn;
mod sync;
mod test_api;
mod worldjobs;

#[cfg(test)]
pub(crate) mod harness;

use crate::ban_store::BanStore;
use crate::journal::Journal;
use crate::player_store::{IdentityKey, PlayerLayout, PlayerRecord, PlayerStore};
use crate::spawn_store::{SpawnLock, SpawnStore};
use crate::store::{ChunkStore, ColumnSource};
use digblock_core::fluid::FluidSim;
use digblock_core::hotbar::Hotbar;
use digblock_core::inv_grid::{InvGrid, reconcile_layout};
use digblock_core::inventory::Inventory;
use digblock_core::player::{self, Player, PlayerIntent, PlayerState};
use digblock_core::worldgen::{Seed, TerrainVersion};
use digblock_net::codec;
use digblock_net::messages::{
	BlockEdit, ClientMsg, InputBatch, InputTick, RejectReason, RemoteState, SelfState, ServerMsg,
};
use digblock_net::protocol::{
	AccountId, CHAT_BURST, EDIT_BURST, MIN_PROTOCOL, NO_CLIENT_SEQ, PROTOCOL_VERSION, PlayerId,
	SERVER_ID, SNAPSHOT_DIVISOR, TICK_RATE, TOD_SYNC_TICKS, protocol_supported, sanitize_chat,
	sanitize_name, sim_supported,
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

/// The first protocol whose decoder knows `ServerMsg::SpellBar` — the
/// join sync sends it only to peers whose Hello spoke this or newer.
const SPELL_BAR_PROTOCOL: u16 = 41;
/// First protocol whose `RejectReason` decoder knows `TooDetailed`.
pub(crate) const DETAIL_BUDGET_PROTOCOL: u16 = 42;
/// Cap on the stored spell-bar bytes (the client sends 3 today; the
/// headroom is for a wider bar, the bound is for a hostile peer).
const SPELL_BAR_MAX: usize = 16;

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
		/// Peer address — the IP half of the ban key.
		addr: std::net::SocketAddr,
		/// Who the master says this is. Established by `verify_join` before
		/// this event exists, so by the time the sim sees a connection the
		/// identity is already settled and nothing the peer sends can
		/// change it. `AccountId::NONE` only in tests and the offline
		/// harness, which have no master to ask.
		account: AccountId,
		/// The board's copy of the account's totals, from the same answer.
		/// Folded into the record at the handshake unless this server
		/// keeps its skills local; empty where there is no master.
		skills: digblock_core::skills::SkillSet,
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
	/// The master's verdict at registration: whether this server's totals reach the hiscores
	/// board. Re-sent whenever a registration lands, so a re-register that moves the verdict
	/// reaches the sim; a lost registration sends nothing and the last verdict stands.
	Registered {
		reports_skills: bool,
	},
}

#[derive(Debug)]
pub enum OutMsg {
	Datagram(Vec<u8>),
	/// A length-prefixed reliable frame, `Arc`-shared so a broadcast fan-out is a refcount bump.
	Frame(std::sync::Arc<[u8]>),
	/// Ask the net layer to close the connection (refused handshake).
	Close,
}

/// Per-client input-path instrumentation. Purely diagnostic — no field here feeds a decision, so
/// sampling or ignoring it changes nothing. It exists because the intake path used to drop inputs
/// three different ways in silence, which made every claim about jitter unfalsifiable.
/// `late_lost` is the one counter that means an input was lost for good.
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

/// Per-client edit-path instrumentation. Purely diagnostic, like [`InputStats`], and deliberately
/// a separate struct — this measures what a CLICK costs, not what the input buffer does. It
/// prices the two constants sized against how far the server's simulated player trails the
/// client's, [`EDIT_POS_TOLERANCE`] and [`digblock_net::protocol::dig_grace`]: any change that
/// makes the server consume inputs later spends their unmeasured headroom.
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

/// What this client is being streamed, and where from. Journal sync is
/// a push with NO acknowledgement, so nothing here notices a column a
/// client lacks — the scan on any draw-distance change and the
/// idle-gated `pump_resync` are what stand behind it.
struct Residency {
	draw_dist: i32,

	synced_columns: FxHashSet<(i32, i32)>,

	/// Journal columns owed to this client, drained a few per tick by
	/// `pump_join_sync`. A join enqueues its whole in-range set here rather
	/// than encoding it inline; `sync_queue_set` is the membership test that
	/// keeps a re-scan from queueing the same column twice.
	sync_queue: std::collections::VecDeque<(i32, i32)>,

	sync_queue_set: FxHashSet<(i32, i32)>,

	/// The background re-sync's rotation: in-range edited columns,
	/// nearest first, handed back one at a time and refilled when spent
	/// (`pump_resync`). Nothing is OWED here — every column on it has
	/// already been sent — so it is kept apart from `sync_queue`, whose
	/// front is what releases the arrival hold.
	resync_queue: std::collections::VecDeque<(i32, i32)>,

	/// Standing kindled lights owed to this client, drained beside the
	/// columns by `pump_join_sync`. A join used to send these inline in
	/// the handshake, which put an unbounded backlog in front of the one
	/// send whose refusal drops the joiner.
	kindled_queue: std::collections::VecDeque<Kindled>,

	/// Ticks until this client's next background re-sync.
	resync_countdown: u32,

	/// Run the full edited-column sync scan for this client next tick.
	/// Set on center change and on going live; steady state does ZERO
	/// journal scans (fresh columns arrive via the first-edit broadcast).
	needs_sync_scan: bool,

	center: (i32, i32),
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
	/// This client's transport has no datagram channel (`NoDatagrams`,
	/// protocol v26), so its snapshots go out wrapped in
	/// `ServerMsg::Unreliable` on the reliable stream and its inputs
	/// arrive the same way. Set once, on a message that lands immediately
	/// after Hello — before the first snapshot is due.
	no_datagrams: Cell<bool>,
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
	/// The cast bucket, spent by every `Cast` ATTEMPT — refused ones
	/// included, since validation is the expensive half: the Sphere's
	/// derives a whole sphere and scans every player against it before
	/// anything can be refused. A bucket that only charged for success
	/// would leave that work unbounded, and the reliable stream's own
	/// budget admits far more messages a second than a spellbook can.
	cast_tokens: f32,
	/// THE bench bucket, in TICKS: refilled one per tick, capped at
	/// `bench_burst_ticks(level)` (two whole blocks' worth, or one
	/// costliest op where that is more — an op no bucket can hold is an op
	/// that is refused forever), and spent at the op's own pace —
	/// `craft_pace_ticks`, per item — the bench's one verb since the
	/// essence redesign retired extraction.
	/// The server-enforced half of both drips (see `process_alchemy`).
	///
	/// One bucket and not two: there is one bench, so extracting and
	/// transmuting draw on the same clock and a client cannot run both at
	/// once for double the throughput.
	bench_tokens: f32,
	/// See [`Residency`].
	residency: Residency,
	live: bool,
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
	///
	/// It carries the SELECTED slot too ([`Hotbar::set_selected`], fed
	/// from `InputTick.active_slot` and re-declared on the reliable
	/// stream by every edit and every `DigStart`). That used to be a
	/// field of its own beside the bar,
	/// which was fine while it only chose the held-item icon — but
	/// `Hotbar::tool_in_hand` now asks whether the hand is on the tool
	/// slot at all, and an answer split across two places is one that can
	/// drift from the client's.
	hotbar: Hotbar,
	grid: InvGrid,
	/// `Some` = the master-vouched account this session belongs to, which is
	/// the key its records persist under; `None` = unverified, session-only
	/// inventory (offline harness and tests only).
	identity: Option<IdentityKey>,
	/// The same account, unwrapped, for the authorization questions that
	/// have no "anonymous" answer — admin and bans. `AccountId::NONE` when
	/// there is none, which both of those refuse by construction.
	account: AccountId,
	/// In-progress hold-to-dig, if any (see `edits::DigState`).
	digging: Option<edits::DigState>,
	/// Tick of the last granted teleport of ANY kind — Return Home and
	/// every waystone port share one clock, so hops cannot be chained.
	/// `None` = never used (the first request after joining is allowed).
	last_teleport_tick: Option<u32>,
	/// Per-skill XP totals — the authoritative copy. Awards happen at
	/// the chokepoints (dig credit, alchemy, the cast) and reach the
	/// client as `ServerMsg::XpGain` totals; levels derive from the
	/// shared curve.
	skills: digblock_core::skills::SkillSet,
	/// The tick this client's wisp burns until, for the snapshot fx byte
	/// alone — the light itself is per-viewer GI, computed client-side.
	/// Session-local, exactly like the client's own copy.
	wisp_until: Option<u32>,
	/// The Hello protocol this session spoke — gates join-sync messages
	/// an older decoder cannot read (`ServerMsg::SpellBar`).
	protocol: u16,
	/// Spell-bar bytes, stored VERBATIM (opaque preference data — the
	/// client validates on read), bounded by [`SPELL_BAR_MAX`].
	spell_bar: Vec<u8>,
	/// The pick's declared dig shape. Session state like the selected
	/// slot; the DEFAULT (`Plane`, clamped by the unlock) reproduces the
	/// pre-toggle behavior for every peer that never sends one.
	dig_orientation: digblock_core::pick::DigOrientation,
	/// The tick this client's causeway runs out, and the planks standing
	/// under it. Session-local like the wisp — the PLANKS are journaled
	/// cells everyone reads, but only the holder of the trail can retire
	/// them, so a departure takes the road up.
	causeway_until: Option<u32>,
	causeway: digblock_core::causeway::Trail,
	causeway_tokens: f32,
}

impl Client {
	/// The held item for snapshots, in hotbar wire encoding. The tool reports its charge too — a
	/// bucket is a different thing full than empty. WHICH tool is the second axis: the slot only
	/// ever says `WaterTool`, so reporting the slot drew every remote's Resonant Pick as a bucket.
	/// Tools are real block ids, so this sends the tool's own id and keeps the sentinels for the
	/// bucket, exactly as `Hotbar::to_wire` does on the Layout wire.
	fn held_wire(&self) -> u16 {
		use digblock_core::block::BlockType;
		use digblock_core::hotbar::{HotbarItem, WIRE_EMPTY, WIRE_TOOL};
		match self.hotbar.selected_item() {
			None => WIRE_EMPTY,
			// A bare tool slot is an empty hand — the fresh-player state,
			// now that the first vessel is crafted.
			Some(HotbarItem::WaterTool) => match self.hotbar.active_tool() {
				None => WIRE_EMPTY,
				Some(BlockType::Bucket) if self.inventory.bucket_full() => {
					digblock_net::messages::HELD_TOOL_FULL
				}
				Some(BlockType::Bucket) => WIRE_TOOL,
				Some(t) => t as u16,
			},
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
			last_teleport_tick: self.last_teleport_tick,
			skills: self.skills.clone(),
			spells: self.spell_bar.clone(),
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
	/// tick they arrived (reaped after HELLO_TIMEOUT_TICKS) and the account
	/// the master vouched for at admission.
	lobby: FxHashMap<
		u64,
		(
			OutTx,
			u32,
			std::net::SocketAddr,
			AccountId,
			digblock_core::skills::SkillSet,
		),
	>,
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
	/// Persistent bans (account + IP), enforced at Hello and driven by
	/// `/ban`.
	ban_store: BanStore,
	/// Resolved waystone sites (kind index → eye), stored once and never
	/// re-derived — a lock's region cannot change.
	waystone_store: crate::waystone_store::WaystoneStore,
	/// The accounts granted admin chat commands.
	///
	/// Accounts, not names. Admin used to be a case-insensitive compare
	/// against `client.name`, which the master would hand to whoever asked
	/// for it — so `--admin-name admin` plus a join under the name "admin"
	/// was a complete server takeover, executable by anyone who read the
	/// roster. Names are display strings; this is an
	/// authorization decision, and it keys on the one identifier the master
	/// vouches for.
	admin_accounts: FxHashSet<AccountId>,
	/// Master-era saves a client can reclaim with its old session (`crate::master_claims`).
	master_claims: FxHashMap<[u8; 16], AccountId>,
	/// Fold the master's copy of a joiner's skills into the record at
	/// join — the default, so a character is one character everywhere.
	/// Off, this server's ladder stands alone (`--use-master-levels false`).
	master_levels: bool,
	/// Whether the master trusts this server's totals onto the board. False until a registration
	/// says otherwise — and nobody can join before one does. Ranked, the testing levers
	/// (`admin::Scope::Unranked`) are withheld: a number they minted would stand on the ladder.
	ranked: bool,
	/// Seconds added to the tick clock by `/time` — same lifetime and
	/// persistence policy as the tick counter it offsets (none).
	tod_offset_secs: f64,
	/// Clock-scheduled sweeps near active players. Empty until something
	/// registers a rule; the tick costs one `is_none` while it is.
	world_jobs: digblock_core::worldjob::Scheduler,
	/// Seconds this world had already lived when the process opened it —
	/// the sweep clock's epoch, and nothing else's. See
	/// [`worldjobs::world_epoch_secs`].
	world_epoch_secs: f64,
	/// Peak live head-count over the trailing game day — what the
	/// regrowth sweep scales its odds by.
	presence: digblock_core::presence::RollingPeak,
	/// Every kindled light still burning, on the sim's own clock — not
	/// the caster's session, so a caster who ports away or leaves the
	/// server changes nothing about when it goes out. Not journaled: a
	/// light is never a cell, and a restart may forget it.
	kindled: Vec<Kindled>,
	/// Every sphere still climbing — at most one per caster. Bill paid and
	/// plan fixed as the channel began; the climb lasts the channel and
	/// UNWINDS if the caster breaks it or leaves.
	closings: Vec<SphereClosing>,
	/// Causeway planks whose removal could not land — an unloaded column
	/// or an unhealthy journal. Held by the SIM because the trail that
	/// knew about them is already gone with its caster, and this is the
	/// one write here that must eventually LAND rather than be skipped
	/// the way fluid's and the sweeps' are: skipped, the glass stands for
	/// good. Retried each tick (`causeway::OWED_CAP` bounds the leak).
	owed_planks: std::collections::VecDeque<digblock_core::causeway::Cell>,
}

/// One sphere climbing about a channelling caster: the cells still to land, who is paying, and
/// the bill. The head is fixed as the channel begins; a caster who walks off leaves it climbing.
/// `plan_len` and `span` are here because the wire carries the climb's PROGRESS, not its cells:
/// every client re-derives the plan from `head`, `radius` and `block`. The blocks are paid whole as
/// the channel begins and settled at its end: `debited` less `placed` is what the climb declined.
struct SphereClosing {
	conn_id: u64,
	by: PlayerId,
	head: (i32, i32, i32),
	radius: i32,
	block: digblock_core::block::BlockType,
	essence: digblock_core::block::Essence,
	fare: u32,
	/// The wisp the seal lights, priced as the channel began.
	wisp: (digblock_core::block::Essence, u32),
	debited: u32,
	placed: u32,
	plan_len: usize,
	span: u32,
	closing: digblock_core::sphere::Closing,
}

impl SphereClosing {
	/// How much of the plan has been handed to the world: a PREFIX
	/// length in `sphere::plan`'s own order, which is the one currency both
	/// ends share. Not the number of cells written — the two differ
	/// wherever a landing was declined, and a client cannot derive that.
	fn landed(&self) -> u32 {
		let n = digblock_core::sphere::landed_by(self.plan_len, self.closing.step(), self.span);
		u32::try_from(n).unwrap_or(u32::MAX)
	}

	/// The cell the shell is built of.
	fn wall(&self) -> u16 {
		digblock_core::chunk::make_cell(self.block, 0)
	}

	/// Both fares, as one bill.
	fn fares(&self) -> [(digblock_core::block::Essence, u32); 2] {
		[(self.essence, self.fare), self.wisp]
	}
}

/// Every pane of conjured glass in the record is an orphan at boot: the Sphere used to be built of
/// it and is not any more, and a road's planks come up before their caster is gone — so nothing
/// legitimately holds one. Each is journaled back to air as an ordinary server edit, which is what
/// a joiner, a resync and an evicted column read; the store holds nothing yet, so nothing else
/// needs correcting. Runs every boot; one pass over the index once the record is clean.
fn sweep_conjured_glass(journal: &mut Journal) {
	use digblock_core::block::BlockType;
	use digblock_core::chunk::cell_id;
	let panes: Vec<(i32, i32, i32)> = journal
		.index
		.iter_edits()
		.filter(|&(_, v)| BlockType::from_id(cell_id(v)).is_conjured())
		.map(|(c, _)| c)
		.collect();
	for &(x, y, z) in &panes {
		journal.append(
			BlockEdit {
				x,
				y,
				z,
				block: BlockType::Air as u16,
			},
			SERVER_ID,
		);
	}
	if !panes.is_empty() {
		log::info!(
			"swept {} panes of conjured glass out of the record",
			panes.len()
		);
	}
}

/// One standing kindled light: where, and the tick it burns until.
#[derive(Clone, Copy)]
struct Kindled {
	cell: [i32; 3],
	until: u32,
}

impl Sim {
	/// `seed` is the operator's number; the GENERATOR comes from the world already on disk, so
	/// restarting a server onto a newer build regenerates the same terrain rather than a new
	/// world under the same edits. A world this build cannot generate fails the boot — the
	/// journal names a generator that is not here, and any other would replace what it anchors.
	pub fn new(
		seed: f64,
		data_dir: &Path,
		sim_radius: i32,
		workers: usize,
	) -> std::io::Result<Self> {
		let recorded = crate::journal::world_terrain(data_dir, seed)?;
		let seed = Seed::new(seed, recorded.unwrap_or(TerrainVersion::CURRENT));
		match recorded {
			None => log::info!("new world: seed {seed:?}"),
			// Continuing is the default; trading the world for the current
			// generator is the operator's --archive-world.
			Some(v) if v != TerrainVersion::CURRENT => log::info!(
				"world stays on the generator that made it ({v:?}); a new world under \
				 {:?} is --archive-world",
				TerrainVersion::CURRENT
			),
			Some(_) => {}
		}
		let store = ChunkStore::new(seed, workers);
		let mut sim = Self::with_store(seed, data_dir, sim_radius, store, player::spawn(seed))?;
		sim.spawn_search = true;
		sim.ensure_spawn_lock();
		sim.ensure_arrival_sites();
		Ok(sim)
	}

	/// Test seam: inject a synthetic column source (and a spawn that fits
	/// it) so sim tests never pay real worldgen.
	#[doc(hidden)]
	pub fn new_with_source(
		seed: Seed,
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
		seed: Seed,
		data_dir: &Path,
		sim_radius: i32,
		store: ChunkStore,
		spawn: Vec3,
	) -> std::io::Result<Self> {
		let mut journal = Journal::open(data_dir, seed)?;
		sweep_conjured_glass(&mut journal);
		let player_store = PlayerStore::open(data_dir)?;
		let ban_store = BanStore::open(data_dir)?;
		let spawn_store = SpawnStore::open(data_dir)?;
		let waystone_store = crate::waystone_store::WaystoneStore::open(data_dir)?;
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
			waystone_store,
			admin_accounts: FxHashSet::default(),
			master_claims: FxHashMap::default(),
			master_levels: true,
			ranked: false,
			tod_offset_secs: 0.0,
			world_jobs: digblock_core::worldjob::Scheduler::new(),
			// After `Journal::open`, which is what creates the directory
			// on a world's first boot.
			world_epoch_secs: worldjobs::world_epoch_secs(data_dir),
			presence: digblock_core::presence::RollingPeak::day(),
			kindled: Vec::new(),
			closings: Vec::new(),
			owed_planks: std::collections::VecDeque::new(),
		})
	}

	/// Grant these accounts the admin chat commands. Replaces the whole set.
	///
	/// `AccountId::NONE` is dropped: an unverified connection carries it, so
	/// admitting it to this set would hand admin to every offline and test
	/// connection at once.
	pub fn set_admin_accounts(&mut self, accounts: impl IntoIterator<Item = AccountId>) {
		self.admin_accounts = accounts.into_iter().filter(|a| !a.is_none()).collect();
	}

	/// The master-era saves a joiner may take back by presenting its old session.
	pub fn set_master_claims(&mut self, claims: FxHashMap<[u8; 16], AccountId>) {
		self.master_claims = claims;
	}

	/// Whether a join folds in the board's copy of the account's skills.
	pub fn set_master_levels(&mut self, on: bool) {
		self.master_levels = on;
	}

	fn is_admin(&self, account: AccountId) -> bool {
		!account.is_none() && self.admin_accounts.contains(&account)
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

	/// Announce every change of skills to the board's outbox, starting
	/// with what the store already holds. Wired by the launcher, not
	/// `Sim::new`, so the test harness's worlds report to nobody.
	pub fn attach_skill_outbox(&mut self, outbox: std::sync::Arc<crate::hiscores::SkillOutbox>) {
		self.player_store.attach_outbox(outbox);
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
		self.step_closings();
		self.step_fluid();
		self.step_world_jobs();
		// Expiry is a comparison, so every client's own countdown agrees
		// with the list a joiner is handed.
		let tick = self.tick;
		self.kindled.retain(|k| k.until > tick);
		self.pump_causeways();
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

	fn handle_event(&mut self, ev: SimEvent) {
		match ev {
			SimEvent::Connected {
				conn_id,
				out,
				addr,
				account,
				skills,
			} => {
				self.lobby
					.insert(conn_id, (out, self.tick, addr, account, skills));
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
				ClientMsg::ClaimLegacy { legacy } => self.claim_legacy(conn_id, legacy),
				// A transport with no datagram channel: answer in kind from
				// here on. Arrives immediately after Hello, on the same
				// ordered stream, so it is in hand before the first
				// snapshot this client is owed.
				ClientMsg::NoDatagrams => {
					if let Some(client) = self.clients.get(&conn_id) {
						client.no_datagrams.set(true);
						log::info!(
							"player {}: no datagram channel — snapshots ride the reliable stream",
							client.id
						);
					}
				}
				// The datagram that could not travel as one. Decoded by the
				// SAME call the net layer makes on a real datagram and
				// handed to the same sim entry point, so the fallback path
				// cannot come to mean anything different.
				ClientMsg::Unreliable(bytes) => {
					match digblock_net::codec::decode_datagram(&bytes) {
						Ok(digblock_net::messages::Datagram::Input(batch)) => {
							self.handle_inputs(conn_id, batch);
						}
						// Server-bound only: a client sending itself a
						// snapshot is nonsense, and silently so.
						Ok(digblock_net::messages::Datagram::Snapshot(_)) => {}
						Err(e) => log::debug!("conn {conn_id}: bad wrapped datagram: {e}"),
					}
				}
				// Queued, NOT applied here: a DigStart must not overtake a
				// same-tick completing EditRequest already in the queue
				// (see `ClientAction`). Beyond the queue cap it drops
				// silently.
				ClientMsg::DigStart { cell, slot } => {
					if self.action_queue.len() < self.clients.len() * EDIT_BURST as usize + 64 {
						self.action_queue.push(ClientAction::DigStart {
							conn_id,
							cell: (cell[0], cell[1], cell[2]),
							slot,
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
					// Sanitize BEFORE deciding what the line IS. `is_command` used to
					// read the raw text while everything downstream ran on the sanitized
					// one, and they disagree for any character the sanitizer strips that
					// `trim_start` does not count as whitespace — `U+200B` is exactly
					// that, so a zero-width space made a command BROADCAST as chat.
					let Some(text) = sanitize_chat(&text) else {
						return;
					};
					// `sanitize_chat` already trimmed.
					let is_command = text.starts_with('/');
					let Some(account) = self.clients.get(&conn_id).map(|c| c.account) else {
						return; // lobby connections don't get to talk
					};
					let is_admin = self.is_admin(account);
					let Some(client) = self.clients.get_mut(&conn_id) else {
						return;
					};
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
							client_send(
								c,
								&admin::server_reply(
									digblock_net::messages::AdminReply::CommandsAreAdminOnly,
								),
							);
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
				ClientMsg::Cast(kind) => self.handle_cast(conn_id, kind),
				ClientMsg::CastBreak => self.break_sphere(conn_id),
				ClientMsg::CausewayEnd => self.lift_causeway(conn_id),
				// Inline like chat: sync radius is read fresh each tick, so the
				// change applies from the next sync pass.
				//
				// Either direction must REQUEST a scan. The full edited-column
				// walk is event-driven off `needs_sync_scan`, which otherwise
				// only rises when the center column moves — so a player who
				// widened their draw distance standing still got no
				// `JournalColumn` for the newly visible ring, and no live edits
				// there either. SHRINKING must scan too: a deferred prune leaves
				// `synced_columns` claiming columns the client has just evicted,
				// and widening again finds them already "synced" and sends
				// nothing.
				ClientMsg::SetDrawDist { draw_dist } => {
					if let Some(client) = self.clients.get_mut(&conn_id) {
						let want = draw_dist.clamp(DRAW_DIST_MIN, DRAW_DIST_MAX);
						if want != client.residency.draw_dist {
							client.residency.needs_sync_scan = true;
						}
						client.residency.draw_dist = want;
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
				// Inline, not queued: preference bytes with no ordering
				// relation to the edit stream. Stored verbatim (bounded);
				// persisted by the next record write-through like the layout.
				ClientMsg::SpellBar { mut slots } => {
					if let Some(client) = self.clients.get_mut(&conn_id) {
						slots.truncate(SPELL_BAR_MAX);
						client.spell_bar = slots;
					}
				}
				// Queued: a toggle-then-swing in one tick must land
				// shape-first (wire order, the Layout rule). Over-budget
				// requests drop silently.
				ClientMsg::SetDigOrientation { mode } => {
					if self.action_queue.len() < self.clients.len() * EDIT_BURST as usize + 64 {
						self.action_queue
							.push(ClientAction::SetDigOrientation { conn_id, mode });
					}
				}
			},
			SimEvent::Inputs { conn_id, batch } => self.handle_inputs(conn_id, batch),
			SimEvent::Disconnected { conn_id } => self.drop_client(conn_id),
			SimEvent::Registered { reports_skills } => self.set_ranked(reports_skills),
		}
	}

	/// Has this cell ever been written? The ore reveal's anti-dupe gate,
	/// answered from the journal — the complete and
	/// correct source, fluid included. `JournalIndex::record` only ever
	/// inserts, so the answer is monotone: a spent vein cannot be
	/// un-spent by putting stone back.
	pub(super) fn cell_edited(&self, x: i32, y: i32, z: i32) -> bool {
		self.journal
			.index
			.column(x.div_euclid(16), z.div_euclid(16))
			.is_some_and(|c| c.contains_key(&(x, y, z)))
	}

	/// The block this client SEES in the cell (`nodes::seen_block`): what
	/// every predictable gate is asked of, so a surface node is never
	/// refused as the stone it stands in.
	pub(super) fn seen_block(
		&self,
		conn_id: u64,
		cell: (i32, i32, i32),
	) -> Option<digblock_core::block::BlockType> {
		let client = self.clients.get(&conn_id)?;
		let v = self.store.cell_at(cell.0, cell.1, cell.2)?;
		Some(digblock_core::worldgen::nodes::seen_block(
			v,
			cell,
			self.store.seed(),
			digblock_core::worldgen::nodes::Levels::of(&client.skills, &client.inventory),
			self.cell_edited(cell.0, cell.1, cell.2),
		))
	}

	/// What a dig on this cell is FOR this client — the same question the
	/// client predicted with, asked of the server's own world.
	pub(super) fn ore_dig_mode(
		&self,
		conn_id: u64,
		cell: (i32, i32, i32),
	) -> digblock_core::worldgen::nodes::DigMode {
		use digblock_core::worldgen::nodes::DigMode;
		let Some(client) = self.clients.get(&conn_id) else {
			return DigMode::Break;
		};
		let Some(v) = self.store.cell_at(cell.0, cell.1, cell.2) else {
			return DigMode::Break;
		};
		digblock_core::worldgen::nodes::dig_mode(
			v,
			cell,
			self.store.seed(),
			digblock_core::worldgen::nodes::Levels::of(&client.skills, &client.inventory),
			self.cell_edited(cell.0, cell.1, cell.2),
			client.hotbar.tool_in_hand(),
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
		// `tool_in_hand`, not `active_tool`: the plane belongs to the pick
		// being SWUNG, and the client's own `pick_plane` asks the same
		// question of the same bar. The edit's declared slot has already
		// been mirrored onto the bar by the time this runs.
		if client.hotbar.tool_in_hand() != digblock_core::block::BlockType::ResonantPick {
			return Vec::new();
		}
		let level = client.skills.level(digblock_core::skills::Skill::Mining);
		let alchemy = client.skills.level(digblock_core::skills::Skill::Alchemy);
		let width = digblock_core::unlock::pick_cells(level);
		// The client mirrors this clamp over the same width, which is what
		// keeps the two footprints bit-identical.
		let orientation = client.dig_orientation.clamped(width);
		let id = digblock_core::chunk::cell_id(hit.cell_value);
		let credit = hit.block.dig_credit(
			self.cell_edited(hit.cell.0, hit.cell.1, hit.cell.2),
			alchemy,
		);
		// A LOCKED cell is deliberately not filtered here: the assembled writes are scanned for
		// protection and the whole swing refused, which is what
		// `a_protected_cell_refuses_the_whole_pick_swing` pins. The world floor is handled inside
		// the rule instead — not a policy but a cell no dig can touch, and letting the plane reach
		// it made the pick refuse every swing at the bottom of a mine.
		let mut out =
			digblock_core::pick::cells(hit.cell, hit.normal, dir, width, orientation, |c| {
				self.store.cell_at(c.0, c.1, c.2).is_some_and(|v| {
					digblock_core::pick::breaks_as_extra(
						id,
						credit,
						v,
						c,
						self.ore_dig_mode(conn_id, c),
						self.cell_edited(c.0, c.1, c.2),
						alchemy,
					)
				})
			});
		out.retain(|&c| c != hit.cell);
		out
	}
}

fn column_of(pos: Vec3) -> (i32, i32) {
	(
		(pos.x.floor() as i32).div_euclid(16),
		(pos.z.floor() as i32).div_euclid(16),
	)
}

/// Squared column distance — the ordering key `in_radius` tests against, shared so "near" means
/// the same thing to the sort and to the gate. i64 throughout: `col` and `center` are separated
/// by at most `TP_LIMIT`, and in i32 both the subtraction at the extremes and the product well
/// inside that range overflow — release wraps, and a wrapped negative sum passes the test, which
/// syncs a client columns it is nowhere near and force-drops it when the queue refuses.
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
mod tests;
