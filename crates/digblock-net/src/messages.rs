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

/// Which command a usage line is for.
///
/// The COMMAND is the server's fact; the line describing it is the
/// client's. Naming the command rather than sending its prose is what
/// lets `/give` advertise the essences from `Essence::ALL` at the point
/// of rendering, so the advertisement and the parser cannot drift.
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
/// Admin replies used to cross the wire as English inside
/// `ServerMsg::Chat` from `SERVER_ID`, which put player-visible text in
/// the server — the one place the third writing rule says it may not
/// live — and left every test asserting the words a handler prints
/// rather than what it did. Appended in v29.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum AdminReply {
	Usage(AdminUsage),
	/// One row of the command catalog, echoed by `/help`. Carries the
	/// catalog's own strings rather than a variant: the usage stem is the
	/// dispatch key AND the client's autocomplete stem, so it is an
	/// identifier the client needs verbatim, not a sentence to translate.
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
	/// The aimed spot cannot hold a spawn: the clearance it would need
	/// rides along so the line can state it without knowing the rule.
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
	/// Pinned by hand but not protected: the point stays put, its cells
	/// can still be dug.
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
	/// A pre-account character on this server was folded into the account
	/// that just joined (AUTH-DESIGN §6).
	LegacyCharacterClaimed,
	/// A slash line from a player who is not an admin.
	CommandsAreAdminOnly,
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

/// YOUR own authoritative state. Full `f32` precision, deliberately:
/// `predict.rs` compares the echo against its claim within `POS_EPS2`, so
/// a claim the server adopted has to round-trip bit-comparably or EVERY
/// snapshot takes the rewind-and-replay path. This is the one place in a
/// snapshot where precision is load-bearing rather than cosmetic.
///
/// No `look`: the server echoes back the direction we sent it, and nothing
/// on the client reads it. No `id` either — it is ours by construction.
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
	/// The player's visible effect, packed by [`pack_fx`]: the wisp they
	/// burn today, with a kind reserved for the siphon draw's remote half
	/// (which needs the drawn CELL, a field of its own when it lands).
	/// Appending a struct field is the one genuinely breaking postcard
	/// change, which is what the v32 `MIN_PROTOCOL` raise carries.
	/// Appended in v32.
	pub fx: u8,
}

/// The remote-effect kinds [`RemoteState::fx`] can carry. Two bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoteFx {
	None,
	/// A burning wisp; the argument is its cast light level, with the
	/// whitened flag beside it — a remote's wisp must cast exactly what
	/// its caster earned, and their skills are not in the snapshot.
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

/// `held` sentinel for a CHARGED water tool.
///
/// It lives here rather than in `digblock_core::hotbar` because the
/// arrangement encoding has no business knowing about it: a slot holds
/// the tool, never the tool's charge, and `Hotbar::from_wire` treats
/// this value as the unknown id it is. Appended in v12.
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

/// Interpolate between two wire yaws along the SHORTEST arc.
///
/// Done in `u16` space on purpose: `wrapping_sub` on the difference is
/// exactly the signed shortest arc, with the wrap at north handled for
/// free. Lerping the decoded radians instead would sweep a player the long
/// way round whenever they crossed the discontinuity — the old code dodged
/// that only because it lerped the look VECTOR and renormalized.
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
	/// `block` is a full u16 CELL (id + reserved state bits) — postcard
	/// varints keep small ids narrow anyway. The server rejects nonzero
	/// state bits until something consumes them.
	Place {
		block: u16,
	},
	/// Water-tool collect: validated with the fluid-aware raycast; the
	/// hit must be a fluid source. Applies Air and charges the bucket
	/// (one unit of Water). Subject to the same hold-to-dig timing gate
	/// as `Dig`, and refused with `BucketFull` if the tool already holds
	/// one — the bucket carries a charge, not a stack.
	// (PlaceArc is appended at the enum's tail — postcard is append-only.)
	Collect,
	/// Use/interact click (door open-close toggle): validated with the
	/// interact-aware raycast (`raycast_interact` — an open door's swung
	/// leaf still stops it); the hit must be interactive. `normal` may
	/// be zero. Appended in v8.
	Interact,
	/// A Mason's Arc placement: `Place`, extended into a full line of
	/// `unlock::arc_cells` along the verified face normal — all or
	/// nothing, so both ends derive one set or the request bounces.
	/// Refused for slabs, doors and water (each carries machinery of its
	/// own the line must not multiply). Appended in v32.
	PlaceArc {
		block: u16,
	},
}

