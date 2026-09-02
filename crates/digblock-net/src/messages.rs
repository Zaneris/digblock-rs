//! Wire message types. Plain arrays instead of glam types so the wire
//! format has no dependency on math-library serde impls; conversion
//! happens at the edges. Append-only — see protocol.rs.

use crate::protocol::PlayerId;
use serde::{Deserialize, Serialize};

// ---- admin command replies -------------------------------------------

/// Why a display name did not resolve to one live player.
///
/// Carried instead of a sentence so the words stay in `voice` with every
/// other line a player reads. Append-only, like everything else here.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoPlayer {
	NotAName,
	StillJoining,
	NotOnline,
	/// More than one live player answers to the name.
	Ambiguous {
		count: u32,
	},
}

/// Which command a usage line is for. The COMMAND is the server's fact; the line describing it is
/// the client's. Naming the command rather than sending its prose is what lets `/give` advertise
/// the essences from `Essence::ALL` at render time, so advertisement and parser cannot drift.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminUsage {
	Tp,
	SetSpawn,
	SetSpawnDir,
	LockSpawn,
	Goto,
	Bring,
	Give,
	Time,
	Reset,
	Ban,
	BanName,
	Unban,
}

/// An admin command's answer, as typed data rather than a sentence.
///
/// Admin replies used to cross the wire as English inside `ServerMsg::Chat` from `SERVER_ID`,
/// which put player-visible text in the server — the one place the third writing rule says it may
/// not live — and left every test asserting words rather than what a handler did. Appended in v29.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum AdminReply {
	Usage(AdminUsage),
	/// One `/help` catalog row; the usage stem is the dispatch key and an identifier, not prose.
	CommandHint {
		usage: String,
		desc: String,
	},
	UnknownCommand {
		typed: String,
	},
	CompassHint,
	NoSuchPlayer {
		name: String,
		why: NoPlayer,
	},
	ThatIsYou,
	Teleported {
		x: i64,
		z: i64,
	},
	TeleportedTo {
		name: String,
	},
	Brought {
		name: String,
	},
	TeleportedYouBy {
		admin: String,
	},
	NotLookingAtBlock,
	NotLookingAtTopFace,
	/// The aimed spot cannot hold a spawn; the clearance it would need rides along.
	SpawnNotClear {
		clear: u32,
		capsule: u32,
	},
	SpawnPointSet {
		cell: [i32; 3],
	},
	NotFacingAnywhere,
	SpawnFacingSet {
		compass: String,
	},
	/// `cell` is the block stood on; `region` is x0,x1,y0,y1,z0,z1.
	SpawnLocked {
		cell: [i32; 3],
		region: [i32; 6],
		already: bool,
	},
	NoClearSpawnToLock,
	/// Pinned by hand but not protected: the point stays put, its cells can still be dug.
	SpawnSetButUnlocked {
		cell: [i32; 3],
	},
	SpawnAlreadyUnlocked,
	SpawnUnlocked,
	NotAnEssence {
		typed: String,
	},
	/// `essence` is an `Essence` index; `held` is the total after the gift.
	Gave {
		name: String,
		essence: u16,
		gained: u32,
		asked: u32,
		held: u32,
	},
	WereGiven {
		essence: u16,
		gained: u32,
	},
	TimeIs {
		hour: u32,
		minute: u32,
	},
	TimeSet {
		hour: u32,
		minute: u32,
	},
	ResetPlayer {
		name: String,
	},
	YouWereReset,
	NotBanningYourself,
	SharesYourIp {
		name: String,
	},
	Banned {
		name: String,
		ip: String,
	},
	NoBans,
	BanEntry {
		name: String,
		ip: String,
	},
	Unbanned {
		arg: String,
		removed: u32,
	},
	NoBanMatches {
		arg: String,
	},
	/// A pre-account character was folded into the account that just joined (AUTH-DESIGN §6).
	LegacyCharacterClaimed,
	/// A slash line from a player who is not an admin.
	CommandsAreAdminOnly,
	/// A testing lever typed on a server whose totals reach the board; `command` is its stem.
	WithheldOnRankedServer {
		command: String,
	},
}

// ---- unreliable datagrams --------------------------------------------

