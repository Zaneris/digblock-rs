//! End-to-end NetClient behavior against a scripted server stub over the
//! in-memory loopback transport — no sockets, no timers.

use digblock_core::block::BlockType;
use digblock_core::player::{Player, PlayerIntent};
use digblock_net::codec::{self, FrameReader};
use digblock_net::loopback::{LoopbackEnd, loopback_pair};
use digblock_net::messages::{
	BlockEdit, ClientMsg, Datagram, RejectReason, RemoteState, SelfState, ServerMsg, Snapshot,
};
use digblock_net::protocol::{
	CLIENT_TICK_LEAD, HANDSHAKE_TIMEOUT_SECS, PREAMBLE_LEN, PROTOCOL_VERSION, RECV_TIMEOUT_SECS,
	decode_preamble, encode_preamble,
};
use digblock_net::transport::{NetTransport, TransportEvent};
use digblock_net::{NetClient, NetEvent};
use glam::{Vec2, Vec3};
use std::assert_matches;

fn flat_world(_x: i32, y: i32, _z: i32) -> Option<u16> {
	Some(if y < 10 {
		BlockType::Dirt as u16
	} else {
		BlockType::Air as u16
	})
}

fn new_client(end: LoopbackEnd) -> NetClient {
	NetClient::new(
		Box::new(end),
		digblock_net::client::NetClientConfig {
			name: "test".into(),
			draw_dist: 8,
			token: [0; 16],
			identity: [0; 16],
		},
	)
}

/// Minimal scripted server: drains its loopback end, decodes, remembers.
/// Speaks the fixed preamble in both directions like the real server.
struct Stub {
	end: LoopbackEnd,
	frames: FrameReader,
	pub client_msgs: Vec<ClientMsg>,
	pub client_protocol: Option<u16>,
	pub input_ticks: std::collections::BTreeSet<u32>,
	pub datagrams: u32,
	scratch: Vec<TransportEvent>,
	sent_preamble: bool,
	/// Version the stub advertises in its own preamble.
	advertise: u16,
}

impl Stub {
	fn new(end: LoopbackEnd) -> Self {
		Self {
			end,
			frames: FrameReader::default(),
			client_msgs: Vec::new(),
			client_protocol: None,
			input_ticks: std::collections::BTreeSet::new(),
			datagrams: 0,
			scratch: Vec::new(),
			sent_preamble: false,
			advertise: PROTOCOL_VERSION,
		}
	}

	fn pump(&mut self) {
		self.scratch.clear();
		let mut events = std::mem::take(&mut self.scratch);
		self.end.poll(&mut events);
		for ev in events.drain(..) {
			match ev {
				TransportEvent::Reliable(mut bytes) => {
					if self.client_protocol.is_none() {
						assert!(bytes.len() >= PREAMBLE_LEN, "preamble arrives whole");
						let pre: [u8; PREAMBLE_LEN] = bytes[..PREAMBLE_LEN].try_into().unwrap();
						self.client_protocol =
							Some(decode_preamble(&pre).expect("client preamble magic"));
						bytes.drain(..PREAMBLE_LEN);
					}
					self.frames.extend(&bytes);
					while let Some(frame) = self.frames.next_frame().unwrap() {
						self.client_msgs.push(codec::decode(&frame).unwrap());
					}
				}
				TransportEvent::Datagram(bytes) => {
					self.datagrams += 1;
					if let Datagram::Input(batch) = codec::decode_datagram(&bytes).unwrap() {
						for i in batch.inputs {
							self.input_ticks.insert(i.tick);
						}
					}
				}
				_ => {}
			}
		}
		self.scratch = events;
	}

	fn send_preamble_if_needed(&mut self) {
		if !self.sent_preamble {
			self.sent_preamble = true;
			let pre = encode_preamble(self.advertise);
			self.end.send_reliable(&pre);
		}
	}

	fn send(&mut self, msg: &ServerMsg) {
		self.send_preamble_if_needed();
		let mut buf = Vec::new();
		codec::write_frame(msg, &mut buf).unwrap();
		self.end.send_reliable(&buf);
	}

	/// The underlying end, for transport-level faults (close, silence).
	fn end_mut(&mut self) -> &mut LoopbackEnd {
		&mut self.end
	}

