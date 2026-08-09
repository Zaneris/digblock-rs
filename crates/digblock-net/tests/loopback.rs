//! End-to-end NetClient behavior against a scripted server stub over the
//! in-memory loopback transport — no sockets, no timers.

use digblock_core::block::BlockType;
use digblock_core::player::{Player, PlayerIntent};
use digblock_net::codec::{self, FrameReader};
use digblock_net::loopback::{loopback_pair, LoopbackEnd};
use digblock_net::messages::{
	BlockEdit, ClientMsg, Datagram, RejectReason, RemoteState, SelfState, ServerMsg, Snapshot,
};
use digblock_net::protocol::{
	decode_preamble, encode_preamble, CLIENT_TICK_LEAD, HANDSHAKE_TIMEOUT_SECS, PREAMBLE_LEN,
	PROTOCOL_VERSION,
};
use digblock_net::transport::{NetTransport, TransportEvent};
use digblock_net::{NetClient, NetEvent};
use glam::{Vec2, Vec3};

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
/// Speaks the 8-byte preamble in both directions like the real server.
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
	assert!(matches!(
		server.client_msgs.first(),
		Some(ClientMsg::Hello { token: t, identity: i, .. }) if *t == token && *i == identity
	));
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
	assert!(matches!(
		server.client_msgs.first(),
		Some(ClientMsg::Hello { protocol, .. }) if *protocol == PROTOCOL_VERSION
	));

	// Server welcomes at tick 600.
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
	assert!(matches!(
		events.first(),
		Some(NetEvent::Welcome { seed, player_id: 1, tod, .. })
			if *seed == 42.0 && *tod == 45.0
	));
	assert!(client.ready());
	assert_eq!(client.predictor.current_tick(), 600 + CLIENT_TICK_LEAD);

	// The world-clock heartbeat passes through as an event.
	server.send(&ServerMsg::TimeOfDay { tod: 44.5 });
	events.clear();
	client.poll(&mut events, 0.016);
	assert!(matches!(
		events.first(),
		Some(NetEvent::TimeOfDay { tod }) if *tod == 44.5
	));

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
	assert!(matches!(
		events.first(),
		Some(NetEvent::Edit {
			cell: (3, 9, 4),
			cell_value: 0, // Air
			..
		})
	));

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
	assert!(matches!(
		events.first(),
		Some(NetEvent::Ack { acked_input_tick, .. }) if *acked_input_tick == first
	));
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
	assert!(!events
		.iter()
		.any(|e| matches!(e, NetEvent::EditRejected { .. })));
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
	assert!(matches!(
		server.client_msgs.get(1),
		Some(ClientMsg::DigStart { cell: [3, 9, -4] })
	));
	assert!(matches!(
		server.client_msgs.get(2),
		Some(ClientMsg::DigCancel)
	));
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
	// Half the datagrams were dropped, but the 4-deep redundancy window
	// delivered every tick.
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

	// A 120 Hz display: send_inputs is called TWICE per predicted tick.
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
