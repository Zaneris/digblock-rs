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
}

/// Newest-last, at most `INPUT_REDUNDANCY` entries.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct InputBatch {
	pub inputs: Vec<InputTick>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct NetPlayerState {
	pub id: PlayerId,
	pub pos: [f32; 3],
	pub vel: [f32; 3],
	pub look: [f32; 3],
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Snapshot {
	pub server_tick: u32,
	/// Highest input tick of YOURS the server has consumed —
	/// the reconciliation anchor (personalized per client).
	pub your_last_input_tick: u32,
	pub players: Vec<NetPlayerState>,
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
	/// hit must be a fluid source. Applies Air, credits Water. Subject to
	/// the same hold-to-dig timing gate as `Dig`.
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
			},
			ClientMsg::EditRequest {
				seq: 8,
				tick: 1240,
				origin: [1.0, 140.0, -3.0],
				dir: [0.0, -1.0, 0.0],
				kind: EditKind::Collect,
				cell: [1, 138, -3],
				normal: [0, 0, 0],
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
			players: vec![NetPlayerState {
				id: 1,
				pos: [1.0, 2.0, 3.0],
				vel: [0.0, -1.0, 0.0],
				look: [0.0, 0.0, 1.0],
			}],
		});
		let bytes = codec::encode_datagram(&d).unwrap();
		let back = codec::decode_datagram(&bytes).unwrap();
		assert_eq!(format!("{d:?}"), format!("{back:?}"));
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
		};
		assert_eq!(
			postcard::to_stdvec(&welcome).unwrap(),
			// variant 1, protocol 2, seed 2.0 le f64, id 3, spawn zeros,
			// tick 9, divisor 3, 16 raw token bytes, sim_version 1, "b",
			// tod 80.0 le f32 (appended in v7)
			vec![
				1, 2, 0, 0, 0, 0, 0, 0, 0, 64, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 3, 2, 2,
				2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 98, 0, 0, 160, 66
			]
		);

		let snap = Datagram::Snapshot(Snapshot {
			server_tick: 6,
			your_last_input_tick: 5,
			players: vec![NetPlayerState {
				id: 1,
				pos: [1.0, 0.0, 0.0],
				vel: [0.0, 0.0, 0.0],
				look: [0.0, 0.0, 1.0],
			}],
		});
		assert_eq!(
			postcard::to_stdvec(&snap).unwrap(),
			// variant 1, tick 6, acked 5, 1 player: id 1, pos x 1.0 le f32,
			// zeros, look z 1.0
			vec![
				1, 6, 5, 1, 1, 0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
				0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 63
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
		};
		assert_eq!(
			postcard::to_stdvec(&m).unwrap(),
			// variant 1, seq 1, tick 2, six zero f32s, KIND 3, zero cell
			// zigzags, zero normals
			vec![
				1, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3,
				0, 0, 0, 0, 0, 0
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
