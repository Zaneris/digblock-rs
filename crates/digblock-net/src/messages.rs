//! Wire message types. Plain arrays instead of glam types so the wire
//! format has no dependency on math-library serde impls; conversion
//! happens at the edges. Append-only — see protocol.rs.

use crate::protocol::PlayerId;
use serde::{Deserialize, Serialize};

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
/// `predict.rs`'s `POS_EPS2` is 1e-6, so the echo of a claim the server
/// adopted has to round-trip bit-comparably or EVERY snapshot takes the
/// rewind-and-replay path. This is the one place in a snapshot where
/// precision is load-bearing rather than cosmetic.
///
/// No `look`: the server echoes back the direction we sent it, and nothing
/// on the client reads it. No `id` either — it is ours by construction.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct SelfState {
	pub pos: [f32; 3],
	pub vel: [f32; 3],
}

/// Somebody else, as the client actually consumes them. ~18 wire bytes for
/// a walking player (28 worst case) against the flat 40 of the old shared
/// struct (v13), which sent three f32s of `look` where only a yaw is ever
/// read and three more of `vel` to drive a 250 ms-capped extrapolation.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct RemoteState {
	pub id: PlayerId,
	/// Still `f32`: a remote's position is drawn directly, and quantizing
	/// it would show up as stepping under interpolation.
	pub pos: [f32; 3],
	/// Velocity in thousandths of a block/s, clamped to `MAX_SPEED`
	/// (25 000 — comfortably inside `i16`). Feeds only the capped
	/// extrapolation in `remote.rs`, so a millimetre/s of resolution is
	/// already far past what one frame can show.
	pub vel: [i16; 3],
	/// Facing as a whole-turn fraction: `0..=u16::MAX` maps onto `0..2π`,
	/// giving 0.0055° of resolution. PITCH IS NOT SENT — `remote.rs` has
	/// only ever computed `look.x.atan2(look.z)`, and remote players are
	/// drawn upright.
	pub yaw: u16,
	/// What the player is holding, in hotbar wire encoding
	/// (`WIRE_EMPTY`/`WIRE_TOOL`/[`HELD_TOOL_FULL`]/block id) — remote
	/// clients render it in hand. Appended in v10.
	pub held: u16,
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
	/// varints keep small ids at one wire byte. The server rejects
	/// nonzero state bits until something consumes them.
	Place {
		block: u16,
	},
	/// Water-tool collect: validated with the fluid-aware raycast; the
	/// hit must be a fluid source. Applies Air and charges the bucket
	/// (one unit of Water). Subject to the same hold-to-dig timing gate
	/// as `Dig`, and refused with `BucketFull` if the tool already holds
	/// one — the bucket carries a charge, not a stack.
	Collect,
	/// Use/interact click (door open-close toggle): validated with the
	/// interact-aware raycast (`raycast_interact` — an open door's swung
	/// leaf still stops it); the hit must be interactive. `normal` may
	/// be zero. Appended in v8.
	Interact,
}