	fn send_snapshot(&mut self, snap: Snapshot) {
		let bytes = codec::encode_datagram(&Datagram::Snapshot(snap)).unwrap();
		self.end.send_datagram(&bytes);
	}
}

#[test]
fn configured_token_and_identity_reach_the_wire() {
	let (a, b) = loopback_pair();
	let token: [u8; 16] = [7; 16];
	let identity: [u8; 16] = [9; 16];
	let mut client = NetClient::new(
		Box::new(a),
		digblock_net::client::NetClientConfig {
			name: "test".into(),
			draw_dist: 8,
			token,
			identity,
		},
	);
	let mut server = Stub::new(b);
	let mut events = Vec::new();

	client.poll(&mut events, 0.016);
	server.pump();
	assert_matches!(
		server.client_msgs.first(),
		Some(ClientMsg::Hello { token: t, identity: i, .. }) if *t == token && *i == identity
	);
}

#[test]
fn full_frame_handshake_edits_snapshot() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();

	// Client connects and sends Hello.
	client.poll(&mut events, 0.016);
	server.pump();
	assert_matches!(
		server.client_msgs.first(),
		Some(ClientMsg::Hello { protocol, .. }) if *protocol == PROTOCOL_VERSION
	);

	// The server welcomes from a tick well past zero.
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 42.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 600,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 45.0,
		spawn_look: [0.0; 3],
	});
	events.clear();
	client.poll(&mut events, 0.016);
	assert_matches!(
		events.first(),
		Some(NetEvent::Welcome { seed, player_id: 1, tod, .. })
			if *seed == 42.0 && *tod == 45.0
	);
	assert!(client.ready());
	assert_eq!(client.predictor.current_tick(), 600 + CLIENT_TICK_LEAD);

	// The world-clock heartbeat passes through as an event.
	server.send(&ServerMsg::TimeOfDay { tod: 44.5 });
	events.clear();
	client.poll(&mut events, 0.016);
	assert_matches!(
		events.first(),
		Some(NetEvent::TimeOfDay { tod }) if *tod == 44.5
	);

	// Predict a few ticks and send inputs.
	let mut player = Player::new(Vec3::new(0.5, 11.5, 0.5));
	let intent = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..3 {
		client
			.predictor
			.predict(&mut player, intent, Vec3::Z, &flat_world);
	}
	client.send_inputs();
	server.pump();
	let first = 600 + CLIENT_TICK_LEAD;
	assert!(server.input_ticks.contains(&first));
	assert!(server.input_ticks.contains(&(first + 2)));

	// A foreign edit broadcast becomes an Edit event.
	server.send(&ServerMsg::EditApplied {
		seq: 1,
		tick: 610,
		by: 2,
		client_seq: 0,
		edit: BlockEdit {
			x: 3,
			y: 9,
			z: 4,
			block: BlockType::Air as u16,
		},
		prev: BlockType::Dirt as u16,
	});
	events.clear();
	client.poll(&mut events, 0.016);
	assert_matches!(
		events.first(),
		Some(NetEvent::Edit {
			cell: (3, 9, 4),
			cell_value: 0, // Air
			..
		})
	);

	// Snapshot: our ack + one remote player.
	let me = player.state();
	server.send_snapshot(Snapshot {
		server_tick: 606,
		your_last_input_tick: first,
		me: Some(SelfState {
			pos: me.pos.to_array(),
			vel: me.vel.to_array(),
		}),
		others: vec![RemoteState {
			id: 2,
			pos: [10.0, 12.0, 10.0],
			vel: [0, 0, 0],
			yaw: digblock_net::messages::yaw_to_wire([1.0, 0.0, 0.0]),
			held: BlockType::Dirt as u16,
		}],
	});
	events.clear();
	client.poll(&mut events, 0.016);
	assert_matches!(
		events.first(),
		Some(NetEvent::Ack { acked_input_tick, .. }) if *acked_input_tick == first
	);
	assert!(!client.remotes.is_empty());

	// Predicted dig, then rejection → revert event with the right prev
	// and the predicted inventory credit to unwind.
	let seq = client.edits.begin(
		(5, 9, 5),
		BlockType::Dirt as u16,
		BlockType::Air as u16,
		first + 3,
		Some((BlockType::Dirt, 1)),
	);
	server.send(&ServerMsg::EditRejected {
		client_seq: seq,
		reason: RejectReason::OutOfReach,
	});
	events.clear();
	client.poll(&mut events, 0.016);
	match events.first() {
		Some(NetEvent::EditRejected { edit: e, .. }) => {
			assert_eq!(e.cell, (5, 9, 5));
			assert_eq!(e.prev, BlockType::Dirt as u16);
			assert_eq!(e.inv_delta, Some((BlockType::Dirt, 1)));
		}
		other => panic!("expected EditRejected, got {other:?}"),
	}
}

