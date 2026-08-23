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
		ClientMsg::DigStart {
			cell: [1, 138, -3],
			slot: 8,
		},
		ClientMsg::DigCancel,
		ClientMsg::Chat { text: "hi".into() },
		ClientMsg::ReturnToSpawn,
		ClientMsg::Cast(CastKind::Waystone(2)),
		ClientMsg::Cast(CastKind::Wisp),
		ClientMsg::Cast(CastKind::Kindle {
			cell: [1, -2, 3],
			face: 5,
		}),
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
			fx: 7,
		}],
	});
	let bytes = codec::encode_datagram(&d).unwrap();
	let back = codec::decode_datagram(&bytes).unwrap();
	assert_eq!(format!("{d:?}"), format!("{back:?}"));
}

/// The client-direction frame cap has to clear every message a client legitimately sends, at its
/// widest — otherwise the server drops honest players for "stream corrupt". Each case below is
/// built at its maximum: a name at `MAX_NAME_LEN`, a chat line at `MAX_CHAT_LEN` in two-byte
/// characters, a full hotbar+grid layout of widest-varint ids.
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
		(
			// The datagram fallback's widest frame: a whole datagram's
			// worth of payload, since that is exactly what it carries.
			// This is the message that made the client cap a question
			// again — everything above it is a few hundred bytes, and
			// this one is bounded by a DIFFERENT constant.
			"wrapped datagram",
			ClientMsg::Unreliable(vec![0xff; crate::protocol::MAX_DATAGRAM]),
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

/// A full snapshot must fit one datagram at the raised `MAX_SNAPSHOT_PLAYERS`, with every field at
/// its worst-case width — the `const_assert` in protocol.rs asserts the arithmetic, this asserts
/// the actual encoder agrees with it. The cliff it guards: before interest management, snapshots
/// silently exceeded `MAX_DATAGRAM` once enough players were in range, and every snapshot stopped
/// being sent.
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
				// The widest varint a `held` can encode to.
				held: u16::MAX,
				fx: u8::MAX,
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

/// The wire yaw must survive a round trip to within one quantization
/// step, including at the wrap and for a straight-down look, where
/// `atan2(0, 0)` has no meaningful answer but must not be NaN.
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
	// Just either side of the wrap: crossing north forwards is the
	// short arc, not almost a whole turn backwards.
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
		// variant tag, seq, tick (varint), by, client_seq, then the
		// edit's x/y/z as zigzag varints, its block, and prev
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
		// variant tag, protocol, name, draw_dist (zigzag), the raw
		// token bytes, sim_version, build. The trailing raw identity
		// bytes were dropped in v23 — the one field this message has
		// ever LOST, which is why MIN_PROTOCOL moved with it rather
		// than the shape staying append-only.
		vec![
			0, 2, 2, 97, 98, 8, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 98
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
		// variant tag, protocol, seed (le f64), player id, spawn,
		// server tick, snapshot divisor, the raw token bytes,
		// sim_version, build, then tod (le f32, appended in v7) and
		// spawn_look (appended in v13)
		vec![
			1, 2, 0, 0, 0, 0, 0, 0, 0, 64, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 3, 2, 2, 2, 2,
			2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 98, 0, 0, 160, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0,
			0, 0, 0
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
			fx: 0,
		}],
	});
	assert_eq!(
		postcard::to_stdvec(&snap).unwrap(),
		// The v14 shape, carrying BOTH your own state and one remote in
		// less wire than v13 spent on the pair (v13 had no separate
		// self entry, so it sent the full shared struct twice).
		// Variant tag, server tick, acked tick; then me = Some tag +
		// pos + vel as le f32s; then the vec length and the remote:
		// id, pos, its velocity components as zigzag i16, yaw, held,
		// and the packed effect byte (appended in v32).
		vec![
			1, 6, 5, 1, 0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
			1, 1, 0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0
		]
	);
}

/// Golden bytes for the v4 additions: the dig-hold announcement and
/// the inventory snapshot. Same contract as the goldens above.
#[test]
fn golden_dig_and_inventory_encoding() {
	let dig = ClientMsg::DigStart {
		cell: [1, 138, -3],
		slot: 8,
	};
	assert_eq!(
		postcard::to_stdvec(&dig).unwrap(),
		// variant tag, the cell's x/y/z as zigzag varints, then v27's
		// declared hotbar slot
		vec![3, 2, 148, 2, 5, 8]
	);
	assert_eq!(postcard::to_stdvec(&ClientMsg::DigCancel).unwrap(), vec![4]);

	let inv = ServerMsg::InventoryFull {
		counts: vec![(2, 3), (5, 1)],
	};
	assert_eq!(
		postcard::to_stdvec(&inv).unwrap(),
		// variant tag, vec length, then each (id, count) pair as varints
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
		// variant tag, string length, then the text bytes
		vec![5, 2, 104, 105]
	);

	let s = ServerMsg::Chat {
		from: 4,
		text: "hi".into(),
	};
	assert_eq!(
		postcard::to_stdvec(&s).unwrap(),
		// variant tag, sender id, string length, then the text bytes
		vec![9, 4, 2, 104, 105]
	);
}