/// One alchemy request (wire form; ids are validated server-side against
/// the core tables). Appended in v8.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum AlchemyOp {
	/// RETIRED (v28, the essence redesign): bench extraction is gone, so
	/// the server refuses this outright — nothing debited, the standing
	/// `InventoryFull` rebase as the whole answer. The variant stays
	/// because the wire is append-only: a frame that stops decoding is a
	/// session that dies mid-parse instead of a verb that stopped working.
	Convert { block: u16, count: u32 },
	/// Craft `count` batches of the shaped item `item` from essence and
	/// feedstock.
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
	/// Append-only across supported protocol versions from v2 on (the
	/// version-first preamble tells the peer which shape to expect, and
	/// postcard tolerates trailing bytes).
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
	/// Hold-to-dig began on `cell` (dig or water-collect — the server
	/// captures the cell's current block itself; a content change resets
	/// the hold). Replaces any prior hold for this client.
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
	/// Hold released or retargeted before completing. Hygiene only — the
	/// server also self-resets via its per-tick aim check.
	DigCancel,
	/// Chat line. Server-side: token-bucket limited, sanitized to
	/// printable ASCII, broadcast back to everyone (sender included —
	/// the echo is the delivery confirmation).
	Chat {
		text: String,
	},
	/// Teleport me back to the spawn point. Server-side: subject to the
	/// per-client [`TELEPORT_COOLDOWN_SECS`] window it SHARES with every
	/// port (violations are ignored — the menu enforces it honestly);
	/// answered with `ServerMsg::Teleported`.
	///
	/// [`TELEPORT_COOLDOWN_SECS`]: crate::protocol::TELEPORT_COOLDOWN_SECS
	ReturnToSpawn,
	/// Runtime draw-distance change (settings slider). Server re-clamps
	/// exactly like Hello's value and adjusts the sync radius from the
	/// next tick.
	SetDrawDist {
		draw_dist: i32,
	},
	/// Alchemy convert/craft. NOT client-predicted — the server answers
	/// every op (success or failure) with an `InventoryFull` rebase.
	/// Rides the edit action queue in wire order, so craft-then-place in
	/// one tick debits before the place's stock gate. Appended in v8.
	Alchemy {
		op: AlchemyOp,
	},
	/// The player's item layout (hotbar wire encoding: `WIRE_EMPTY`/
	/// `WIRE_TOOL`/block id), sent on any arrangement change. Rides the
	/// edit action queue in wire order, so assign-to-bar-then-place in
	/// one tick validates against the new bar. The server sanitizes,
	/// enforces Place/Collect slots against it, echoes it in snapshots'
	/// held item, and persists it with the identity's inventory.
	/// Appended in v10.
	Layout {
		hotbar: Vec<u16>,
		grid: Vec<u16>,
	},
	/// Claim the pre-account character this server holds for `legacy`, and fold
	/// it into the account that sent this. Appended in v24.
	///
	/// `legacy` is the LOOKUP KEY those records were filed under — the truncated
	/// hash of the old device secret, computed locally. The secret itself never
	/// leaves the machine, and there is nothing a server could learn here that
	/// it could present elsewhere as an identity.
	///
	/// It is still a bearer credential for one thing: whoever presents it first
	/// gets those records. That is unavoidable — the server stores only the
	/// hash, so it has nothing to run a challenge against — and it is bounded,
	/// because a successful claim DELETES the legacy entry. See AUTH-DESIGN.md
	/// §6.
	///
	/// Sent once per connection, immediately after `Hello`. In-order on the
	/// reliable stream, so the server already knows which account is asking.
	/// Unmatched claims are silent.
	ClaimLegacy {
		legacy: [u8; 16],
	},
	/// This client's transport has no datagram channel: its unreliable traffic
	/// will arrive as [`ClientMsg::Unreliable`] on this stream, and the server
	/// must answer in kind ([`ServerMsg::Unreliable`]). Appended in v26.
	///
	/// Sent once per connection, immediately after `Hello`, by a client whose
	/// transport says so — today only iOS Safari, whose `WebTransport.datagrams`
	/// has no writable side. In-order on the reliable stream, so it lands before
	/// the first snapshot the server would otherwise send where this client
	/// cannot hear it.
	///
	/// A capability, deliberately not a preference. Announcing it rather than
	/// inferring it from the first `Unreliable` frame is what keeps the two
	/// directions from disagreeing.
	NoDatagrams,
	/// One datagram that could not travel as one: the encoded
	/// [`Datagram`] payload, verbatim. Appended in v26.
	///
	/// The payload is bit-identical to what the datagram would have
	/// carried, so both ends decode it through exactly the same path —
	/// the fallback owes its correctness to having no second encoding to
	/// drift from. What it loses is the lossiness: these are delivered
	/// and ordered like everything else on this stream, so a lost packet
	/// costs a retransmit instead of a skipped frame. That is the whole
	/// trade, and it is the right way round for a client that would
	/// otherwise be unable to play at all.
	Unreliable(Vec<u8>),
	/// A completed cast channel asking for its spell. The channel itself
	/// is client pacing (the `handle_return_to_spawn` stance: honest
	/// clients gate locally); the server validates the UNLOCK and the
	/// PRICE, which are the real gates, and — for anything that MOVES
	/// the caster — the shared [`TELEPORT_COOLDOWN_SECS`] window.
	/// Appended in v32.
	///
	/// [`TELEPORT_COOLDOWN_SECS`]: crate::protocol::TELEPORT_COOLDOWN_SECS
	Cast(CastKind),
}