/// One QUIC datagram = one `Datagram`, raw postcard, no length prefix.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Datagram {
	/// Client → server.
	Input(InputBatch),
	/// Server → client.
	Snapshot(Snapshot),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct InputTick {
	/// The client's predicted server tick this input applies to.
	pub tick: u32,
	pub move_axis: [f32; 2],
	pub jump: bool,
	/// Look direction (lives in the camera, not `Player` — must ride).
	pub look: [f32; 3],
	/// Predicted post-tick state: the adoption-envelope claim.
	pub claimed_pos: [f32; 3],
	pub claimed_vel: [f32; 3],
	/// The hotbar slot selected when this input was sampled — feeds the
	/// held-item broadcast in snapshots (enforcement reads the slot the
	/// edit itself declares, not this lagged copy). Appended in v10.
	pub active_slot: u8,
}

/// Newest-last, at most `INPUT_REDUNDANCY` entries.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct InputBatch {
	pub inputs: Vec<InputTick>,
}

/// YOUR own authoritative state. Full `f32` precision, deliberately: `predict.rs` compares the
/// echo against its claim within `POS_EPS2`, so a claim the server adopted has to round-trip
/// bit-comparably or EVERY snapshot takes the rewind-and-replay path. No `look`: the server
/// echoes back the direction we sent it and nothing on the client reads it. No `id` either — it
/// is ours by construction.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct SelfState {
	pub pos: [f32; 3],
	pub vel: [f32; 3],
}

/// Somebody else, as the client actually consumes them — a fraction of
/// the wire the old shared struct (v13) spent, which sent a whole `look`
/// vector where only a yaw is ever read and a full-precision `vel` to
/// drive an `EXTRAP_CAP_TICKS`-capped extrapolation.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct RemoteState {
	pub id: PlayerId,
	/// Still `f32`: a remote's position is drawn directly, and quantizing
	/// it would show up as stepping under interpolation.
	pub pos: [f32; 3],
	/// Velocity in thousandths of a block/s, clamped to `MAX_SPEED` —
	/// which in those units stays comfortably inside `i16`. Feeds only the
	/// capped extrapolation in `remote.rs`, so a millimetre/s of resolution
	/// is already far past what one frame can show.
	pub vel: [i16; 3],
	/// Facing as a whole-turn fraction: `0..=u16::MAX` maps onto `0..2π`,
	/// finer than a frame of motion can show. PITCH IS NOT SENT —
	/// `remote.rs` has only ever computed `look.x.atan2(look.z)`, and
	/// remote players are drawn upright.
	pub yaw: u16,
	/// What the player is holding, in hotbar wire encoding
	/// (`WIRE_EMPTY`/`WIRE_TOOL`/[`HELD_TOOL_FULL`]/block id) — remote
	/// clients render it in hand. Appended in v10.
	pub held: u16,
	/// The player's visible effect, packed by [`pack_fx`]: the wisp they burn today, with a kind
	/// reserved for the siphon draw's remote half. Appending a struct field is the one genuinely
	/// breaking postcard change, which is what the v32 `MIN_PROTOCOL` raise carries. Appended in
	/// v32.
	pub fx: u8,
	/// Which skills this player has mastered, one bit each in `Skill` order
	/// (`Skill::mastery_bits`), for the glyph beside their name tag — their skills are not in
	/// the snapshot, and the nameplate is the one place another player reads them. Appended in
	/// v38, the same breaking shape as `fx`, so `MIN_PROTOCOL` rose with it.
	pub mastery: u8,
}

/// The remote-effect kinds [`RemoteState::fx`] can carry. Two bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoteFx {
	None,
	/// A burning wisp: its cast light level and whitened flag; caster skills are not in snapshots.
	Wisp {
		level: u8,
		whitened: bool,
	},
}

/// Kind in the top two bits, the whitened flag under it, the level in
/// the low five — one byte, decoded by [`unpack_fx`], its round-trip
/// pinned by `fx_round_trips`.
pub fn pack_fx(fx: RemoteFx) -> u8 {
	match fx {
		RemoteFx::None => 0,
		RemoteFx::Wisp { level, whitened } => (1 << 6) | (u8::from(whitened) << 5) | (level & 0x1F),
	}
}

pub fn unpack_fx(b: u8) -> RemoteFx {
	match b >> 6 {
		1 => RemoteFx::Wisp {
			level: b & 0x1F,
			whitened: b & 0x20 != 0,
		},
		_ => RemoteFx::None,
	}
}