#[test]
fn authoritative_broadcast_cancels_pending_revert() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	client.poll(&mut events, 0.016);

	// We predict a dig; another player's place lands on the cell first.
	let seq = client.edits.begin(
		(7, 9, 7),
		BlockType::Stone as u16,
		BlockType::Air as u16,
		10,
		Some((BlockType::Stone, 1)),
	);
	server.send(&ServerMsg::EditApplied {
		seq: 2,
		tick: 11,
		by: 3,
		client_seq: 9,
		edit: BlockEdit {
			x: 7,
			y: 9,
			z: 7,
			block: BlockType::Sand as u16,
		},
		prev: BlockType::Stone as u16,
	});
	server.send(&ServerMsg::EditRejected {
		client_seq: seq,
		reason: RejectReason::Occluded,
	});
	events.clear();
	client.poll(&mut events, 0.016);
	// One Edit event (the authoritative sand) carrying our superseded
	// prediction so its inventory credit can be unwound; NO EditRejected
	// cell revert.
	match events.iter().find(|e| matches!(e, NetEvent::Edit { .. })) {
		Some(NetEvent::Edit {
			cell: (7, 9, 7),
			cell_value,
			superseded,
			..
		}) => {
			assert_eq!(*cell_value, BlockType::Sand as u16);
			assert_eq!(superseded.len(), 1);
			assert_eq!(superseded[0].inv_delta, Some((BlockType::Stone, 1)));
		}
		other => panic!("expected the authoritative Edit, got {other:?}"),
	}
	assert!(
		!events
			.iter()
			.any(|e| matches!(e, NetEvent::EditRejected { .. }))
	);
}

#[test]
fn inventory_full_maps_to_event() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	server.send(&ServerMsg::InventoryFull {
		counts: vec![(BlockType::Dirt as u16, 5), (9999, 3)],
	});
	events.clear();
	client.poll(&mut events, 0.016);
	match events
		.iter()
		.find(|e| matches!(e, NetEvent::Inventory { .. }))
	{
		Some(NetEvent::Inventory { counts, .. }) => {
			// Raw pairs pass through; the engine's Inventory::from_wire
			// does the strict decode.
			assert_eq!(counts, &vec![(BlockType::Dirt as u16, 5), (9999, 3)]);
		}
		other => panic!("expected Inventory, got {other:?}"),
	}
}

/// The rebase has to sum the deltas that were in flight when the COUNTS
/// were decoded, not whatever is pending when the engine gets around to
/// dispatching the event. Those are different moments: `confirm` runs
/// inside the same `poll`, before any event is handed out, and the
/// engine's backlog can park an `Inventory` behind hundreds of edits.
///
/// Here one `InventoryFull` and the confirmation of a pending place arrive
/// in the SAME poll. Reading the live set afterwards would find the entry
/// already dropped by `confirm` and rebase to a stale count.
#[test]
fn inventory_rebase_uses_the_deltas_in_flight_at_decode_time() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	events.clear();
	client.poll(&mut events, 0.016);

	// Predict a place: one Dirt debited locally.
	let cell = (1, 10, 1);
	let seq = client.edits.begin(
		cell,
		BlockType::Air as u16,
		BlockType::Dirt as u16,
		0,
		Some((BlockType::Dirt, -1)),
	);

	// Both land in one poll: the server's counts predate the place, and its
	// confirmation follows immediately after.
	server.send(&ServerMsg::InventoryFull {
		counts: vec![(BlockType::Dirt as u16, 5)],
	});
	server.send(&ServerMsg::EditApplied {
		seq: 1,
		tick: 1,
		by: 1,
		client_seq: seq,
		edit: BlockEdit {
			x: cell.0,
			y: cell.1,
			z: cell.2,
			block: BlockType::Dirt as u16,
		},
		prev: BlockType::Air as u16,
	});
	events.clear();
	client.poll(&mut events, 0.016);

	let pending = events
		.iter()
		.find_map(|e| match e {
			NetEvent::Inventory { pending, .. } => Some(pending.clone()),
			_ => None,
		})
		.expect("Inventory event");
	assert_eq!(
		pending,
		vec![(BlockType::Dirt, -1)],
		"the in-flight debit rode along with the counts it must be summed against"
	);
	assert!(
		client.edits.is_empty(),
		"and the confirm did land in this same poll — otherwise the test proves nothing"
	);
}