/// Golden bytes for the v6 return-to-spawn pair. Same contract as the
/// goldens above: a failure means the wire encoding moved.
#[test]
fn golden_return_spawn_encoding() {
	assert_eq!(
		postcard::to_stdvec(&ClientMsg::ReturnToSpawn).unwrap(),
		// the variant tag alone: no fields
		vec![6]
	);

	let t = ServerMsg::Teleported {
		pos: [1.0, 0.0, 0.0],
		look: [0.0, 0.0, 1.0],
	};
	assert_eq!(
		postcard::to_stdvec(&t).unwrap(),
		// variant tag, pos as le f32s, then look (appended in v9)
		vec![
			10, 0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 63
		]
	);
}

/// Golden bytes for the v32 cast trio. `Cast` carries a `CastKind`
/// whose variants ship ahead of their spells, so the tags are part of
/// the contract from day one — Wisp and Kindle must decode on a server
/// that refuses them, or their arrival would need a second bump.
#[test]
fn golden_cast_encoding() {
	assert_eq!(
		postcard::to_stdvec(&ClientMsg::Cast(CastKind::Waystone(2))).unwrap(),
		// variant tag, kind tag, the SurfaceKind index
		vec![13, 0, 2]
	);
	assert_eq!(
		postcard::to_stdvec(&ClientMsg::Cast(CastKind::Kindle {
			cell: [1, -2, 3],
			face: 5,
		}))
		.unwrap(),
		// variant tag, kind tag, zigzag cell, face
		vec![13, 2, 2, 3, 6, 5]
	);
	assert_eq!(
		postcard::to_stdvec(&ServerMsg::Waystones {
			entries: vec![(2, [1.0, 2.0, 3.0])]
		})
		.unwrap(),
		// variant tag, count, kind index, eye as le f32s
		vec![21, 1, 2, 0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64]
	);
	assert_eq!(
		postcard::to_stdvec(&ServerMsg::CastRefused {
			reason: CastRefusal::CantPay
		})
		.unwrap(),
		// variant tag, reason tag
		vec![22, 1]
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
		// variant tag, hotbar length then its ids, grid length then its
		// ids
		vec![9, 1, 2, 1, 7]
	);
	let s = ServerMsg::Layout {
		hotbar: vec![2],
		grid: vec![7],
	};
	assert_eq!(
		postcard::to_stdvec(&s).unwrap(),
		// the server-side variant tag, then the same two lists
		vec![13, 1, 2, 1, 7]
	);
	let r = ServerMsg::EditRejected {
		client_seq: 1,
		reason: RejectReason::WrongSlot,
	};
	assert_eq!(
		postcard::to_stdvec(&r).unwrap(),
		// variant tag, client seq, then the reason's own discriminant
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
		// variant tag, vec length, then each (usage, description) pair
		vec![12, 1, 2, 47, 120, 1, 121]
	);
}

/// Golden bytes for the v8 additions: `EditKind::Interact` (a new kind
/// discriminant) and `ClientMsg::Alchemy` (a new variant). Existing
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
		// variant tag, seq, tick, origin and dir as f32s, the KIND
		// discriminant, the cell's zigzags, the normals, then slot
		// (appended in v10)
		vec![
			1, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0,
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
		// variant tag, the op's Convert discriminant, block, count
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
		// variant tag, the op's Craft discriminant, item, then a count
		// wide enough to need a multi-byte varint
		vec![8, 1, 24, 231, 7]
	);

	// Flagged sub-material ids ride InventoryFull as plain varints.
	let inv = ServerMsg::InventoryFull {
		counts: vec![(0x8000, 1)],
	};
	assert_eq!(
		postcard::to_stdvec(&inv).unwrap(),
		// variant tag, vec length, the flagged id as a multi-byte
		// varint, then its count
		vec![8, 1, 128, 128, 2, 1]
	);
}

/// Golden bytes for the v29 admin reply. Same contract as the goldens above: a failure means the
/// wire encoding moved, and moving it needs a `PROTOCOL_VERSION` bump. The point of the variant is
/// that the ARGUMENTS cross, not a sentence — so the bytes are the arguments, and a server that
/// spelled the line out again would be visibly longer here.
#[test]
fn golden_admin_reply_encoding() {
	let s = ServerMsg::Admin(AdminReply::Teleported { x: 40, z: -20 });
	assert_eq!(
		postcard::to_stdvec(&s).unwrap(),
		// ServerMsg variant tag, AdminReply tag, then two zigzag varints
		vec![20, 6, 80, 39]
	);

	// A usage line is one byte of payload: which command, no prose.
	let u = ServerMsg::Admin(AdminReply::Usage(AdminUsage::Give));
	assert_eq!(postcard::to_stdvec(&u).unwrap(), vec![20, 0, 6]);

	// And a reason travels as a tag beside the name it is about.
	let n = ServerMsg::Admin(AdminReply::NoSuchPlayer {
		name: "ada".into(),
		why: NoPlayer::NotOnline,
	});
	assert_eq!(
		postcard::to_stdvec(&n).unwrap(),
		// tag, variant, name length, "ada", then the reason's own tag
		vec![20, 4, 3, 97, 100, 97, 2]
	);
}