/// `held` sentinel for a CHARGED water tool. It lives here rather than in `digblock_core::hotbar`
/// because the arrangement encoding has no business knowing about it: a slot holds the tool,
/// never the tool's charge, and `Hotbar::from_wire` treats this value as the unknown id it is.
pub const HELD_TOOL_FULL: u16 = u16::MAX - 2;

/// Encode a look direction as the wire yaw. Vertical looks (a player
/// staring straight down) have no yaw to speak of; `atan2(0, 0)` is 0,
/// which is as good an answer as any and never NaN.
pub fn yaw_to_wire(look: [f32; 3]) -> u16 {
	let turns = look[0].atan2(look[2]) / std::f32::consts::TAU;
	// atan2 gives (-π, π] → (-0.5, 0.5]; `rem_euclid` folds it to [0, 1).
	(turns.rem_euclid(1.0) * 65536.0) as u16
}

/// Wire yaw back to radians, in `[0, 2π)`. Callers rotate by it, and
/// rotation is 2π-periodic, so the shifted range versus `atan2`'s
/// `(-π, π]` is not a difference anyone can see.
pub fn yaw_from_wire(yaw: u16) -> f32 {
	yaw as f32 / 65536.0 * std::f32::consts::TAU
}

/// Interpolate between two wire yaws along the SHORTEST arc. Done in `u16` space on purpose:
/// `wrapping_sub` on the difference is exactly the signed shortest arc, with the wrap at north
/// handled for free. Lerping the decoded radians instead would sweep a player the long way round
/// whenever they crossed the discontinuity.
pub fn yaw_lerp(a: u16, b: u16, t: f32) -> u16 {
	let arc = b.wrapping_sub(a) as i16;
	a.wrapping_add((arc as f32 * t).round() as i16 as u16)
}

/// Velocity component → wire, clamped to the protocol's speed ceiling.
pub fn vel_to_wire(v: f32) -> i16 {
	let m = crate::protocol::MAX_SPEED;
	// NaN clamps to 0 via the fallthrough: `f32::clamp` panics on a NaN
	// bound, not a NaN value, but `as i16` on NaN is 0 anyway.
	(v.clamp(-m, m) * 1000.0).round() as i16
}

pub fn vel_from_wire(v: i16) -> f32 {
	v as f32 / 1000.0
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Snapshot {
	pub server_tick: u32,
	/// Highest input tick of YOURS the server has consumed —
	/// the reconciliation anchor (personalized per client).
	pub your_last_input_tick: u32,
	/// Your own state. `None` only for a client the sim does not consider
	/// live yet, which is also the case where there is nothing to ack.
	pub me: Option<SelfState>,
	pub others: Vec<RemoteState>,
}

// ---- reliable control stream -----------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum EditKind {
	Dig,
	/// `block` is a full u16 CELL (id + reserved state bits); nonzero state bits are rejected.
	Place {
		block: u16,
	},
	/// Water-tool collect: the fluid-aware ray, the same hold gate as `Dig`, `BucketFull` if full.
	Collect,
	/// Use/interact click (door toggle): the interact-aware ray; `normal` may be zero. In v8.
	Interact,
	/// RETIRED (the Mason's Arc was removed): no client sends it and a server refuses it;
	/// kept decodable, the wire being append-only.
	PlaceArc {
		block: u16,
	},
	/// One Causeway plank: the client NAMES `cell`; the server bounds it via `causeway::claim_ok`.
	CausewayStep,
}