#[test]
fn teleport_and_admin_commands_map_to_events() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	server.send(&ServerMsg::Teleported {
		pos: [100.5, 60.5, -40.5],
		look: [0.0, 0.0, -1.0],
	});
	server.send(&ServerMsg::AdminCommands {
		commands: vec![("/tp <x> <z>".into(), "teleport".into())],
	});
	events.clear();
	client.poll(&mut events, 0.016);
	match events
		.iter()
		.find(|e| matches!(e, NetEvent::Teleported { .. }))
	{
		Some(NetEvent::Teleported { pos, look }) => {
			assert_eq!(pos.to_array(), [100.5, 60.5, -40.5]);
			assert_eq!(look.to_array(), [0.0, 0.0, -1.0]);
		}
		other => panic!("expected Teleported, got {other:?}"),
	}
	match events
		.iter()
		.find(|e| matches!(e, NetEvent::AdminCommands { .. }))
	{
		Some(NetEvent::AdminCommands { commands }) => {
			assert_eq!(commands[0].0, "/tp <x> <z>");
		}
		other => panic!("expected AdminCommands, got {other:?}"),
	}
}

#[test]
fn dig_start_and_cancel_reach_the_wire() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	client.send_dig_start((3, 9, -4));
	client.send_dig_cancel();
	server.pump();
	assert_matches!(
		server.client_msgs.get(1),
		Some(ClientMsg::DigStart { cell: [3, 9, -4] })
	);
	assert_matches!(server.client_msgs.get(2), Some(ClientMsg::DigCancel));
}

#[test]
fn input_redundancy_survives_datagram_loss() {
	let (mut a, b) = loopback_pair();
	a.set_datagram_policy(Some(2), 0); // drop every 2nd outgoing datagram
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	client.poll(&mut events, 0.016);

	let mut player = Player::new(Vec3::new(0.5, 11.5, 0.5));
	for _ in 0..20 {
		client
			.predictor
			.predict(&mut player, PlayerIntent::default(), Vec3::Z, &flat_world);
		client.send_inputs();
	}
	// The final ticks only rode the last (possibly dropped) datagram; in
	// live operation the next strides' sends re-carry them. Model that.
	for _ in 0..2 {
		client
			.predictor
			.predict(&mut player, PlayerIntent::default(), Vec3::Z, &flat_world);
		client.send_inputs();
	}
	server.pump();
	// Half the datagrams were dropped, but the redundancy window delivered
	// every tick.
	let first = CLIENT_TICK_LEAD;
	for t in first..first + 20 {
		assert!(server.input_ticks.contains(&t), "missing tick {t}");
	}
}

#[test]
fn handshake_timeout_disconnects() {
	let (a, _b) = loopback_pair();
	let mut client = new_client(a);
	let mut events = Vec::new();
	// Connected transport, server never answers: the deadline fires.
	for _ in 0..7 {
		client.poll(&mut events, 1.0);
	}
	assert!(
		events.iter().any(
			|e| matches!(e, NetEvent::Disconnected { reason } if reason.contains("handshake timeout"))
		),
		"{events:?}"
	);
	// And only once.
	events.clear();
	client.poll(&mut events, HANDSHAKE_TIMEOUT_SECS + 1.0);
	assert!(events.is_empty(), "{events:?}");
}

#[test]
fn incompatible_server_version_is_refused_locally() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	server.advertise = 9999;
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.pump();
	assert_eq!(server.client_protocol, Some(PROTOCOL_VERSION));
	// The stub answers with an alien version; Welcome bytes after it must
	// not be interpreted.
	server.send(&ServerMsg::Welcome {
		protocol: 9999,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	events.clear();
	client.poll(&mut events, 0.016);
	assert!(
		matches!(events.first(), Some(NetEvent::Refused { reason }) if reason.contains("9999")),
		"{events:?}"
	);
	assert!(!client.ready());
}

#[test]
fn sim_version_mismatch_is_refused() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.pump();
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION + 1,
		build: "newer".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	events.clear();
	client.poll(&mut events, 0.016);
	assert!(
		matches!(events.first(), Some(NetEvent::Refused { reason }) if reason.contains("sim")),
		"{events:?}"
	);
	assert!(!client.ready());
}