/// What a [`ClientMsg::Cast`] winds up toward — shaped for the whole
/// spell roster from day one so the message never needs a second form.
/// `Kindle` is a RETIRED verb kept decodable and refused (the
/// `AlchemyOp::Convert` pattern): the light no longer lands where the
/// caster aims but in the cell their head is in, which the server
/// already knows — so `KindleHere` carries nothing to trust.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum CastKind {
	/// A port to the biome waystone at this `SurfaceKind`
	/// declaration-order index.
	Waystone(u8),
	Wisp,
	Kindle {
		cell: [i32; 3],
		face: u8,
	},
	/// Appended in v33.
	KindleHere,
	/// The Ward: a glass sphere about the caster's head, every cell
	/// derived on the server (`digblock_core::ward`) — so, like
	/// `KindleHere`, it carries nothing to trust. Appended in v34.
	Ward,
}

/// Why a cast was refused — a reason, never a sentence; `voice` chooses
/// the words. Appended in v32.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum CastRefusal {
	/// The destination (or the spell) is beyond the caster's Arcana.
	Locked,
	/// The caster cannot pay the passage. Refused BEFORE any debit.
	CantPay,
	/// A conjuring with no place to land: a kindle whose head cell
	/// cannot hold light, or a ward whose shell would close on a player
	/// or over ground not resident.
	BadTarget,
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
	/// Timed dig not satisfied: no hold on record for this cell, or not
	/// enough elapsed ticks (hardness minus the grace).
	DigIncomplete,
	/// Place refused: no stock of that block (an `InventoryFull` snapshot
	/// follows so the client can re-base).
	NoStock,
	/// Place/Collect refused: the declared hotbar slot doesn't hold the
	/// item the edit claims to use. Honest clients never see this — the
	/// slot rides the same reliable stream as `Layout`. Appended in v10.
	WrongSlot,
	/// The cell may never be modified: the world floor, or a region an
	/// admin has locked. Honest clients predict this locally and never
	/// send the edit; the server rejects it regardless. Appended in v11.
	Protected,
	/// Collect refused: the water tool is already charged. An honest
	/// client never aims the scoop with a full bucket — the fluid ray is
	/// only picked while it's empty. Appended in v12.
	BucketFull,
	/// Dig refused: the block's `unlock::mine` level exceeds the
	/// digger's Mining level (SKILLS-DESIGN §5.1's strength wall). Sent at
	/// `DigStart` like `Protected`, so the player isn't left holding the
	/// button; honest clients predict it in `diggable()` and never open
	/// the hold. Appended in v17.
	SkillGated,
	/// Dig refused: the block is craft-only and the digger's Alchemy level
	/// is below its `unlock::craft` level, so the pickup would hand out
	/// an item they cannot make (`unlock::pickup`). Rides the
	/// same `DigStart` path as `SkillGated`; predicted in `diggable()`.
	/// Appended in v19.
	NotUnlocked,
	/// Dig refused: the cell shows this player a revealed ore vein and
	/// their active tool is not the Essence Siphon (SKILLS-DESIGN §5.3).
	/// If you can SEE the seam, the game will not let you spend it on a
	/// stone dig by accident. Sent at `DigStart` like `SkillGated`, so
	/// nobody holds the button through a timer for nothing; predicted in
	/// `diggable()`. Appended in v21.
	NeedsTool,
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
	/// Authoritative inventory snapshot: (block id, count) pairs, absent
	/// = zero. Sent after `JournalDone` on join and alongside every
	/// `NoStock` and `WrongSlot` rejection (drift-repair backstop — both
	/// are how a drifted count presents); every other transition is
	/// implicit in the predict/confirm/reject flow.
	InventoryFull {
		counts: Vec<(u16, u32)>,
	},
	/// A player's chat line, already sanitized. `from` resolves to a
	/// display name via the client's PlayerJoined roster.
	Chat {
		from: PlayerId,
		text: String,
	},
	/// The server moved YOUR player (return to spawn, admin /tp).
	/// Hard-set — the client drops its prediction buffer and in-flight
	/// acks instead of smoothing across the map. Remote players just see
	/// the jump in snapshots.
	Teleported {
		pos: [f32; 3],
		/// Facing to adopt on arrival; all-zeros = keep the current
		/// view (return-to-spawn). Appended in v9.
		look: [f32; 3],
	},
	/// Authoritative world-clock heartbeat, broadcast every
	/// [`TOD_SYNC_TICKS`](crate::protocol::TOD_SYNC_TICKS): the client
	/// hard-sets its sun to this angle (the drift since the last beat is
	/// far below visible). Added in v7.
	TimeOfDay {
		tod: f32,
	},
	/// The server's chat-command set, sent only to clients that may use
	/// it (the configured admin) after join sync. `(usage, description)`
	/// pairs, e.g. `("/tp <x> <z> [n|s|e|w]", "teleport to column
	/// top")`. Drives the client's chat Tab-autocomplete; the client
	/// bakes in no command knowledge. Added in v9.
	AdminCommands {
		commands: Vec<(String, String)>,
	},
	/// The identity's persisted item layout, sent in join sync right
	/// after `InventoryFull` (only when one is stored). The client
	/// sanitizes and reconciles it against the counts. Appended in v10.
	Layout {
		hotbar: Vec<u16>,
		grid: Vec<u16>,
	},
	/// A hold-to-dig was refused at `DigStart`, before any timer ran.
	/// `DigStart` carries no seq, so this answers by cell instead.
	///
	/// Only sent for refusals the client cannot predict — a protected
	/// region it has no knowledge of. Ordinary undiggable targets stay
	/// silent (the client's own `diggable` check already agrees), so this
	/// never becomes chatter on a mistimed swing.
	///
	/// LAST on purpose: postcard encodes the variant index, so this must
	/// be appended, never inserted. Appended in v11.
	DigRefused {
		cell: [i32; 3],
		reason: RejectReason,
	},
	/// Full per-skill XP sync: `(skill index, total)` pairs, absent =
	/// zero. Sent in join sync after `Layout` — always, even when empty
	/// (a fresh identity), so the client never guesses. Levels are
	/// derived client-side from the shared curve. Appended in v16.
	Skills {
		xp: Vec<(u8, u64)>,
	},
	/// One skill's new TOTAL after a server-side award. A total rather
	/// than a delta on purpose: idempotent and reorder-safe — the client
	/// takes the max and animates the difference itself. Appended in
	/// v16.
	XpGain {
		skill: u8,
		total: u64,
	},
	/// Another player just reached `level` in `skill` — the cue for the
	/// confetti over their head. Sent to everyone BUT the author (who
	/// learns it from their own `XpGain` and celebrates locally), so a
	/// client never has to filter its own id out. Carries the derived
	/// level rather than a total: nobody else's XP is any of your
	/// business, and the level is all the presentation needs. Appended in
	/// v18.
	LevelUp {
		player: PlayerId,
		skill: u8,
		level: u32,
	},
	/// Every journaled column within `AREA_SYNC_RADIUS` of where you were just
	/// put has been sent — the ground under you is the real one now.
	///
	/// The client holds the world (no render, no player sim) from `Welcome` or
	/// `Teleported` until this lands, because until then it is standing on
	/// pristine worldgen the journal is about to overwrite: on a spawn anyone
	/// has dug out, the arrival pose is a guess the first snapshot then
	/// corrects, which reads as being *moved* after joining.
	///
	/// Deliberately not `JournalDone`, which is sent at handshake time BEFORE
	/// any column goes out. One message covers both entrances, and it is sent
	/// immediately when the radius holds no edits at all. Appended in v20.
	AreaReady,
	/// A snapshot for a client that announced [`ClientMsg::NoDatagrams`]:
	/// the encoded [`Datagram`] payload, verbatim. Appended in v26.
	///
	/// Sent only to such a client, and dropped on a full outbound queue
	/// exactly as the datagram it stands in for would be — a snapshot
	/// stays lossy at the SEND, whatever carries it. That distinction is
	/// load-bearing: every other frame on this stream drops the player
	/// when the queue refuses it, and a stalled reader must not be killed
	/// by the one message class that has always tolerated a stall.
	Unreliable(Vec<u8>),
	/// An admin command's answer, as typed data. Replaces the English
	/// that used to ride `Chat` from `SERVER_ID`; the sentence is now
	/// `voice`'s. Appended in v29.
	Admin(AdminReply),
	/// The world's RESOLVED waystones — `(SurfaceKind index, eye)` — sent
	/// once in join sync after `Skills`. Server state the client cannot
	/// derive: each eye was resolved against the journal ONCE and stored,
	/// so re-deriving from the seed would disagree exactly on damaged
	/// worlds. Appended in v32.
	Waystones {
		entries: Vec<(u8, [f32; 3])>,
	},
	/// A cast the server would not perform, and why — the spellbook greys
	/// what it can predict, so an honest client sees this only across a
	/// race (levelling down is impossible, but spending essence between
	/// build and click is not). Appended in v32.
	CastRefused {
		reason: CastRefusal,
	},
	/// Conjured light standing in `cell`, with the seconds it has left —
	/// sent to everyone on the cast and to a joiner for each light still
	/// burning. The light is per-viewer GI and the expiry is each
	/// client's own clock from here; the server's list is what a joiner
	/// and a departed caster both rely on, since a light outlives the
	/// session that lit it. Appended in v33.
	Kindled {
		cell: [i32; 3],
		secs_left: u16,
	},
}

#[cfg(test)]
mod tests;