/// One alchemy request (wire form; ids are validated server-side against
/// the core tables). Appended in v8.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum AlchemyOp {
	/// RETIRED (v28): refused, nothing debited; kept decodable, the wire being append-only.
	Convert { block: u16, count: u32 },
	/// Craft `count` batches of the shaped item `item` from essence and feedstock.
	Craft { item: u16, count: u32 },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct BlockEdit {
	pub x: i32,
	pub y: i32,
	pub z: i32,
	/// Full u16 cell (id + reserved state bits).
	pub block: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ClientMsg {
	/// Append-only from v2 on: the version-first preamble tells the peer which shape to expect.
	Hello {
		protocol: u16,
		name: String,
		draw_dist: i32,
		/// Session token from a previous `Welcome` for future
		/// reconnect-with-state; all zeros = fresh join.
		token: [u8; 16],
		/// `digblock_core::SIM_VERSION` — behavior compatibility gate
		/// (physics + worldgen). Exact match required.
		sim_version: u16,
		/// Informational build string (diagnostics only).
		build: String,
		// v23 REMOVED `identity: [u8; 16]`, and removing rather than
		// deprecating was the point. It was a client-chosen bearer secret
		// that keyed the server's persisted inventory, replayed verbatim to
		// every server a player ever joined — so any server operator held
		// the credential to that player's records everywhere else.
		// Identity now arrives from the MASTER, via
		// /api/tokens/verify, and the client asserts nothing about who it
		// is. A dead 16-byte field that used to mean "trust me" is exactly
		// the kind of thing that gets rewired by accident, so it is gone.
	},
	EditRequest {
		/// Per-connection sequence, echoed back for confirm/reject.
		seq: u32,
		/// Client tick when the click was cast.
		tick: u32,
		/// Claimed eye position at cast time.
		origin: [f32; 3],
		dir: [f32; 3],
		kind: EditKind,
		/// Expected hit cell (dig) / support cell (place).
		cell: [i32; 3],
		/// Place face normal; all zeros is invalid for placement.
		normal: [i8; 3],
		/// The hotbar slot this edit used. Place/Collect must match the
		/// server's copy of that slot (`WrongSlot` otherwise) — declared
		/// here, on the same reliable stream as `Layout`, so a quick
		/// slot-switch-then-click can never race the jitter-buffered
		/// datagram path. Ignored for Dig/Interact. Appended in v10.
		slot: u8,
	},
	Bye,
	/// Hold-to-dig began on `cell`; the server captures the block itself. Replaces any prior hold.
	DigStart {
		cell: [i32; 3],
		/// The hotbar slot the hold is being made with, for the same
		/// reason [`ClientMsg::EditRequest::slot`] carries one: this
		/// rides the reliable stream while the selection otherwise
		/// reaches the server on the jitter-buffered input path, so a
		/// player who picks up the siphon and immediately holds on a
		/// vein would be refused `NeedsTool` for the buffer's depth.
		/// Appended in v27, which is why the floor moved with it — a
		/// decoder expecting this byte runs off the end of an older
		/// peer's frame.
		slot: u8,
	},
	/// Hold released or retargeted before completing. Hygiene only — the server also self-resets.
	DigCancel,
	/// Chat line. Server-side: rate-limited, sanitized, echoed back to everyone, sender included.
	Chat {
		text: String,
	},
	/// Teleport me to spawn; the shared `TELEPORT_COOLDOWN_SECS` window. Answers `Teleported`.
	ReturnToSpawn,
	/// Runtime draw-distance change; the server re-clamps exactly as it does Hello's value.
	SetDrawDist {
		draw_dist: i32,
	},
	/// Alchemy craft. NOT predicted — every op is answered with an `InventoryFull` rebase. In v8.
	Alchemy {
		op: AlchemyOp,
	},
	/// The player's item layout (hotbar wire encoding), sent on any arrangement change. In v10.
	Layout {
		hotbar: Vec<u16>,
		grid: Vec<u16>,
	},
	/// Claim this server's pre-account character: `legacy` is the local hash key. AUTH-DESIGN §6.
	ClaimLegacy {
		legacy: [u8; 16],
	},
	/// This transport has no datagram channel — a capability, announced and never inferred. In v26.
	NoDatagrams,
	/// One datagram that could not travel as one: the encoded [`Datagram`] payload, verbatim.
	Unreliable(Vec<u8>),
	/// A completed cast channel asking for its spell; the server validates unlock and price.
	Cast(CastKind),
	/// The caster broke their Sphere channel; the server also unwinds on its own. Appended in v34.
	CastBreak,
	/// Take the road up now, before its span runs out. No refund: the fare bought the span.
	CausewayEnd,
	/// The spell-bar slot assignments (`InvId::Spell` ids, one byte per slot), sent on any change.
	/// Opaque preference data: the server stores the bytes verbatim, the client validates on read.
	/// Appended in v41.
	SpellBar {
		slots: Vec<u8>,
	},
	/// The Resonant Pick's declared dig shape (`DigOrientation::to_wire`; unknown bytes read
	/// Horizontal). Reliable and in-order with `EditRequest`, so a toggle-then-swing in one tick
	/// is evaluated under the same shape on both ends; the server clamps it against the sender's
	/// Mining unlock exactly as the client does. Appended in v41.
	SetDigOrientation {
		mode: u8,
	},
}

/// What a [`ClientMsg::Cast`] winds up toward — shaped for the whole spell roster from day one so
/// the message never needs a second form. `Kindle` is a RETIRED verb kept decodable and refused
/// (the `AlchemyOp::Convert` pattern): the light lands in the cell the caster's head is in, which
/// the server already knows, so `KindleHere` carries nothing to trust.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum CastKind {
	/// A port to the biome waystone at this `SurfaceKind` declaration-order index.
	Waystone(u8),
	Wisp,
	Kindle {
		cell: [i32; 3],
		face: u8,
	},
	/// Appended in v33.
	KindleHere,
	/// The Sphere: the DECLARED slot holding its block (an edit's rule), every cell server-derived. v39.
	Sphere {
		slot: u8,
	},
	/// The Causeway: begins the road; each plank follows as its own [`EditKind::CausewayStep`].
	Causeway,
}