#[test]
fn input_sends_are_throttled_to_the_stride() {
	use digblock_net::protocol::INPUT_SEND_STRIDE;
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.pump();
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	client.poll(&mut events, 0.016);
	server.pump();
	server.datagrams = 0;

	// A high-refresh display: send_inputs is called TWICE per predicted tick.
	let mut player = Player::new(Vec3::new(0.5, 11.5, 0.5));
	const TICKS: u32 = 20;
	for _ in 0..TICKS {
		client
			.predictor
			.predict(&mut player, PlayerIntent::default(), Vec3::Z, &flat_world);
		client.send_inputs();
		client.send_inputs(); // same-tick duplicate frame
	}
	// The last tick rides the NEXT stride's send in live operation.
	client
		.predictor
		.predict(&mut player, PlayerIntent::default(), Vec3::Z, &flat_world);
	client.send_inputs();
	server.pump();
	// At most one datagram per stride worth of ticks...
	assert!(
		server.datagrams <= TICKS / INPUT_SEND_STRIDE + 1,
		"{} datagrams for {TICKS} ticks",
		server.datagrams
	);
	assert!(server.datagrams >= TICKS / INPUT_SEND_STRIDE - 1);
	// ...and every tick still arrived (the redundancy window covers the
	// stride with margin).
	let first = CLIENT_TICK_LEAD;
	for t in first..first + TICKS {
		assert!(server.input_ticks.contains(&t), "missing tick {t}");
	}
}

/// A join sync must not land in one frame. `JournalColumn` expands to one
/// event PER CELL, so a few hundred columns is six figures of events —
/// materialised into `out`, then into the engine's backlog, inside a
/// single rAF callback on wasm. `poll` decodes to a budget and leaves the
/// rest in the frame reader.
#[test]
fn a_large_journal_sync_is_spread_across_polls() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});

	// Many columns of many cells each, all arriving at once.
	const COLS: i32 = 40;
	const CELLS: i32 = 100;
	for cx in 0..COLS {
		server.send(&ServerMsg::JournalColumn {
			cx,
			cz: 0,
			cells: (0..CELLS)
				.map(|y| BlockEdit {
					x: cx * 16,
					y: 64 + y,
					z: 0,
					block: BlockType::Dirt as u16,
				})
				.collect(),
		});
	}

	let mut polls = 0;
	let mut edits = 0;
	loop {
		events.clear();
		client.poll(&mut events, 0.016);
		let n = events
			.iter()
			.filter(|e| matches!(e, NetEvent::Edit { .. }))
			.count();
		// The budget is per poll, and one column is decoded atomically, so
		// a poll can overshoot by at most one column's worth.
		assert!(
			events.len() <= 256 + CELLS as usize,
			"poll {polls} produced {} events — the budget is not bounding anything",
			events.len()
		);
		edits += n;
		polls += 1;
		if n == 0 {
			break;
		}
		assert!(polls < 200, "sync never drained");
	}
	assert_eq!(
		edits,
		(COLS * CELLS) as usize,
		"every edit still arrived, just spread out"
	);
	assert!(
		polls > 4,
		"a 4000-edit sync landed in {polls} polls — it was not actually paced"
	);
}

/// Welcome, then the server drops the connection. Everything queued must
/// still be delivered, and the disconnect must surface exactly once.
///
/// Until the loopback transport learned to close, NO in-process test could
/// reach this path at all — `status` was hardcoded to `Connected` and
/// `poll` never emitted `Disconnected`, so coverage stopped at the
/// pre-Welcome handshake timeout.
#[test]
fn a_post_welcome_disconnect_surfaces_once_after_queued_bytes() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	events.clear();
	client.poll(&mut events, 0.016);
	assert!(client.ready());

	// A chat line already on the wire when the connection dies.
	server.send(&ServerMsg::Chat {
		from: 2,
		text: "bye".into(),
	});
	server.end_mut().close("server closed");

	events.clear();
	client.poll(&mut events, 0.016);
	let chat_at = events
		.iter()
		.position(|e| matches!(e, NetEvent::Chat { .. }))
		.expect("the queued chat line still arrived");
	let disc_at = events
		.iter()
		.position(|e| matches!(e, NetEvent::Disconnected { .. }))
		.expect("the disconnect surfaced");
	assert!(chat_at < disc_at, "queued bytes come before the close");

	// And it does not repeat on later polls.
	for _ in 0..5 {
		events.clear();
		client.poll(&mut events, 0.016);
		assert!(
			!events
				.iter()
				.any(|e| matches!(e, NetEvent::Disconnected { .. })),
			"disconnect repeated: {events:?}"
		);
	}
}

