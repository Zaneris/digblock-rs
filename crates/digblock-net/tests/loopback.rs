//! End-to-end NetClient behavior against a scripted server stub over the
//! in-memory loopback transport — no sockets, no timers.

use digblock_core::block::BlockType;
use digblock_core::player::{Player, PlayerIntent};
use digblock_net::codec::{self, FrameReader};
use digblock_net::loopback::{loopback_pair, LoopbackEnd};
use digblock_net::messages::{
	BlockEdit, ClientMsg, Datagram, NetPlayerState, RejectReason, ServerMsg, Snapshot,
};
use digblock_net::protocol::{CLIENT_TICK_LEAD, PROTOCOL_VERSION};
use digblock_net::transport::{NetTransport, TransportEvent};
use digblock_net::{NetClient, NetEvent};
use glam::{Vec2, Vec3};

fn flat_world(_x: i32, y: i32, _z: i32) -> Option<BlockType> {
	Some(if y < 10 {
		BlockType::Dirt
	} else {
		BlockType::Air
	})
}

fn new_client(end: LoopbackEnd) -> NetClient {
	NetClient::new(
		Box::new(end),
		digblock_net::client::NetClientConfig {
			name: "test".into(),
			draw_dist: 8,
		},
	)
}

/// Minimal scripted server: drains its loopback end, decodes, remembers.
struct Stub {
	end: LoopbackEnd,
	frames: FrameReader,
	pub client_msgs: Vec<ClientMsg>,
	pub input_ticks: std::collections::BTreeSet<u32>,
	scratch: Vec<TransportEvent>,
}

impl Stub {
	fn new(end: LoopbackEnd) -> Self {
		Self {
			end,
			frames: FrameReader::default(),
			client_msgs: Vec::new(),
			input_ticks: std::collections::BTreeSet::new(),
			scratch: Vec::new(),
		}
	}

	fn pump(&mut self) {
		self.scratch.clear();
		let mut events = std::mem::take(&mut self.scratch);
		self.end.poll(&mut events);
		for ev in events.drain(..) {
			match ev {
				TransportEvent::Reliable(bytes) => {
					self.frames.extend(&bytes);
					while let Some(frame) = self.frames.next_frame().unwrap() {
						self.client_msgs.push(codec::decode(&frame).unwrap());
					}
				}
				TransportEvent::Datagram(bytes) => {
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

	fn send(&mut self, msg: &ServerMsg) {
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
fn full_frame_handshake_edits_snapshot() {
	let (a, b) = loopback_pair();
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();

	// Client connects and sends Hello.
	client.poll(&mut events);
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
	});
	events.clear();
	client.poll(&mut events);
	assert!(matches!(
		events.first(),
		Some(NetEvent::Welcome { seed, player_id: 1, .. }) if *seed == 42.0
	));
	assert!(client.ready());
	assert_eq!(client.predictor.current_tick(), 600 + CLIENT_TICK_LEAD);

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
			block: BlockType::Air as u8,
		},
		prev: BlockType::Dirt as u8,
	});
	events.clear();
	client.poll(&mut events);
	assert!(matches!(
		events.first(),
		Some(NetEvent::Edit {
			cell: (3, 9, 4),
			block: BlockType::Air
		})
	));

	// Snapshot: our ack + one remote player.
	let me = player.state();
	server.send_snapshot(Snapshot {
		server_tick: 606,
		your_last_input_tick: first,
		players: vec![
			NetPlayerState {
				id: 1,
				pos: me.pos.to_array(),
				vel: me.vel.to_array(),
				look: [0.0, 0.0, 1.0],
			},
			NetPlayerState {
				id: 2,
				pos: [10.0, 12.0, 10.0],
				vel: [0.0, 0.0, 0.0],
				look: [1.0, 0.0, 0.0],
			},
		],
	});
	events.clear();
	client.poll(&mut events);
	assert!(matches!(
		events.first(),
		Some(NetEvent::Ack { acked_input_tick, .. }) if *acked_input_tick == first
	));
	assert!(!client.remotes.is_empty());

	// Predicted dig, then rejection → revert event with the right prev.
	let seq = client
		.edits
		.begin((5, 9, 5), BlockType::Dirt, BlockType::Air, first + 3);
	server.send(&ServerMsg::EditRejected {
		client_seq: seq,
		reason: RejectReason::OutOfReach,
	});
	events.clear();
	client.poll(&mut events);
	match events.first() {
		Some(NetEvent::EditRejected(e)) => {
			assert_eq!(e.cell, (5, 9, 5));
			assert_eq!(e.prev, BlockType::Dirt);
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
	client.poll(&mut events);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
	});
	client.poll(&mut events);

	// We predict a dig; another player's place lands on the cell first.
	let seq = client
		.edits
		.begin((7, 9, 7), BlockType::Stone, BlockType::Air, 10);
	server.send(&ServerMsg::EditApplied {
		seq: 2,
		tick: 11,
		by: 3,
		client_seq: 9,
		edit: BlockEdit {
			x: 7,
			y: 9,
			z: 7,
			block: BlockType::Sand as u8,
		},
		prev: BlockType::Stone as u8,
	});
	server.send(&ServerMsg::EditRejected {
		client_seq: seq,
		reason: RejectReason::Occluded,
	});
	events.clear();
	client.poll(&mut events);
	// One Edit event (the authoritative sand), NO EditRejected revert.
	assert!(matches!(
		events.iter().find(|e| matches!(e, NetEvent::Edit { .. })),
		Some(NetEvent::Edit {
			cell: (7, 9, 7),
			block: BlockType::Sand
		})
	));
	assert!(!events
		.iter()
		.any(|e| matches!(e, NetEvent::EditRejected(_))));
}

#[test]
fn input_redundancy_survives_datagram_loss() {
	let (mut a, b) = loopback_pair();
	a.set_datagram_policy(Some(2), 0); // drop every 2nd outgoing datagram
	let mut client = new_client(a);
	let mut server = Stub::new(b);
	let mut events = Vec::new();
	client.poll(&mut events);
	server.send(&ServerMsg::Welcome {
		protocol: PROTOCOL_VERSION,
		seed: 1.0,
		player_id: 1,
		spawn: [0.5, 11.5, 0.5],
		server_tick: 0,
		snapshot_divisor: 3,
	});
	client.poll(&mut events);

	let mut player = Player::new(Vec3::new(0.5, 11.5, 0.5));
	for _ in 0..20 {
		client
			.predictor
			.predict(&mut player, PlayerIntent::default(), Vec3::Z, &flat_world);
		client.send_inputs();
	}
	// The final tick only rode the last datagram; in live operation the
	// next frame's datagram re-carries it. Model that follow-up send.
	client.send_inputs();
	server.pump();
	// Half the datagrams were dropped, but the 4-deep redundancy window
	// delivered every tick.
	let first = CLIENT_TICK_LEAD;
	for t in first..first + 20 {
		assert!(server.input_ticks.contains(&t), "missing tick {t}");
	}
}