/// Why a cast was refused — a reason, never a sentence; `voice` chooses
/// the words. Appended in v32.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum CastRefusal {
	/// The destination (or the spell) is beyond the caster's Arcana.
	Locked,
	/// The caster cannot pay the passage. Refused BEFORE any debit.
	CantPay,
	/// A conjuring with no place to land: a head cell that cannot hold light, or a blocked shell.
	BadTarget,
	/// The Sphere's declared slot holds no block. Appended in v39, with the two below.
	NoBlock,
	/// The slot holds a block a sphere cannot be built of (`digblock_core::sphere::accepts`).
	BadBlock,
	/// Fewer of the block in hand than the shell needs. Refused BEFORE any debit.
	BlocksShort,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum RejectReason {
	OutOfReach,
	Occluded,
	InsidePlayer,
	NotLoaded,
	RateLimited,
	BadBlock,
	/// The server cannot persist edits right now (journal unhealthy).
	ServerError,
	/// Timed dig not satisfied: no hold on record for this cell, or too few elapsed ticks.
	DigIncomplete,
	/// Place refused: no stock of that block; an `InventoryFull` snapshot follows to re-base.
	NoStock,
	/// Place/Collect refused: the declared slot doesn't hold the item the edit claims. In v10.
	WrongSlot,
	/// The cell may never be modified: the world floor, or a region an admin has locked.
	Protected,
	/// Collect refused: the water tool is already charged; an honest client never aims with one.
	BucketFull,
	/// Dig refused by `unlock::mine` (§5.1's strength wall); sent at `DigStart`, like `Protected`.
	SkillGated,
	/// Dig refused: craft-only, and the digger is below its `unlock::pickup` level. In v19.
	NotUnlocked,
	/// Dig refused: the cell shows this player a revealed vein and the tool is not the siphon.
	NeedsTool,
	/// Place refused: the chunk's slot budget (`digblock_core::budget`) has no room for the
	/// shape's template. In v42; older decoders get `Occluded`.
	TooDetailed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ServerMsg {
	Refused {
		reason: String,
	},
	/// Append-only across supported protocol versions from v2 on.
	Welcome {
		protocol: u16,
		seed: f64,
		player_id: PlayerId,
		spawn: [f32; 3],
		server_tick: u32,
		snapshot_divisor: u32,
		/// Server-issued token the client presents on a future resume.
		session_token: [u8; 16],
		/// Server's `digblock_core::SIM_VERSION`.
		sim_version: u16,
		/// Informational build string (diagnostics only).
		build: String,
		/// The server's current time-of-day angle (see
		/// `digblock_core::sun`) — anchors the joiner's clock; the
		/// `TimeOfDay` heartbeat keeps it anchored. Appended in v7.
		tod: f32,
		/// The facing to adopt on arrival — the admin's `/set-spawn-dir`,
		/// sent only when this join actually placed the player AT spawn.
		/// Zero = keep the current view (the `Teleported.look`
		/// convention); a resume in place always sends zero, since the
		/// spawn facing describes spawn, not wherever you logged out.
		/// Appended in v13.
		spawn_look: [f32; 3],
		/// Which generator made this world, NOT which one the server
		/// prefers: `seed` alone is half a world, and the pair is what
		/// both ends must generate from. A client that does not carry it
		/// refuses the join — the only thing worldgen can refuse for now.
		/// Appended in v37, which is why the floor moved with it.
		terrain: u16,
	},
	/// Join/late sync: last-write-wins final cell states for one column.
	JournalColumn {
		cx: i32,
		cz: i32,
		cells: Vec<BlockEdit>,
	},
	JournalDone {
		through_seq: u64,
	},
	EditApplied {
		/// Server-global edit sequence (journal order).
		seq: u64,
		tick: u32,
		by: PlayerId,
		/// The author's request seq; meaningful only when `by` is you,
		/// and `NO_CLIENT_SEQ` for a write no request named (a door's
		/// companion half, the fluid sim, a world-job sweep).
		client_seq: u32,
		edit: BlockEdit,
		/// Block cell that was replaced (dig pickup).
		prev: u16,
	},
	EditRejected {
		client_seq: u32,
		reason: RejectReason,
	},
	PlayerJoined {
		id: PlayerId,
		name: String,
	},
	PlayerLeft {
		id: PlayerId,
	},
	/// Authoritative inventory snapshot; also sent on `NoStock`/`WrongSlot` to repair count drift.
	InventoryFull {
		counts: Vec<(u16, u32)>,
	},
	/// A player's chat line, already sanitized; `from` resolves via the PlayerJoined roster.
	Chat {
		from: PlayerId,
		text: String,
	},
	/// The server moved YOUR player. Hard-set — the client drops its prediction buffer and acks.
	Teleported {
		pos: [f32; 3],
		/// Facing to adopt on arrival; all-zeros = keep the current
		/// view (return-to-spawn). Appended in v9.
		look: [f32; 3],
	},
	/// Authoritative world-clock heartbeat, every `TOD_SYNC_TICKS`: the client hard-sets its sun.
	TimeOfDay {
		tod: f32,
	},
	/// The server's chat-command set, sent only to an admin: `(usage, description)` pairs.
	AdminCommands {
		commands: Vec<(String, String)>,
	},
	/// The identity's persisted item layout, sent right after `InventoryFull` when one is stored.
	Layout {
		hotbar: Vec<u16>,
		grid: Vec<u16>,
	},
	/// A hold refused at `DigStart`, answered by cell; only for refusals no client can predict.
	DigRefused {
		cell: [i32; 3],
		reason: RejectReason,
	},
	/// Full per-skill XP sync: `(skill index, total)` pairs; always sent in join sync, even empty.
	Skills {
		xp: Vec<(u8, u64)>,
	},
	/// One skill's new TOTAL after an award — a total, not a delta: idempotent and reorder-safe.
	XpGain {
		skill: u8,
		total: u64,
	},
	/// Another player reached `level` in `skill`; sent to everyone BUT the author, who has XpGain.
	LevelUp {
		player: PlayerId,
		skill: u8,
		level: u32,
	},
	/// The ground under you is real now; the client holds the world from `Welcome` until it lands.
	AreaReady,
	/// A snapshot for a `NoDatagrams` client: the encoded payload, DROPPED on a full queue.
	Unreliable(Vec<u8>),
	/// An admin command's answer, as typed data; the sentence is `voice`'s. Appended in v29.
	Admin(AdminReply),
	/// The world's RESOLVED waystones — server state the client cannot derive from the seed.
	Waystones {
		entries: Vec<(u8, [f32; 3])>,
	},
	/// A cast the server would not perform, and why; honest clients see this only across a race.
	CastRefused {
		reason: CastRefusal,
	},
	/// Conjured light in `cell` with the seconds left; a light outlives the session that lit it.
	Kindled {
		cell: [i32; 3],
		secs_left: u16,
	},
	/// A Sphere channel BEGAN: the CELLS never travel, both ends deriving them from head, radius and block. v39.
	SphereWinding {
		by: PlayerId,
		head: [i32; 3],
		radius: u8,
		span: u32,
		block: u16,
	},
	/// A Sphere's channel ENDED: `landed` cells of that plan reached the world; `sealed` is carried.
	SphereEnded {
		by: PlayerId,
		head: [i32; 3],
		radius: u8,
		landed: u32,
		sealed: bool,
		block: u16,
	},
	/// The identity's stored spell-bar assignments, sent right after `Layout` in join sync — but
	/// only to peers whose Hello spoke v41+ (an older client cannot decode the variant). In v41.
	SpellBar {
		slots: Vec<u8>,
	},
}

#[cfg(test)]
mod tests;