/// A transport that goes quiet WITHOUT reporting anything — a hung peer,
/// or a native background thread that died before it could say so. Nothing
/// else can notice this, so the client's own liveness deadline must.
#[test]
fn a_silent_server_eventually_disconnects_the_client() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	events.clear();
	client.poll(&mut events, 0.016);
	assert!(client.ready());

	// Silence: no bytes, no close, no status change.
	server.end_mut().go_silent();

	// Well inside the deadline, nothing happens — a quiet moment is not a
	// dead server, and firing early would drop healthy sessions.
	let mut elapsed = 0.0f32;
	while elapsed < RECV_TIMEOUT_SECS - 1.0 {
		events.clear();
		client.poll(&mut events, 0.1);
		elapsed += 0.1;
		assert!(
			!events
				.iter()
				.any(|e| matches!(e, NetEvent::Disconnected { .. })),
			"gave up after only {elapsed:.1}s"
		);
	}
	// Past it, exactly one disconnect.
	let mut disconnects = 0;
	for _ in 0..40 {
		events.clear();
		client.poll(&mut events, 0.1);
		disconnects += events
			.iter()
			.filter(|e| matches!(e, NetEvent::Disconnected { .. }))
			.count();
	}
	assert_eq!(disconnects, 1, "one disconnect, and only one");
}

/// Traffic keeps the deadline at bay: a busy session must never trip it.
#[test]
fn ongoing_traffic_keeps_the_liveness_deadline_from_firing() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	events.clear();
	client.poll(&mut events, 0.016);

	// Several times over the deadline, with heartbeats throughout.
	let mut elapsed = 0.0f32;
	let mut since_beat = 0.0f32;
	while elapsed < RECV_TIMEOUT_SECS * 3.0 {
		if since_beat >= 5.0 {
			server.send(&ServerMsg::TimeOfDay { tod: 44.0 });
			since_beat = 0.0;
		}
		events.clear();
		client.poll(&mut events, 0.1);
		elapsed += 0.1;
		since_beat += 0.1;
		assert!(
			!events
				.iter()
				.any(|e| matches!(e, NetEvent::Disconnected { .. })),
			"a live session was dropped at {elapsed:.1}s"
		);
	}
}

