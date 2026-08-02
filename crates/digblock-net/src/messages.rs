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
	Place { block: u8 },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct BlockEdit {
	pub x: i32,
	pub y: i32,
	pub z: i32,
	pub block: u8,
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
		/// Block that was replaced (dig pickup).
		prev: u8,
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
			ClientMsg::Bye,
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
			},
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
			ServerMsg::PlayerJoined {
				id: 4,
				name: "bot".into(),
			},
			ServerMsg::PlayerLeft { id: 4 },
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
		};
		assert_eq!(
			postcard::to_stdvec(&hello).unwrap(),
			// variant 0, protocol 2, name "ab", draw_dist zigzag 8,
			// 16 raw token bytes, sim_version 1, build "b"
			vec![0, 2, 2, 97, 98, 8, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 98]
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
		};
		assert_eq!(
			postcard::to_stdvec(&welcome).unwrap(),
			// variant 1, protocol 2, seed 2.0 le f64, id 3, spawn zeros,
			// tick 9, divisor 3, 16 raw token bytes, sim_version 1, "b"
			vec![
				1, 2, 0, 0, 0, 0, 0, 0, 0, 64, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 3, 2, 2,
				2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 98
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
}