/// One alchemy request (wire form; ids are validated server-side against
/// the core tables). Appended in v8.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum AlchemyOp {
	/// Break `count` whole blocks of `block` into sub-material units.
	Convert { block: u16, count: u32 },
	/// Craft `count` batches of the shaped item `item` from sub-material.
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
		/// Persistent 16-byte client identity secret; all zeros =
		/// anonymous (no persistent inventory, no duplicate check). The
		/// server stores only a hash of it.
		identity: [u8; 16],
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
		/// Place face normal; (0,0,0) is invalid for placement.
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
	/// per-client [`RETURN_SPAWN_COOLDOWN_SECS`] window (violations are
	/// ignored — the menu enforces it honestly); answered with
	/// `ServerMsg::Teleported`.
	///
	/// [`RETURN_SPAWN_COOLDOWN_SECS`]: crate::protocol::RETURN_SPAWN_COOLDOWN_SECS
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
		/// The author's request seq; meaningful only when `by` is you.
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
	/// `NoStock` rejection (drift-repair backstop); every other
	/// transition is implicit in the predict/confirm/reject flow.
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
	/// Every journaled column within `AREA_SYNC_RADIUS` of where you were
	/// just put has been sent — the ground under you is the real one now.
	///
	/// The client holds the world (no render, no player sim) from `Welcome`
	/// or `Teleported` until this lands, because until then it is standing
	/// on pristine worldgen the journal is about to overwrite: on a spawn
	/// anyone has dug out, the arrival pose is a guess that the first
	/// snapshot then corrects, which reads as being *moved* after joining.
	///
	/// It is deliberately not `JournalDone`, which is sent at handshake
	/// time BEFORE any column goes out and only names a sequence number.
	/// One message covers both entrances: a join and a teleport are the
	/// same event to the world around you. Sent immediately when the
	/// radius holds no edits at all, so an untouched world never waits.
	/// Appended in v20.
	AreaReady,
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::codec;

	#[test]
	fn round_trip_all_variants() {
		let msgs = vec![
			ClientMsg::Hello {
				protocol: 2,
				name: "ted".into(),
				draw_dist: 16,
				token: [7; 16],
				sim_version: 1,
				build: "0.1.0".into(),
				identity: [3; 16],
			},
			ClientMsg::EditRequest {
				seq: 7,
				tick: 1234,
				origin: [1.0, 140.0, -3.0],
				dir: [0.0, -1.0, 0.0],
				kind: EditKind::Place { block: 5 },
				cell: [1, 138, -3],
				normal: [0, 1, 0],
				slot: 2,
			},
			ClientMsg::EditRequest {
				seq: 8,
				tick: 1240,
				origin: [1.0, 140.0, -3.0],
				dir: [0.0, -1.0, 0.0],
				kind: EditKind::Collect,
				cell: [1, 138, -3],
				normal: [0, 0, 0],
				slot: 8,
			},
			ClientMsg::Bye,
			ClientMsg::DigStart { cell: [1, 138, -3] },
			ClientMsg::DigCancel,
			ClientMsg::Chat { text: "hi".into() },
			ClientMsg::ReturnToSpawn,
			ClientMsg::EditRequest {
				seq: 9,
				tick: 1250,
				origin: [1.0, 140.0, -3.0],
				dir: [1.0, 0.0, 0.0],
				kind: EditKind::Interact,
				cell: [2, 138, -3],
				normal: [0, 0, 0],
				slot: 0,
			},
			ClientMsg::Alchemy {
				op: AlchemyOp::Convert {
					block: 6,
					count: 10,
				},
			},
			ClientMsg::Alchemy {
				op: AlchemyOp::Craft { item: 24, count: 2 },
			},
			ClientMsg::Layout {
				hotbar: vec![2, 5, u16::MAX, u16::MAX - 1],
				grid: vec![7, u16::MAX],
			},
		];
		for m in msgs {
			let mut buf = Vec::new();
			codec::write_frame(&m, &mut buf).unwrap();
			let mut r = codec::FrameReader::default();
			r.extend(&buf);
			let frame = r.next_frame().unwrap().unwrap();
			let back: ClientMsg = codec::decode(&frame).unwrap();
			assert_eq!(format!("{m:?}"), format!("{back:?}"));
		}

		let msgs = vec![
			ServerMsg::Refused {
				reason: "full".into(),
			},
			ServerMsg::Welcome {
				protocol: 2,
				seed: 42.125,
				player_id: 3,
				spawn: [0.5, 140.5, 0.5],
				server_tick: 600,
				snapshot_divisor: 3,
				session_token: [9; 16],
				sim_version: 1,
				build: "0.1.0".into(),
				tod: 45.0,
				spawn_look: [1.0, 0.0, 0.0],
			},
			ServerMsg::TimeOfDay { tod: -12.5 },
			ServerMsg::JournalColumn {
				cx: -2,
				cz: 9,
				cells: vec![BlockEdit {
					x: -30,
					y: 130,
					z: 150,
					block: 0,
				}],
			},
			ServerMsg::JournalDone { through_seq: 88 },
			ServerMsg::EditApplied {
				seq: 89,
				tick: 660,
				by: 3,
				client_seq: 8,
				edit: BlockEdit {
					x: 1,
					y: 138,
					z: -3,
					block: 5,
				},
				prev: 0,
			},
			ServerMsg::EditRejected {
				client_seq: 9,
				reason: RejectReason::OutOfReach,
			},
			ServerMsg::EditRejected {
				client_seq: 10,
				reason: RejectReason::DigIncomplete,
			},
			ServerMsg::EditRejected {
				client_seq: 11,
				reason: RejectReason::NoStock,
			},
			ServerMsg::PlayerJoined {
				id: 4,
				name: "bot".into(),
			},
			ServerMsg::PlayerLeft { id: 4 },
			ServerMsg::InventoryFull {
				counts: vec![(2, 3), (5, 1), (0x8000, 54), (0x8001, 27)],
			},
			ServerMsg::Chat {
				from: 4,
				text: "hi".into(),
			},
			ServerMsg::Teleported {
				pos: [0.5, 140.5, 0.5],
				look: [0.0, 0.0, -1.0],
			},
			ServerMsg::AdminCommands {
				commands: vec![("/ban <name>".into(), "ban by IP".into())],
			},
			ServerMsg::Layout {
				hotbar: vec![2, 5, u16::MAX - 1],
				grid: vec![u16::MAX, 7],
			},
			ServerMsg::DigRefused {
				cell: [1, 138, -3],
				reason: RejectReason::Protected,
			},
			ServerMsg::Skills {
				xp: vec![(0, 1_068), (1, 394)],
			},
			ServerMsg::XpGain {
				skill: 0,
				total: 1_103_585,
			},
			ServerMsg::LevelUp {
				player: 3,
				skill: 1,
				level: 42,
			},
		];
		for m in msgs {
			let mut buf = Vec::new();
			codec::write_frame(&m, &mut buf).unwrap();
			let mut r = codec::FrameReader::default();
			r.extend(&buf);
			let frame = r.next_frame().unwrap().unwrap();
			let back: ServerMsg = codec::decode(&frame).unwrap();
			assert_eq!(format!("{m:?}"), format!("{back:?}"));
		}

		let d = Datagram::Snapshot(Snapshot {
			server_tick: 999,
			your_last_input_tick: 998,
			me: Some(SelfState {
				pos: [4.0, 5.0, 6.0],
				vel: [0.0, -1.0, 0.0],
			}),
			others: vec![RemoteState {
				id: 1,
				pos: [1.0, 2.0, 3.0],
				vel: [0, -1000, 0],
				yaw: 1234,
				held: 5,
			}],
		});
		let bytes = codec::encode_datagram(&d).unwrap();
		let back = codec::decode_datagram(&bytes).unwrap();
		assert_eq!(format!("{d:?}"), format!("{back:?}"));
	}

	/// The client-direction frame cap has to clear every message a client
	/// legitimately sends, at its widest — otherwise the server drops
	/// honest players for "stream corrupt".
	///
	/// Each case below is built at its maximum: a name at `MAX_NAME_LEN`,
	/// a chat line at `MAX_CHAT_LEN` in two-byte characters, a full
	/// hotbar+grid layout of three-byte varint ids.
	#[test]
	fn max_client_frame_fits_every_client_message() {
		use crate::protocol::{MAX_CHAT_LEN, MAX_CLIENT_FRAME, MAX_NAME_LEN};
		let widest = [
			(
				"hello",
				ClientMsg::Hello {
					protocol: u16::MAX,
					name: "é".repeat(MAX_NAME_LEN),
					draw_dist: i32::MAX,
					token: [0xff; 16],
					sim_version: u16::MAX,
					build: "0.0.0-somewhat-long-prerelease-tag".into(),
					identity: [0xff; 16],
				},
			),
			(
				"chat",
				ClientMsg::Chat {
					text: "é".repeat(MAX_CHAT_LEN),
				},
			),
			(
				"layout",
				ClientMsg::Layout {
					hotbar: vec![u16::MAX; digblock_core::hotbar::HOTBAR_SLOTS],
					grid: vec![u16::MAX; digblock_core::inv_grid::GRID_SLOTS],
				},
			),
			(
				"edit",
				ClientMsg::EditRequest {
					seq: u32::MAX,
					tick: u32::MAX,
					origin: [f32::MIN; 3],
					dir: [f32::MIN; 3],
					kind: EditKind::Place { block: u16::MAX },
					cell: [i32::MIN; 3],
					normal: [-1, -1, -1],
					slot: u8::MAX,
				},
			),
			(
				"alchemy",
				ClientMsg::Alchemy {
					op: AlchemyOp::Craft {
						item: u16::MAX,
						count: u32::MAX,
					},
				},
			),
		];
		for (name, msg) in widest {
			let mut buf = Vec::new();
			crate::codec::write_frame(&msg, &mut buf).expect("encodes");
			assert!(
				buf.len() <= MAX_CLIENT_FRAME,
				"{name} is {} bytes, over the {MAX_CLIENT_FRAME}-byte client cap",
				buf.len()
			);
		}
	}

	/// A full snapshot must fit one datagram at the raised
	/// `MAX_SNAPSHOT_PLAYERS`, with every field at its worst-case width —
	/// the `const_assert` in protocol.rs asserts the arithmetic, this
	/// asserts the actual encoder agrees with it.
	///
	/// The old cliff this guards: before interest management, snapshots
	/// silently exceeded `MAX_DATAGRAM` at ~30 players and every snapshot
	/// stopped being sent.
	#[test]
	fn a_full_snapshot_fits_one_datagram() {
		use crate::protocol::{MAX_DATAGRAM, MAX_SNAPSHOT_PLAYERS};
		let snap = Snapshot {
			server_tick: u32::MAX,
			your_last_input_tick: u32::MAX,
			me: Some(SelfState {
				pos: [12345.6; 3],
				vel: [-9.5; 3],
			}),
			others: (0..MAX_SNAPSHOT_PLAYERS - 1)
				.map(|i| RemoteState {
					id: i as u8,
					pos: [12345.6; 3],
					vel: [i16::MIN, i16::MAX, -1],
					yaw: u16::MAX,
					// 3-byte varint: the widest `held` there is.
					held: u16::MAX,
				})
				.collect(),
		};
		let n = crate::codec::encode_datagram(&Datagram::Snapshot(snap))
			.expect("a full snapshot encodes")
			.len();
		assert!(
			n <= MAX_DATAGRAM,
			"{MAX_SNAPSHOT_PLAYERS} players encode to {n} bytes, over MAX_DATAGRAM {MAX_DATAGRAM}"
		);
	}

	/// The wire yaw must survive a round trip to within its own resolution
	/// (360° / 65536), including at the wrap and for a straight-down look,
	/// where `atan2(0, 0)` has no meaningful answer but must not be NaN.
	#[test]
	fn yaw_round_trips_within_a_step() {
		let step = std::f32::consts::TAU / 65536.0;
		for (name, look) in [
			("+z", [0.0f32, 0.0, 1.0]),
			("-z", [0.0, 0.0, -1.0]),
			("+x", [1.0, 0.0, 0.0]),
			("-x", [-1.0, 0.0, 0.0]),
			("diagonal", [0.707, 0.0, 0.707]),
			("just past the wrap", [-0.0001, 0.0, -1.0]),
		] {
			let want = look[0].atan2(look[2]).rem_euclid(std::f32::consts::TAU);
			let got = yaw_from_wire(yaw_to_wire(look));
			// Compare on the circle: 0 and 2π are the same heading.
			let d = (got - want).abs();
			let d = d.min(std::f32::consts::TAU - d);
			assert!(d <= step, "{name}: {got} vs {want} (> one step)");
		}
		// Straight down has no yaw; it must still be finite.
		assert!(yaw_from_wire(yaw_to_wire([0.0, -1.0, 0.0])).is_finite());
	}

	/// Yaw interpolation takes the SHORT way round. Lerping decoded
	/// radians instead would spin a player crossing the wrap almost all
	/// the way back the other way.
	#[test]
	fn yaw_lerp_takes_the_short_arc() {
		// A quarter turn either side of the wrap: 15/16 → 1/16 is an eighth
		// of a turn forwards, not seven eighths backwards.
		let a = (0.9375 * 65536.0) as u16;
		let b = (0.0625 * 65536.0) as u16;
		let mid = yaw_lerp(a, b, 0.5);
		assert_eq!(mid, 0, "halfway is exactly north, not south");
		assert_eq!(yaw_lerp(a, b, 0.0), a);
		assert_eq!(yaw_lerp(a, b, 1.0), b);
		// And the same arc walked backwards.
		assert_eq!(yaw_lerp(b, a, 0.5), 0);
		// An ordinary interior span is a plain lerp.
		assert_eq!(yaw_lerp(1000, 3000, 0.5), 2000);
	}

	/// Velocity quantization keeps a millimetre/s and clamps at the
	/// protocol's speed ceiling rather than wrapping into `i16`.
	#[test]
	fn velocity_quantization_is_lossless_enough_and_clamps() {
		for v in [0.0f32, 0.5, -0.5, 4.0, -20.0, 19.999] {
			let back = vel_from_wire(vel_to_wire(v));
			assert!((back - v).abs() <= 0.001, "{v} → {back}");
		}
		let m = crate::protocol::MAX_SPEED;
		assert_eq!(vel_from_wire(vel_to_wire(1e9)), m);
		assert_eq!(vel_from_wire(vel_to_wire(-1e9)), -m);
		// The ceiling has to fit i16 with room to spare, or the clamp
		// would wrap into a velocity pointing the other way.
		assert!((m * 1000.0) < i16::MAX as f32);
	}

	/// Golden bytes: an accidental reorder of fields or enum variants
	/// changes this encoding. If this test fails, the wire format broke —
	/// bump PROTOCOL_VERSION or restore append-only order.
	#[test]
	fn golden_edit_applied_encoding() {
		let m = ServerMsg::EditApplied {
			seq: 5,
			tick: 300,
			by: 2,
			client_seq: 1,
			edit: BlockEdit {
				x: 10,
				y: 130,
				z: -4,
				block: 4,
			},
			prev: 2,
		};
		let bytes = postcard::to_stdvec(&m).unwrap();
		assert_eq!(
			bytes,
			// variant 4, seq 5, tick 300 (varint), by 2, client_seq 1,
			// zigzag varints 10/130/-4, block 4, prev 2
			vec![4, 5, 172, 2, 2, 1, 20, 132, 2, 7, 4, 2]
		);
	}

	/// Golden bytes for the three messages protocol evolution actually
	/// endangers: Hello, Welcome (the append-only handshake pair) and
	/// Snapshot (the hot datagram). A failure means the wire encoding
	/// moved — bump PROTOCOL_VERSION or restore append-only order.
	#[test]
	fn golden_handshake_and_snapshot_encoding() {
		let hello = ClientMsg::Hello {
			protocol: 2,
			name: "ab".into(),
			draw_dist: 4,
			token: [1; 16],
			sim_version: 1,
			build: "b".into(),
			identity: [3; 16],
		};
		assert_eq!(
			postcard::to_stdvec(&hello).unwrap(),
			// variant 0, protocol 2, name "ab", draw_dist zigzag 8,
			// 16 raw token bytes, sim_version 1, build "b",
			// 16 raw identity bytes (appended in v4)
			vec![
				0, 2, 2, 97, 98, 8, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 98, 3, 3,
				3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3
			]
		);

		let welcome = ServerMsg::Welcome {
			protocol: 2,
			seed: 2.0,
			player_id: 3,
			spawn: [0.0, 0.0, 0.0],
			server_tick: 9,
			snapshot_divisor: 3,
			session_token: [2; 16],
			sim_version: 1,
			build: "b".into(),
			tod: 80.0,
			spawn_look: [0.0, 0.0, 0.0],
		};
		assert_eq!(
			postcard::to_stdvec(&welcome).unwrap(),
			// variant 1, protocol 2, seed 2.0 le f64, id 3, spawn zeros,
			// tick 9, divisor 3, 16 raw token bytes, sim_version 1, "b",
			// tod 80.0 le f32 (appended in v7), spawn_look zeros
			// (appended in v13)
			vec![
				1, 2, 0, 0, 0, 0, 0, 0, 0, 64, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 3, 2, 2,
				2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 98, 0, 0, 160, 66, 0, 0, 0, 0, 0,
				0, 0, 0, 0, 0, 0, 0
			]
		);

		let snap = Datagram::Snapshot(Snapshot {
			server_tick: 6,
			your_last_input_tick: 5,
			me: Some(SelfState {
				pos: [1.0, 0.0, 0.0],
				vel: [0.0, 0.0, 0.0],
			}),
			others: vec![RemoteState {
				id: 1,
				pos: [1.0, 0.0, 0.0],
				vel: [0, 0, 0],
				yaw: 0,
				held: 4,
			}],
		});
		assert_eq!(
			postcard::to_stdvec(&snap).unwrap(),
			// v14 shape, 47 bytes against v13's 41 for a snapshot carrying
			// BOTH your own state and one remote (v13 sent 40 per player
			// and no separate self entry, so its two-player equivalent was
			// 84). variant 1, tick 6, acked 5; me = Some tag 1 + pos
			// (x 1.0 le f32, zeros) + vel zeros; then vec len 1, and the
			// remote: id 1, pos (x 1.0, zeros), three zigzag i16 vels,
			// yaw 0, held 4.
			vec![
				1, 6, 5, 1, 0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
				0, 1, 1, 0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4
			]
		);
	}

	/// Golden bytes for the v4 additions: the dig-hold announcement and
	/// the inventory snapshot. Same contract as the goldens above.
	#[test]
	fn golden_dig_and_inventory_encoding() {
		let dig = ClientMsg::DigStart { cell: [1, 138, -3] };
		assert_eq!(
			postcard::to_stdvec(&dig).unwrap(),
			// variant 3, zigzag varints 1/138/-3
			vec![3, 2, 148, 2, 5]
		);
		assert_eq!(postcard::to_stdvec(&ClientMsg::DigCancel).unwrap(), vec![4]);

		let inv = ServerMsg::InventoryFull {
			counts: vec![(2, 3), (5, 1)],
		};
		assert_eq!(
			postcard::to_stdvec(&inv).unwrap(),
			// variant 8, seq len 2, pairs (2,3) (5,1) as varints
			vec![8, 2, 2, 3, 5, 1]
		);
	}

	/// Golden bytes for the v5 chat pair. Same contract as the goldens
	/// above: a failure means the wire encoding moved.
	#[test]
	fn golden_chat_encoding() {
		let c = ClientMsg::Chat { text: "hi".into() };
		assert_eq!(
			postcard::to_stdvec(&c).unwrap(),
			// variant 5, len 2, "hi"
			vec![5, 2, 104, 105]
		);

		let s = ServerMsg::Chat {
			from: 4,
			text: "hi".into(),
		};
		assert_eq!(
			postcard::to_stdvec(&s).unwrap(),
			// variant 9, from 4, len 2, "hi"
			vec![9, 4, 2, 104, 105]
		);
	}

	/// Golden bytes for the v6 return-to-spawn pair. Same contract as the
	/// goldens above: a failure means the wire encoding moved.
	#[test]
	fn golden_return_spawn_encoding() {
		assert_eq!(
			postcard::to_stdvec(&ClientMsg::ReturnToSpawn).unwrap(),
			// variant 6, no fields
			vec![6]
		);

		let t = ServerMsg::Teleported {
			pos: [1.0, 0.0, 0.0],
			look: [0.0, 0.0, 1.0],
		};
		assert_eq!(
			postcard::to_stdvec(&t).unwrap(),
			// variant 10, pos x 1.0 le f32, zeros, look z 1.0 le f32
			// (look appended in v9)
			vec![10, 0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 63]
		);
	}

	/// Golden bytes for the v10 additions: the Layout pair, the WrongSlot
	/// rejection, and the appended `EditRequest.slot`/`held`/`active_slot`
	/// fields (the latter two pinned by the handshake/snapshot goldens).
	#[test]
	fn golden_layout_and_wrong_slot_encoding() {
		let c = ClientMsg::Layout {
			hotbar: vec![2],
			grid: vec![7],
		};
		assert_eq!(
			postcard::to_stdvec(&c).unwrap(),
			// variant 9, hotbar len 1 id 2, grid len 1 id 7
			vec![9, 1, 2, 1, 7]
		);
		let s = ServerMsg::Layout {
			hotbar: vec![2],
			grid: vec![7],
		};
		assert_eq!(
			postcard::to_stdvec(&s).unwrap(),
			// variant 13, hotbar len 1 id 2, grid len 1 id 7
			vec![13, 1, 2, 1, 7]
		);
		let r = ServerMsg::EditRejected {
			client_seq: 1,
			reason: RejectReason::WrongSlot,
		};
		assert_eq!(
			postcard::to_stdvec(&r).unwrap(),
			// variant 5, seq 1, reason 9
			vec![5, 1, 9]
		);
	}

	/// Golden bytes for the v9 admin-command advertisement. Same contract
	/// as the goldens above.
	#[test]
	fn golden_admin_commands_encoding() {
		let m = ServerMsg::AdminCommands {
			commands: vec![("/x".into(), "y".into())],
		};
		assert_eq!(
			postcard::to_stdvec(&m).unwrap(),
			// variant 12, len 1, "/x", "y"
			vec![12, 1, 2, 47, 120, 1, 121]
		);
	}

	/// Golden bytes for the v8 additions: `EditKind::Interact` (kind
	/// discriminant 3) and `ClientMsg::Alchemy` (variant 8). Existing
	/// goldens above prove the append changed nothing before it.
	#[test]
	fn golden_interact_and_alchemy_encoding() {
		let m = ClientMsg::EditRequest {
			seq: 1,
			tick: 2,
			origin: [0.0, 0.0, 0.0],
			dir: [0.0, 0.0, 0.0],
			kind: EditKind::Interact,
			cell: [0, 0, 0],
			normal: [0, 0, 0],
			slot: 0,
		};
		assert_eq!(
			postcard::to_stdvec(&m).unwrap(),
			// variant 1, seq 1, tick 2, six zero f32s, KIND 3, zero cell
			// zigzags, zero normals, slot 0 (appended in v10)
			vec![
				1, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3,
				0, 0, 0, 0, 0, 0, 0
			]
		);

		let c = ClientMsg::Alchemy {
			op: AlchemyOp::Convert {
				block: 6,
				count: 10,
			},
		};
		assert_eq!(
			postcard::to_stdvec(&c).unwrap(),
			// variant 8, op variant 0, block 6, count 10
			vec![8, 0, 6, 10]
		);
		let k = ClientMsg::Alchemy {
			op: AlchemyOp::Craft {
				item: 24,
				count: 999,
			},
		};
		assert_eq!(
			postcard::to_stdvec(&k).unwrap(),
			// variant 8, op variant 1, item 24, count 999 (varint e7 07)
			vec![8, 1, 24, 231, 7]
		);

		// Flagged sub-material ids ride InventoryFull as plain varints.
		let inv = ServerMsg::InventoryFull {
			counts: vec![(0x8000, 1)],
		};
		assert_eq!(
			postcard::to_stdvec(&inv).unwrap(),
			// variant 8, len 1, id 0x8000 (varint 80 80 02), count 1
			vec![8, 1, 128, 128, 2, 1]
		);
	}
}