/// The overlay's ping. RTT is measured on the input→ack loop rather than a
/// separate ping message: a tick is stamped when it FIRST goes on the
/// wire, and the clock stops when a snapshot acknowledges that tick. So it
/// includes the server's jitter buffer and the snapshot cadence — the
/// latency a player actually feels — which a ping/pong would exclude.
#[test]
fn rtt_is_measured_on_the_input_ack_loop() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	events.clear();
	client.poll(&mut events, 0.016);
	assert_eq!(client.stats().rtt_ms, 0.0, "no sample before any ack");

	// Predict a tick and put it on the wire — this is what stamps it.
	let mut player = Player::new(Vec3::new(0.5, 11.5, 0.5));
	let intent = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	let tick = client
		.predictor
		.predict(&mut player, intent, Vec3::Z, &flat_world);
	client.send_inputs();

	// A stretch of frames goes by before the ack comes back.
	for _ in 0..8 {
		events.clear();
		client.poll(&mut events, 0.01);
	}
	let state = player.state();
	let outcome = client
		.predictor
		.reconcile(&mut player, tick, state, &flat_world);
	client.note_reconciled(outcome);

	let rtt = client.stats().rtt_ms;
	assert!(
		(70.0..=90.0).contains(&rtt),
		"expected ~80ms round trip, got {rtt}"
	);
	assert_eq!(client.stats().rtt_last_ms, rtt, "first sample seeds both");

	// A tick answered without ever being SENT is not a round trip and must
	// not be timed — otherwise an unsent tick reads as an instant reply.
	let before = client.stats().rtt_ms;
	let tick2 = client
		.predictor
		.predict(&mut player, intent, Vec3::Z, &flat_world);
	for _ in 0..20 {
		events.clear();
		client.poll(&mut events, 0.01);
	}
	let state = player.state();
	let outcome = client
		.predictor
		.reconcile(&mut player, tick2, state, &flat_world);
	client.note_reconciled(outcome);
	assert_eq!(
		client.stats().rtt_ms,
		before,
		"an unsent tick's ack left the estimate alone"
	);

	// A STALE ack is not a round trip either, and this one is the subtle
	// case: `reconcile` returns before it assigns `last_rtt`, so the value
	// left over describes the ack BEFORE it. Sampling it re-blends a
	// measurement already in the average — which barely moves `rtt_ms`, but
	// collapses `jitter_ms`, because deviation is taken from the running
	// mean and a duplicate deviates from it by nothing. On a high-latency
	// link a large share of acks are stale, so this was most wrong exactly
	// where the overlay is most read.
	let tick3 = client
		.predictor
		.predict(&mut player, intent, Vec3::Z, &flat_world);
	for _ in 0..20 {
		events.clear();
		client.poll(&mut events, 0.01);
	}
	client.send_inputs();
	for _ in 0..20 {
		events.clear();
		client.poll(&mut events, 0.01);
	}
	let state = player.state();
	let outcome = client
		.predictor
		.reconcile(&mut player, tick3, state, &flat_world);
	client.note_reconciled(outcome);
	let (rtt_after, jitter_after) = (client.stats().rtt_ms, client.stats().jitter_ms);

	// Re-deliver the SAME acked tick: stale by definition.
	let outcome = client
		.predictor
		.reconcile(&mut player, tick3, state, &flat_world);
	assert_eq!(
		outcome,
		digblock_net::predict::Reconciled::StaleAck,
		"the re-delivery really was stale"
	);
	client.note_reconciled(outcome);
	assert_eq!(
		client.stats().rtt_ms,
		rtt_after,
		"a stale ack is not a round trip"
	);
	assert_eq!(
		client.stats().jitter_ms,
		jitter_after,
		"and above all does not deflate the jitter"
	);

	// The loopback transport measures no path, so it reports no wire RTT —
	// and that must read as ABSENT, not as a zero-latency connection. The
	// two figures are independent by construction: `rtt_ms` is the
	// input→ack loop the netcode times itself, `wire_rtt_ms` is whatever
	// the transport knows. Nothing derives one from the other.
	assert_eq!(
		client.stats().wire_rtt_ms,
		0.0,
		"loopback has no path to measure"
	);
	assert!(
		client.stats().rtt_ms > 0.0,
		"but the ack loop is still timed — the two are independent"
	);
}

/// Traffic counters roll up into a per-second rate, and the connection
/// gauges track the live state the overlay reports.
#[test]
fn traffic_counters_produce_a_per_second_rate() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events, 0.016);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
		session_token: [0; 16],
		sim_version: digblock_core::SIM_VERSION,
		build: "test".into(),
		tod: 80.0,
		spawn_look: [0.0; 3],
	});
	events.clear();
	client.poll(&mut events, 0.016);

	// A second of snapshots at the real cadence.
	for i in 0..20u32 {
		server.send_snapshot(Snapshot {
			server_tick: i * 3,
			your_last_input_tick: 0,
			me: Some(SelfState {
				pos: [0.5, 11.5, 0.5],
				vel: [0.0; 3],
			}),
			others: vec![RemoteState {
				id: 2,
				pos: [10.0, 12.0, 10.0],
				vel: [0, 0, 0],
				yaw: 0,
				held: 5,
			}],
		});
		events.clear();
		client.poll(&mut events, 0.05);
	}
	let s = client.stats();
	assert!(
		(15.0..=25.0).contains(&s.snapshots_per_sec),
		"~20 snapshots/s, got {}",
		s.snapshots_per_sec
	);
	assert!(s.down_kbps > 0.0, "inbound bytes counted");
	assert_eq!(s.remotes, 1, "the remote is being interpolated");
	assert_eq!(s.replays_per_sec, 0.0, "a clean session never replays");
}
