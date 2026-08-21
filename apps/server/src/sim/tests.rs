use super::harness::*;
use super::{
	AccountId, ClientMsg, MIN_PROTOCOL, OutMsg, PROTOCOL_VERSION, RejectReason, ServerMsg, SimEvent,
};
use digblock_net::codec::FrameReader;
use std::assert_matches;
use tokio::sync::mpsc::channel;

#[test]
fn welcome_anchors_tod_and_heartbeat_advances_it() {
	let mut sim = flat_sim("tod");
	let mut c = connect(&mut sim, 1);
	let welcome_tod = c
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Welcome { tod, .. } => Some(*tod),
			_ => None,
		})
		.expect("Welcome");
	// Joined near tick 0: the clock sits at the fresh-world start.
	assert!(
		(welcome_tod - digblock_core::sun::TOD_START).abs() < 0.1,
		"welcome tod {welcome_tod}"
	);
	for _ in 0..super::TOD_SYNC_TICKS {
		sim.tick_once(vec![]);
	}
	c.drain();
	let beat = c
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::TimeOfDay { tod } => Some(*tod),
			_ => None,
		})
		.expect("heartbeat within one sync period");
	// One `TOD_SYNC_TICKS` period at the sun's rate — it has moved
	// toward noon (tod falls).
	use digblock_net::protocol::TICK_RATE;
	let advance =
		360.0 / digblock_core::sun::DAY_SECS * (super::TOD_SYNC_TICKS as f32 / TICK_RATE as f32);
	assert!(
		(welcome_tod - beat - advance).abs() < 0.1,
		"welcome {welcome_tod} → beat {beat} (expected a {advance} fall)"
	);
}

#[test]
fn set_draw_dist_updates_and_clamps() {
	let mut sim = flat_sim("setdraw");
	let _c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	assert_eq!(sim.test_draw_dist(1), Some(4), "harness Hello value");

	let msg = |draw_dist| SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::SetDrawDist { draw_dist },
	};
	sim.tick_once(vec![msg(12)]);
	assert_eq!(sim.test_draw_dist(1), Some(12));
	sim.tick_once(vec![msg(1000)]);
	assert_eq!(sim.test_draw_dist(1), Some(super::DRAW_DIST_MAX));
	sim.tick_once(vec![msg(-3)]);
	assert_eq!(sim.test_draw_dist(1), Some(super::DRAW_DIST_MIN));

	// Unknown connection: silently ignored.
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 99,
		msg: ClientMsg::SetDrawDist { draw_dist: 8 },
	}]);
}

/// Growing the radius must request a sync scan. The edited-column walk
/// is event-driven off `needs_sync_scan`, which otherwise only rises
/// when the center column moves — so a player who widened their draw
/// distance standing still got no `JournalColumn` for the ring that
/// just became visible, and (because `broadcast_edit` gates on
/// `synced_columns`) no live edits there either.
#[test]
fn growing_draw_dist_syncs_the_newly_visible_ring() {
	// A pre-journaled edit in a column outside the harness Hello's
	// draw distance and inside the widened one. The client spawns in
	// the origin column and never moves, so `needs_sync_scan` can only
	// come from the SetDrawDist itself.
	let dir = temp_dir("drawgrow");
	let col = (6, 0);
	{
		let mut j = crate::journal::Journal::open(&dir, SEED).unwrap();
		j.append(
			digblock_net::messages::BlockEdit {
				x: col.0 * 16 + 8,
				y: 127,
				z: 8,
				block: digblock_core::block::BlockType::Stone as u16,
			},
			0,
		);
		j.sync();
	}
	let mut sim = super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	for _ in 0..5 {
		sim.tick_once(vec![]);
	}
	c.drain();
	let synced = |c: &TestClient| {
		c.msgs
			.iter()
			.filter(|m| matches!(m, ServerMsg::JournalColumn { cx, cz, .. } if (*cx, *cz) == col))
			.count()
	};
	assert_eq!(synced(&c), 0, "column sits outside draw_dist 4");

	c.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::SetDrawDist { draw_dist: 8 },
	}]);
	for _ in 0..5 {
		sim.tick_once(vec![]);
	}
	c.drain();
	assert_eq!(
		synced(&c),
		1,
		"widening the radius synced the column without moving: {:?}",
		c.msgs
	);
}

#[test]
fn version_mismatch_refused_and_closed() {
	let refused_with = |protocol: u16, sim_version: u16| -> (Vec<ServerMsg>, bool) {
		let mut sim = flat_sim("refuse");
		let (tx, rx) = channel(super::OUT_CAP);
		sim.tick_once(vec![
			SimEvent::Connected {
				conn_id: 9,
				out: tx,
				addr: test_addr(9),
				account: AccountId::NONE,
			},
			SimEvent::Msg {
				conn_id: 9,
				msg: ClientMsg::Hello {
					protocol,
					name: "old".into(),
					draw_dist: 4,
					token: [0; 16],
					sim_version,
					build: "old-build".into(),
				},
			},
		]);
		let mut c = TestClient {
			rx,
			frames: FrameReader::default(),
			msgs: Vec::new(),
			snaps: Vec::new(),
			dgram_snaps: 0,
		};
		let mut closed = false;
		while let Ok(out) = c.rx.try_recv() {
			match out {
				OutMsg::Frame(bytes) => {
					c.frames.extend(&bytes);
					while let Some(f) = c.frames.next_frame().unwrap() {
						c.msgs.push(digblock_net::codec::decode(&f).unwrap());
					}
				}
				OutMsg::Close => closed = true,
				OutMsg::Datagram(_) => {}
			}
		}
		(c.msgs, closed)
	};

	// Old protocol → Refused naming the supported range, then Close.
	let (msgs, closed) = refused_with(MIN_PROTOCOL - 1, digblock_core::SIM_VERSION);
	assert!(
		matches!(&msgs[..], [ServerMsg::Refused { reason }] if reason.contains("protocol")),
		"{msgs:?}"
	);
	assert!(closed);

	// Same protocol, older sim behavior → Refused naming both sims.
	let (msgs, closed) = refused_with(PROTOCOL_VERSION, digblock_core::SIM_VERSION + 1);
	assert!(
		matches!(&msgs[..], [ServerMsg::Refused { reason }] if reason.contains("sim")),
		"{msgs:?}"
	);
	assert!(closed);
}

#[test]
fn lobby_without_hello_times_out() {
	let mut sim = flat_sim("lobby-timeout");
	let (tx, mut rx) = channel(super::OUT_CAP);
	sim.tick_once(vec![SimEvent::Connected {
		conn_id: 7,
		out: tx,
		addr: test_addr(7),
		account: AccountId::NONE,
	}]);
	// One tick short of the deadline: still in the lobby, no Close.
	for _ in 0..super::HELLO_TIMEOUT_TICKS - 1 {
		sim.tick_once(vec![]);
	}
	assert_matches!(
		rx.try_recv(),
		Err(tokio::sync::mpsc::error::TryRecvError::Empty)
	);
	// Crossing it: reaped with a Close so the net task ends.
	sim.tick_once(vec![]);
	let mut closed = false;
	while let Ok(out) = rx.try_recv() {
		closed |= matches!(out, OutMsg::Close);
	}
	assert!(closed, "lobby member closed after the Hello deadline");
	// A late Hello on the reaped connection is simply ignored.
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 7,
		msg: ClientMsg::Hello {
			protocol: PROTOCOL_VERSION,
			name: "late".into(),
			draw_dist: 4,
			token: [0; 16],
			sim_version: digblock_core::SIM_VERSION,
			build: "late-build".into(),
		},
	}]);
	assert_eq!(sim.player_count(), 0);
}

#[test]
fn closed_outbound_queue_reaps_client_and_frees_id() {
	let mut sim = flat_sim("reap");
	let a = connect(&mut sim, 1); // id 0
	let mut b = connect(&mut sim, 2); // id 1
	drop(a); // client 1's receiver gone: next push fails Closed

	// Any broadcast to 1 flags it dead; the next tick reaps it.
	let _c = connect(&mut sim, 3); // join announcement hits client 1
	sim.tick_once(vec![]);
	assert_eq!(sim.player_count(), 2, "dead client reaped");
	b.drain();
	assert!(
		b.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::PlayerLeft { id: 0 })),
		"others heard the forced leave: {:?}",
		b.msgs
	);

	// Its id is reusable by a fresh join.
	let mut d = connect(&mut sim, 4);
	d.drain();
	let welcome_id = d.msgs.iter().find_map(|m| match m {
		ServerMsg::Welcome { player_id, .. } => Some(*player_id),
		_ => None,
	});
	assert_eq!(welcome_id, Some(0), "freed id reused");
}

#[test]
fn stalled_datagram_queue_escalates_to_drop() {
	let mut sim = flat_sim("stall");
	let c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// Never drain: snapshots fill the bounded queue, then the stall
	// clock runs. OUT_CAP fills at one snapshot per SNAPSHOT_DIVISOR
	// ticks, then DGRAM_STALL_SECS of Full escalates to dead.
	let enough = 3 * super::OUT_CAP as u32 + (super::DGRAM_STALL_SECS + 2) * 60;
	for _ in 0..enough {
		sim.tick_once(vec![]);
		if sim.player_count() == 0 {
			break;
		}
	}
	assert_eq!(sim.player_count(), 0, "stalled reader dropped");
	drop(c);
}

#[test]
fn edit_request_flood_is_bounded_and_rejected() {
	let mut sim = flat_sim("flood");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let flood: Vec<SimEvent> = (0..1000)
		.map(|seq| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: [0.0; 3],
				dir: [0.0, -1.0, 0.0],
				kind: digblock_net::messages::EditKind::Dig,
				cell: [0, 0, 0],
				normal: [0, 0, 0],
				slot: 0,
			},
		})
		.collect();
	sim.tick_once(flood);
	assert!(
		sim.action_queue.is_empty(),
		"queue fully drained after the tick"
	);
	c.drain();
	let rejected = c
		.msgs
		.iter()
		.filter(|m| {
			matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::RateLimited,
					..
				}
			)
		})
		.count();
	assert!(rejected > 0, "flood produced rate-limit rejections");
}

#[test]
fn hostile_names_are_sanitized() {
	let mut sim = flat_sim("names");
	let (tx, _rx) = channel(super::OUT_CAP);
	let long_name = "x".repeat(1000) + "\n\u{7}";
	sim.tick_once(vec![
		SimEvent::Connected {
			conn_id: 5,
			out: tx,
			addr: test_addr(5),
			account: AccountId::NONE,
		},
		SimEvent::Msg {
			conn_id: 5,
			msg: ClientMsg::Hello {
				protocol: PROTOCOL_VERSION,
				name: long_name,
				draw_dist: 4,
				token: [0; 16],
				sim_version: digblock_core::SIM_VERSION,
				build: "t".into(),
			},
		},
	]);
	let mut b = connect(&mut sim, 6);
	b.drain();
	let roster_name = b
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::PlayerJoined { id: 0, name } => Some(name.clone()),
			_ => None,
		})
		.expect("roster entry for the hostile name");
	assert_eq!(roster_name.len(), digblock_net::protocol::MAX_NAME_LEN);
	assert!(roster_name.chars().all(|c| !c.is_control()));
}

/// The migration, end to end through the sim: a player joins a server
/// they used to play on before accounts, offers the key their old
/// character is filed under, and gets it — once.
///
/// The second join is the half worth having. A client that still has
/// its old identity file sends this claim on EVERY join forever,
/// because it cannot know which servers it has already migrated on, so
/// "claimed already" has to be as quiet and as harmless as "never
/// played here" — and above all must not pay out twice.
#[test]
fn a_legacy_character_transfers_to_the_account_once() {
	use digblock_core::block::BlockType;
	use digblock_core::skills::Skill;

	let dir = temp_dir("legacyclaim");
	let account = test_account(21);
	let legacy = [0x9au8; 16];
	{
		// Stand in for what a pre-account server left on disk. The
		// store's own tests cover reading a v1 FILE; what this one is
		// about is the sim path on top of it.
		let mut sim =
			super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let mut old = crate::player_store::PlayerRecord::default();
		old.inventory.add(BlockType::Dirt, 12);
		old.skills.add_xp(Skill::Mining, 4_000);
		sim.test_put_legacy(legacy, old);

		let mut c = connect_as_account(&mut sim, 1, account);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::InventoryFull { counts } if counts.is_empty()
			)),
			"join sync still shows the account's own empty inventory: {:?}",
			c.msgs
		);

		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::ClaimLegacy { legacy },
		}]);
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::InventoryFull { counts }
					if counts.contains(&(BlockType::Dirt as u16, 12))
			)),
			"the claim rebases the inventory: {:?}",
			c.msgs
		);
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::Skills { xp } if xp.contains(&(Skill::Mining as u8, 4_000))
			)),
			"and the levels: {:?}",
			c.msgs
		);
		// The live client, not just the wire — the next edit validates
		// against this.
		assert_eq!(sim.test_client_inventory(1).count(BlockType::Dirt), 12);
		sim.shutdown();
	}

	// Restart on the same data dir, and claim again with the same key.
	// The character is on the account now; the claim is a no-op.
	let mut sim = super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	let mut c = connect_as_account(&mut sim, 2, account);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::InventoryFull { counts }
				if counts.contains(&(BlockType::Dirt as u16, 12))
		)),
		"the migrated character survived the restart: {:?}",
		c.msgs
	);
	c.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 2,
		msg: ClientMsg::ClaimLegacy { legacy },
	}]);
	c.drain();
	assert!(
		!c.msgs.iter().any(is_claim_answer),
		"a spent claim says nothing at all: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_client_inventory(2).count(BlockType::Dirt),
		12,
		"and above all does not pay out twice"
	);
}

/// An unverified connection carries no account, so there is nothing a
/// claim could transfer ONTO. It must not consume the quarantined
/// record on the way to discovering that — otherwise any accountless
/// peer could burn every legacy character on the server by guessing.
#[test]
fn an_accountless_connection_cannot_spend_a_legacy_character() {
	let mut sim = flat_sim("legacyanon");
	let legacy = [0x9bu8; 16];
	sim.test_put_legacy(legacy, crate::player_store::PlayerRecord::default());

	let mut c = connect(&mut sim, 1); // AccountId::NONE
	c.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::ClaimLegacy { legacy },
	}]);
	c.drain();
	assert!(
		!c.msgs.iter().any(is_claim_answer),
		"no answer: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_legacy_len(),
		1,
		"the character is still there for whoever actually owns it"
	);
}

/// Anything the sim only ever sends in answer to a claim. Ordinary
/// join traffic (`AreaReady`, snapshots) keeps flowing either way, so
/// "said nothing" has to mean these and not "sent no frame at all".
fn is_claim_answer(m: &ServerMsg) -> bool {
	matches!(
		m,
		ServerMsg::InventoryFull { .. }
			| ServerMsg::Layout { .. }
			| ServerMsg::Skills { .. }
			| ServerMsg::Chat { .. }
	)
}

/// Inventory persists across reconnect AND across a full server
/// rebuild on the same data dir, keyed by the ACCOUNT — which is what
/// makes a character follow a player to a second device rather than
/// staying with the machine that made it.
#[test]
fn account_inventory_persists_across_reconnect_and_reboot() {
	use digblock_core::block::BlockType;
	use digblock_net::messages::EditKind;
	use glam::Vec3;

	let dir = temp_dir("persist");
	let account = test_account(7);
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	{
		let mut sim =
			super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let _c = connect_as_account(&mut sim, 1, account);
		wait_live(&mut sim, 1);
		let mut t = 0;
		dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
		// The rock rule: mined stone lands as Rubble.
		assert_eq!(sim.test_inventory_count(1, BlockType::Rubble), 1);

		// Park the Rubble deep in the grid (a deliberate arrangement —
		// it must survive reconnect and reboot verbatim).
		let mut grid = vec![digblock_core::hotbar::WIRE_EMPTY; digblock_core::inv_grid::GRID_SLOTS];
		grid[17] = BlockType::Rubble as u16;
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Layout {
				hotbar: vec![digblock_core::hotbar::WIRE_EMPTY; 8]
					.into_iter()
					.chain([digblock_core::hotbar::WIRE_TOOL])
					.collect(),
				grid: grid.clone(),
			},
		}]);

		// Reconnect within the same server run.
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
		let mut c2 = connect_as_account(&mut sim, 2, account);
		c2.drain();
		assert!(
			c2.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::InventoryFull { counts }
					if counts == &vec![(BlockType::Rubble as u16, 1)]
			)),
			"reconnect carries the credit: {:?}",
			c2.msgs
		);
		assert!(
			c2.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::Layout { grid: g, .. } if g == &grid
			)),
			"reconnect carries the arrangement: {:?}",
			c2.msgs
		);
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 2 }]);
		sim.shutdown();
	}
	// Full reboot on the same data dir.
	let mut sim = super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	let mut c3 = connect_as_account(&mut sim, 3, account);
	c3.drain();
	assert!(
		c3.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::InventoryFull { counts }
				if counts == &vec![(BlockType::Rubble as u16, 1)]
		)),
		"reboot carries the credit: {:?}",
		c3.msgs
	);
	assert!(
		c3.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::Layout { grid, .. }
				if grid.get(17) == Some(&(BlockType::Rubble as u16))
		)),
		"reboot carries the arrangement: {:?}",
		c3.msgs
	);
	// A different account starts empty.
	let mut c4 = connect_as_account(&mut sim, 4, test_account(9));
	c4.drain();
	assert!(
		c4.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::InventoryFull { counts } if counts.is_empty()
		)),
		"{:?}",
		c4.msgs
	);
}

/// The strength wall, server half: a DigStart on stone below the Rock
/// family's wall refuses with `SkillGated` and opens no hold (refused
/// at the START — the player must not hold the button through a dig
/// timer for nothing); at the gate level the hold opens normally.
#[test]
fn gated_rock_refuses_the_dig_start() {
	use digblock_core::raycast::{REACH, raycast};
	use glam::Vec3;
	let mut sim = flat_sim("rockgate");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigStart {
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::DigRefused {
				reason: digblock_net::messages::RejectReason::SkillGated,
				..
			}
		)),
		"level-1 stone dig refused at the start: {:?}",
		c.msgs
	);
	assert_eq!(sim.test_digging(1), None, "no hold opened");

	sim.bench_set_mining_level(
		1,
		digblock_core::unlock::mine(digblock_core::BlockType::Stone),
	);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigStart {
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			slot: 0,
		},
	}]);
	assert_eq!(sim.test_digging(1), Some(hit.cell), "gate cleared at 5");
}

/// SKILLS-DESIGN §5.3, server half — the whole ore loop in one test, because
/// the three halves only mean anything together:
///
/// 1. Below the reveal, a vein is stone: the hold opens, the dig completes,
///    and the player gets rubble and never knows.
/// 2. At the reveal WITHOUT the siphon, `DigStart` refuses with `NeedsTool`
///    and opens no hold — if you can see the seam, the game will not let you
///    spend it by accident.
/// 3. With the siphon, the completing dig writes the tier's SPENT ROCK rather
///    than air, credits ESSENCE, and pays BOTH skills.
#[test]
fn a_revealed_vein_needs_the_siphon_and_pays_two_skills() {
	use digblock_core::block::BlockType;
	use digblock_core::skills::Skill;
	use digblock_net::messages::EditKind;
	use glam::Vec3;
	let (cell, eye, ore) = super::harness::a_vein_under_spawn();
	let reveal = digblock_core::unlock::reveal(ore).expect("an ore");

	// 1. Below the reveal: stone all the way down.
	let mut sim = super::harness::flat_sim_at("ore-blind", eye);
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_mining_level(1, reveal - 1);
	let mut t = 100;
	let down = Vec3::new(0.0, -1.0, 0.0);
	let hold = BlockType::Stone.hardness_ticks().unwrap() as u32 + 8;
	let dug = dig_hold(&mut sim, 1, down, hold, 1, EditKind::Dig, &mut t);
	c.drain();
	assert_eq!(dug, cell, "dug the cell the vein is in");
	assert_eq!(sim.test_cell_at(cell.0, cell.1, cell.2), Some(0), "a hole");
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Rubble),
		1,
		"rubble, and no idea what was lost"
	);
	assert_eq!(sim.test_inventory_count(1, ore), 0);

	// 2. At the reveal, bare-handed: refused before the hold opens.
	let mut sim = super::harness::flat_sim_at("ore-refuse", eye);
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_mining_level(1, reveal);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigStart {
			cell: [cell.0, cell.1, cell.2],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::DigRefused {
				reason: digblock_net::messages::RejectReason::NeedsTool,
				..
			}
		)),
		"a revealed vein refuses a bare hand: {:?}",
		c.msgs
	);
	assert_eq!(sim.test_digging(1), None, "no hold opened");

	// 3. With the siphon: a draw, not a dig.
	let mut sim = super::harness::flat_sim_at("ore-draw", eye);
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_mining_level(1, reveal);
	sim.test_grant(1, BlockType::EssenceSiphon, 1);
	sim.bench_set_active_tool(1, BlockType::EssenceSiphon);
	let before = (
		sim.test_skill_xp(1, Skill::Mining),
		sim.test_skill_xp(1, Skill::Alchemy),
	);
	let mut t = 100;
	// Timed as the ORE, which is slower than the stone it sits in.
	let hold = ore.hardness_ticks().unwrap() as u32 + 8;
	let dug = dig_hold(&mut sim, 1, down, hold, 1, EditKind::Dig, &mut t);
	c.drain();
	assert_eq!(dug, cell);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(digblock_core::worldgen::nodes::spent_block(ore).expect("a tier") as u16),
		"the TIER's tapped face, not a hole and not one generic grey \
			 stone — mining ore never excavates, and a worked seam still \
			 reads as the seam you worked"
	);
	// Essence, straight into the stocks. Every line of the ore's own
	// yield, asked of the shared function rather than restated, so a
	// retune of what a tier is worth moves the test with it.
	let alchemy = sim.test_skill_level(1, Skill::Alchemy);
	let mining = sim.test_skill_level(1, Skill::Mining);
	// Cell and seed included: part of a draw is derived from the vein
	// itself (`ore::traces_aether`), so asking without them would be
	// asking a different question than the server answered.
	let want = digblock_core::alchemy::field_yield(ore, alchemy, mining, cell, sim.store.seed());
	assert!(!want.is_empty(), "a vein has to be worth drawing");
	for (e, n) in want {
		assert_eq!(sim.test_sub_count(1, e), n, "{e:?} from the draw");
	}
	assert_eq!(
		sim.test_inventory_count(1, ore),
		0,
		"and no ore BLOCK — a vein is never an item"
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Rubble),
		0,
		"never the stone's own drop"
	);
	let (mining_xp, alch_xp) = digblock_core::skills::field_xp(ore).expect("an ore");
	assert_eq!(
		sim.test_skill_xp(1, Skill::Mining) - before.0,
		mining_xp,
		"full Mining at the rock face"
	);
	assert_eq!(
		sim.test_skill_xp(1, Skill::Alchemy) - before.1,
		alch_xp,
		"and the Alchemy cut — neither skill levels alone"
	);

	// 4. And it is timed as the ore on THIS end too. Case 3 alone
	//    cannot show that: it holds for the ore's own length, which
	//    satisfies a server timing the draw by either number. An ore is
	//    several times Stone's hardness, so a server reading the cell's
	//    hardness would hand out an Aetherock for a fraction of the
	//    intended hold — a gap `dig_grace` comes nowhere near, and one
	//    a modified client can simply take.
	let mut sim = super::harness::flat_sim_at("ore-short", eye);
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_mining_level(1, reveal);
	sim.test_grant(1, BlockType::EssenceSiphon, 1);
	sim.bench_set_active_tool(1, BlockType::EssenceSiphon);
	let mut t = 100;
	let stone_hold = BlockType::Stone.hardness_ticks().unwrap() as u32 + 8;
	assert!(
		stone_hold < ore.hardness_ticks().unwrap() as u32,
		"the vein has to out-last its stone or this proves nothing"
	);
	let dug = dig_hold(&mut sim, 1, down, stone_hold, 1, EditKind::Dig, &mut t);
	c.drain();
	assert_eq!(
		sim.test_cell_at(dug.0, dug.1, dug.2),
		Some(BlockType::Stone as u16),
		"a stone-length hold does not draw the vein"
	);
	assert_eq!(sim.test_inventory_count(1, ore), 0, "and credits nothing");

	// 5. Owning the siphon is not holding it (SIM v20). With the bar
	//    on a block slot the tool axis still names the siphon — that
	//    is what it is FOR, it survives a block selection — and the
	//    server must answer exactly as it does for the bare hand
	//    above, or a player carrying a siphon draws every seam they
	//    look at while holding dirt.
	let mut sim = super::harness::flat_sim_at("ore-stowed", eye);
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_mining_level(1, reveal);
	sim.test_grant(1, BlockType::EssenceSiphon, 1);
	sim.bench_set_active_tool(1, BlockType::EssenceSiphon);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigStart {
			cell: [cell.0, cell.1, cell.2],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::DigRefused {
				reason: digblock_net::messages::RejectReason::NeedsTool,
				..
			}
		)),
		"a stowed siphon is no siphon: {:?}",
		c.msgs
	);
	assert_eq!(sim.test_digging(1), None, "no hold opened");
	// And the declared slot is what decides it — the same client, the
	// same tick, now with the hand on the tool.
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigStart {
			cell: [cell.0, cell.1, cell.2],
			slot: digblock_core::hotbar::HOTBAR_SLOTS as u8 - 1,
		},
	}]);
	assert!(
		sim.test_digging(1).is_some(),
		"the hold opens the moment the siphon is in hand"
	);
}

/// SKILLS-DESIGN §5.6, server half: the Resonant Pick breaks the
/// PLANE of the face it hits, derived from the server's own raycast
/// and its own world — the client names one cell and this end decides
/// what the swing touched.
///
/// The three properties that matter, in one test because they are one
/// mechanism: the width follows the level, every extra cell holds the
/// same id, and the credit is one per BLOCK broken — which is what
/// `begin_multi`'s single `inv_delta` describes.
#[test]
fn the_resonant_pick_breaks_the_plane_the_server_derives() {
	use digblock_core::block::BlockType;
	use digblock_core::unlock::{PICK_PLANE, PICK_PLANE_AT, PICK_WIDE_AT};
	use digblock_net::messages::EditKind;
	use glam::Vec3;
	// Read off the unlock table rather than written down again: this is
	// one of the two test suites `unlock.rs` names as having each carried
	// its own copy of these thresholds.
	for (level, want) in [
		(PICK_WIDE_AT - 1, 1usize),
		(PICK_WIDE_AT, 3),
		(PICK_PLANE_AT, PICK_PLANE),
	] {
		let mut sim = flat_sim(&format!("pick{level}"));
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		sim.bench_set_mining_level(1, level);
		sim.test_grant(1, BlockType::ResonantPick, 1);
		sim.bench_set_active_tool(1, BlockType::ResonantPick);
		let mut t = 100;
		// Straight down at the flat stone floor: the plane is
		// horizontal, so every cell of it is stone and qualifies.
		let down = Vec3::new(0.0, -1.0, 0.0);
		let hold = BlockType::Stone.hardness_ticks().unwrap() as u32 * 3 + 16;
		let dug = dig_hold(&mut sim, 1, down, hold, 1, EditKind::Dig, &mut t);
		c.drain();
		let broken = (-1..=1)
			.flat_map(|dx| (-1..=1).map(move |dz| (dx, dz)))
			.filter(|&(dx, dz)| sim.test_cell_at(dug.0 + dx, dug.1, dug.2 + dz) == Some(0))
			.count();
		assert_eq!(broken, want, "level {level} breaks {want}");
		assert_eq!(
			sim.test_inventory_count(1, BlockType::Rubble),
			want as u32,
			"level {level}: one credit per block, not per swing"
		);
		// Nothing behind the face: a pick widens a wall, it does not
		// bore through one.
		assert_ne!(
			sim.test_cell_at(dug.0, dug.1 - 1, dug.2),
			Some(0),
			"level {level} tunnelled"
		);
	}
}

/// A protected cell ANYWHERE in the pick's plane refuses the whole
/// swing — proved rather than cited, because "the check already runs
/// over the assembled `writes`" is exactly the kind of claim that
/// stops being true the moment somebody assembles the writes
/// differently. Nothing partially breaks: a refused action leaves the
/// aimed cell standing too.
#[test]
fn a_protected_cell_refuses_the_whole_pick_swing() {
	use digblock_core::block::BlockType;
	use digblock_core::raycast::{REACH, raycast};
	use digblock_net::messages::EditKind;
	use glam::Vec3;
	let mut sim = flat_sim("pick-protect");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_mining_level(1, digblock_core::unlock::PICK_PLANE_AT);
	sim.test_grant(1, BlockType::ResonantPick, 1);
	sim.bench_set_active_tool(1, BlockType::ResonantPick);
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.0, -1.0, 0.0);
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	// Lock a region whose NEAR edge is the aimed cell's +x neighbour,
	// so the aimed cell itself is free and the refusal can only come
	// from a cell the swing reached. (`lock_region` is a sphere of
	// `LOCK_RADIUS` around the eye's floor cell.)
	let lock_x = hit.cell.0 + 1 + digblock_core::spawn::LOCK_RADIUS;
	sim.bench_lock_spawn_at(Vec3::new(
		lock_x as f32 + 0.5,
		hit.cell.1 as f32 + 2.5,
		hit.cell.2 as f32 + 0.5,
	));
	let locks = sim.protected_regions();
	assert!(
		digblock_core::protect::immutable(hit.cell.0 + 1, hit.cell.1, hit.cell.2, &locks)
			&& !digblock_core::protect::immutable(hit.cell.0, hit.cell.1, hit.cell.2, &locks),
		"the lock has to straddle the plane, not swallow it"
	);
	let mut t = 100;
	let hold = BlockType::Stone.hardness_ticks().unwrap() as u32 * 3 + 16;
	dig_hold(&mut sim, 1, down, hold, 1, EditKind::Dig, &mut t);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				reason: digblock_net::messages::RejectReason::Protected,
				..
			}
		)),
		"the swing reached a locked cell: {:?}",
		c.msgs
	);
	assert_ne!(
		sim.test_cell_at(hit.cell.0, hit.cell.1, hit.cell.2),
		Some(0),
		"nothing partially broke"
	);
}

/// SIM v13's craft gate, server half: a DigStart on a craft-only block
/// below the Alchemy level that makes it refuses with `NotUnlocked` and
/// opens no hold — you cannot mine your way to a torch you can't yet
/// transmute, in your own world or through somebody else's build. The
/// natural block beside it stays freely diggable at the same level.
#[test]
fn craft_only_blocks_refuse_the_dig_start() {
	use digblock_core::block::BlockType;
	use digblock_core::raycast::{REACH, raycast};
	use glam::Vec3;
	let mut sim = flat_sim("craftgate");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let ground = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	// Stand a torch on the flat world's surface, clear of the capsule
	// (DigStart validates the gates and the cell, never the reach). The
	// world is stone, so clear the strength wall too — this test is about
	// the other one.
	sim.bench_set_mining_level(1, digblock_core::unlock::mine(BlockType::Stone));
	let cell = (ground.cell.0 + 3, ground.cell.1 + 1, ground.cell.2);
	sim.test_set_cell(cell.0, cell.1, cell.2, BlockType::Torch as u16);
	let start = |sim: &mut super::Sim| {
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [cell.0, cell.1, cell.2],
				slot: 0,
			},
		}]);
	};

	start(&mut sim);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::DigRefused {
				reason: digblock_net::messages::RejectReason::NotUnlocked,
				..
			}
		)),
		"level-1 torch dig refused at the start: {:?}",
		c.msgs
	);
	assert_eq!(sim.test_digging(1), None, "no hold opened");

	// One level short is still short — the gate is the recipe's level,
	// read out of the unlock table rather than written down here.
	let need = digblock_core::unlock::pickup(BlockType::Torch);
	sim.bench_set_alchemy_level(1, need - 1);
	start(&mut sim);
	assert_eq!(sim.test_digging(1), None, "still gated one level short");

	sim.bench_set_alchemy_level(1, need);
	start(&mut sim);
	assert_eq!(
		sim.test_digging(1),
		Some(cell),
		"gate cleared at the recipe"
	);

	// The stone underneath was never the craft gate's business.
	sim.test_set_cell(cell.0, cell.1, cell.2, BlockType::Air as u16);
	let mut c2 = connect(&mut sim, 2);
	wait_live(&mut sim, 2);
	sim.bench_set_mining_level(2, digblock_core::unlock::mine(BlockType::Stone));
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 2,
		msg: ClientMsg::DigStart {
			cell: [ground.cell.0, ground.cell.1, ground.cell.2],
			slot: 0,
		},
	}]);
	c2.drain();
	assert_eq!(
		sim.test_digging(2),
		Some(ground.cell),
		"natural stone digs at Alchemy 1: {:?}",
		c2.msgs
	);
}

/// XP is server-computed at the two chokepoints and lands as new
/// TOTALS: a stone dig pays the block's `dig_xp` on top of the
/// harness's gate grant, crafting pays its `craft_xp` in Alchemy, and
/// the totals survive reconnect and a full server reboot via the
/// account's stored record (join sync always carries a `Skills`
/// frame, empty for a fresh account).
#[test]
fn xp_awards_on_dig_and_alchemy_and_persists() {
	use digblock_core::block::BlockType;
	use digblock_net::messages::{AlchemyOp, EditKind};
	use glam::Vec3;

	let dir = temp_dir("xp");
	let account = test_account(8);
	// dig_hold grants whatever the target's gates ask — for stone that
	// is the Rock family's strength wall — and the dig pays its own
	// `dig_xp` on top of that level's total. Derived, so a retune of
	// the wall or of the dig curve moves both halves together.
	let granted =
		digblock_core::skills::total_xp_for(digblock_core::unlock::mine(BlockType::Stone));
	// Filled in from where the hold actually lands: §5.2's depth reward
	// keys on the dug CELL, so the expected total is derived from the
	// same call the chokepoint makes rather than from the flat table.
	let after;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	{
		let mut sim =
			super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
		let mut c = connect_as_account(&mut sim, 1, account);
		c.drain();
		assert!(
			c.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::Skills { xp } if xp.is_empty())),
			"fresh account syncs empty skills: {:?}",
			c.msgs
		);
		wait_live(&mut sim, 1);
		let mut t = 0;
		let cell = dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
		after = granted + digblock_core::skills::dig_award(BlockType::Stone, None, 1, cell.1).0;
		c.drain();
		assert!(
			c.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::XpGain { skill: 0, total } if *total == after)),
			"stone dig pays its Mining award on the granted {granted}: {:?}",
			c.msgs
		);

		// Craft the first vessel from granted timber: its `craft_xp`
		// in Alchemy, in wire order with the InventoryFull rebase.
		let timber: u32 = digblock_core::alchemy::craft_feedstock(BlockType::Bucket)
			.iter()
			.map(|&(_, n)| n)
			.sum();
		sim.test_grant(1, BlockType::Wood, timber);
		let bucket_xp = digblock_core::skills::craft_xp(BlockType::Bucket);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Alchemy {
				op: AlchemyOp::Craft {
					item: BlockType::Bucket as u16,
					count: 1,
				},
			},
		}]);
		c.drain();
		assert!(
			c.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::XpGain { skill: 1, total } if *total == bucket_xp)),
			"the vessel pays its craft_xp: {:?}",
			c.msgs
		);

		// Reconnect in the same run: the totals ride the Skills sync.
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
		let mut c2 = connect_as_account(&mut sim, 2, account);
		c2.drain();
		let bucket_xp = digblock_core::skills::craft_xp(BlockType::Bucket);
		assert!(
			c2.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::Skills { xp } if xp == &vec![(0, after), (1, bucket_xp)]
			)),
			"reconnect carries the totals: {:?}",
			c2.msgs
		);
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 2 }]);
		sim.shutdown();
	}
	// Full reboot on the same data dir.
	let mut sim = super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	let mut c3 = connect_as_account(&mut sim, 3, account);
	c3.drain();
	let bucket_xp = digblock_core::skills::craft_xp(BlockType::Bucket);
	assert!(
		c3.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::Skills { xp } if xp == &vec![(0, after), (1, bucket_xp)]
		)),
		"reboot carries the totals: {:?}",
		c3.msgs
	);
}

/// The retired verb: a legacy `Convert` frame still decodes — the
/// wire is append-only — and the server refuses it outright. Nothing
/// is debited, no XP moves, and the standing `InventoryFull` rebase is
/// the whole answer, so an out-of-date client at least resyncs.
#[test]
fn a_legacy_convert_is_refused_with_the_standing_rebase() {
	use digblock_core::block::BlockType;
	use digblock_net::messages::AlchemyOp;
	let mut sim = flat_sim("noconvert");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::Rubble, 5);
	c.drain();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Alchemy {
			op: AlchemyOp::Convert {
				block: BlockType::Rubble as u16,
				count: 1,
			},
		},
	}]);
	c.drain();
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Rubble),
		5,
		"nothing debited"
	);
	assert!(
		!c.msgs.iter().any(|m| matches!(m, ServerMsg::XpGain { .. })),
		"no XP for a retired verb: {:?}",
		c.msgs
	);
	assert!(
		c.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::InventoryFull { .. })),
		"the standing rebase still answers: {:?}",
		c.msgs
	);
}

/// §8.1's anti-cheat half over the bench's one remaining verb: a Craft
/// costs the item's own `craft_pace_ticks` out of the per-player bench
/// bucket, charged per item, so a bulk `count` cannot buy a batch's XP
/// in one frame and a flood of single ops beyond the burst refuses
/// with no debit — sustained throughput can never beat one craft per
/// pace interval.
#[test]
fn crafting_pace_is_server_enforced() {
	use digblock_core::block::{BlockType, Essence};
	use digblock_net::messages::AlchemyOp;
	let mut sim = flat_sim("craftpace");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let level = digblock_core::unlock::craft(BlockType::LumberSlab).expect("a recipe");
	sim.bench_set_alchemy_level(1, level);
	sim.test_grant(1, BlockType::Wood, 100_000);
	sim.test_grant_essence(1, Essence::Vitae, 100_000);
	let pace = digblock_core::alchemy::craft_pace_ticks(BlockType::LumberSlab, level);
	let burst = digblock_core::alchemy::bench_burst_ticks(level);
	let admits = (burst / pace as f32) as u32;
	assert!(admits >= 2, "a burst that admits fewer than two is not one");
	let craft = |count: u32| SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Alchemy {
			op: AlchemyOp::Craft {
				item: BlockType::LumberSlab as u16,
				count,
			},
		},
	};
	// One op per tick throughout: an Alchemy message also spends an
	// EDIT token, so bunching several into one tick measures that
	// bucket instead of this one. One past the burst is one too many —
	// refused outright, nothing debited and no XP.
	let batch = digblock_core::shapes::craft_batch(BlockType::LumberSlab);
	sim.tick_once(vec![craft(admits + 1)]);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::LumberSlab),
		0,
		"a bulk craft beyond the burst is refused with no debit"
	);
	// What it CAN cover lands, and empties the bucket.
	sim.tick_once(vec![craft(admits)]);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::LumberSlab),
		admits * batch
	);
	sim.tick_once(vec![craft(1)]);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::LumberSlab),
		admits * batch,
		"the drip is what is left once the burst is gone"
	);
	// One pace interval refills exactly one more craft's worth.
	for _ in 0..pace {
		sim.tick_once(vec![]);
	}
	sim.tick_once(vec![craft(1)]);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::LumberSlab),
		(admits + 1) * batch
	);
	// Refused ops paid nothing: the total is exactly the landed
	// crafts, at the slab's own `craft_xp` each.
	c.drain();
	let last_alch = c.msgs.iter().rev().find_map(|m| match m {
		ServerMsg::XpGain { skill: 1, total } => Some(*total),
		_ => None,
	});
	// Against the level this rig was GRANTED, since `XpGain` carries a
	// running total and not a delta.
	let each = digblock_core::skills::craft_xp(BlockType::LumberSlab);
	let granted = digblock_core::skills::total_xp_for(level);
	assert_eq!(
		last_alch,
		Some(granted + u64::from(admits + 1) * each),
		"XP only for landed crafts"
	);
}

/// A live account refuses a second connection; unverified (accountless)
/// connections coexist freely, because they share no persisted record
/// to stomp.
#[test]
fn duplicate_account_refused_anonymous_pairs_ok() {
	let mut sim = flat_sim("dupid");
	let _a = connect_as_account(&mut sim, 1, test_account(5));
	let dup = try_connect(&mut sim, 2, test_account(5));
	assert_matches!(
		dup.msgs.first(),
		Some(ServerMsg::Refused { reason }) if reason.contains("account"),
		"{:?}",
		dup.msgs
	);
	// Anonymous pair: both welcomed (connect asserts Welcome).
	let _b = connect(&mut sim, 3);
	let _c = connect(&mut sim, 4);
	assert_eq!(sim.player_count(), 3);

	// The refused account can join once the holder leaves.
	sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
	let _d = connect_as_account(&mut sim, 5, test_account(5));
	assert_eq!(sim.player_count(), 3);
}

#[test]
fn return_to_spawn_teleports_with_cooldown() {
	use digblock_net::protocol::{RETURN_SPAWN_COOLDOWN_SECS, TICK_RATE};
	let request = |sim: &mut super::Sim| {
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::ReturnToSpawn,
		}]);
	};
	let mut sim = flat_sim("rts");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);

	// Walk away, then return: teleported to spawn (one settle tick of
	// slack — the teleport lands before the tick's physics).
	let mut t = 100;
	for _ in 0..240 {
		sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
		t += 1;
	}
	assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) > 5.0);
	request(&mut sim);
	assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) < 1.0);
	c.drain();
	assert!(
		c.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Teleported { .. })),
		"client told about the teleport: {:?}",
		c.msgs
	);

	// Inside the cooldown the request is silently ignored.
	for _ in 0..120 {
		sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
		t += 1;
	}
	let away = sim.test_player(1).unwrap().pos;
	assert!(away.distance(FLAT_SPAWN) > 5.0);
	request(&mut sim);
	assert!(
		sim.test_player(1).unwrap().pos.distance(away) < 1.0,
		"cooldown violation ignored"
	);

	// After the window it works again. (Drain as we go — an undrained
	// outbound queue trips the snapshot-stall reaper.)
	for _ in 0..(RETURN_SPAWN_COOLDOWN_SECS * TICK_RATE) {
		sim.tick_once(vec![]);
		c.drain();
	}
	request(&mut sim);
	assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) < 1.0);
}

/// The cast chokepoint: a home jump pays Arcana by the DISTANCE it
/// covered (every figure derived from `skills::home_xp`), and the gate
/// widens to the caster's own rung — a request inside the base cooldown
/// but past the swift one teleports.
#[test]
fn a_home_cast_pays_its_distance_and_the_gate_reads_the_rung() {
	use digblock_core::skills::{Skill, home_xp, total_xp_for};
	use digblock_core::unlock::{RETURN_SWIFT_AT, return_cooldown_secs};
	use digblock_net::protocol::TICK_RATE;
	let request = |sim: &mut super::Sim| {
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::ReturnToSpawn,
		}]);
	};
	let mut sim = flat_sim("rtsxp");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_arcana_level(1, RETURN_SWIFT_AT);
	let base_total = total_xp_for(RETURN_SWIFT_AT);

	// Walk far enough that the pay is provably nonzero, then jump.
	for t in 100..400 {
		sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
	}
	let from = sim.test_player(1).unwrap().pos;
	assert!(
		from.distance(FLAT_SPAWN) > digblock_core::skills::HOME_XP_STRIDE,
		"never walked a stride's worth"
	);
	request(&mut sim);
	let landed = sim.test_player(1).unwrap().pos;
	c.drain();
	let total = c
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::XpGain { skill, total } if *skill == Skill::Arcana as u8 => Some(*total),
			_ => None,
		})
		.expect("a jump that covered ground pays");
	assert_eq!(total, base_total + home_xp(from.distance(landed)));

	// Standing at spawn, wait out the SWIFT cooldown — shorter than the
	// base one, which is what the rung buys — and recast: the teleport
	// answers (the widened gate), but a zero-length jump pays nothing.
	for _ in 0..(return_cooldown_secs(RETURN_SWIFT_AT) * TICK_RATE) {
		sim.tick_once(vec![]);
		c.drain();
	}
	assert!(return_cooldown_secs(RETURN_SWIFT_AT) < return_cooldown_secs(1));
	c.msgs.clear();
	request(&mut sim);
	c.drain();
	assert!(
		c.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Teleported { .. })),
		"the swift rung's window should already be open"
	);
	assert!(
		!c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::XpGain { skill, .. } if *skill == Skill::Arcana as u8
		)),
		"a zero-length jump paid XP"
	);
}

/// Blocks built at the spawn point raise the landing — for a joiner
/// AND a returning player — even when nobody is near spawn (the scan
/// reads the journal's in-memory cells, not resident chunks).
#[test]
fn blocked_spawn_raises_the_landing() {
	use digblock_net::messages::BlockEdit;
	let dir = temp_dir("blockedspawn");
	{
		let mut j = crate::journal::Journal::open(&dir, SEED).unwrap();
		// Two stone blocks stacked through the spawn capsule.
		for y in [FLAT_GROUND_TOP as i32 + 1, FLAT_GROUND_TOP as i32 + 2] {
			j.append(
				BlockEdit {
					x: 8,
					y,
					z: 8,
					block: digblock_core::block::BlockType::Stone as u16,
				},
				0,
			);
		}
		j.sync();
	}
	let mut sim = super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	let c = connect(&mut sim, 1);
	let spawn = c
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Welcome { spawn, .. } => Some(*spawn),
			_ => None,
		})
		.expect("Welcome");
	assert!(
		spawn[1] >= FLAT_SPAWN.y + 2.0,
		"join landed on top of the tower: y {} vs base {}",
		spawn[1],
		FLAT_SPAWN.y
	);
	assert_eq!(sim.test_player(1).unwrap().pos.to_array(), spawn);
}

#[test]
fn join_roster_and_announcements() {
	let mut sim = flat_sim("roster");
	let mut a = connect(&mut sim, 1); // id 0
	let b = connect(&mut sim, 2); // id 1

	// The joiner gets a roster entry for the player already online,
	// and never an announcement about itself.
	assert!(
		b.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::PlayerJoined { id: 0, name } if name.as_str() == "t1"
		)),
		"joiner missing roster entry: {:?}",
		b.msgs
	);
	assert!(
		!b.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::PlayerJoined { id: 1, .. }))
	);

	// The player already online hears the join...
	a.drain();
	assert!(a.msgs.iter().any(|m| matches!(
		m,
		ServerMsg::PlayerJoined { id: 1, name } if name.as_str() == "t2"
	)));

	// ...and the leave.
	sim.tick_once(vec![SimEvent::Disconnected { conn_id: 2 }]);
	a.drain();
	assert!(
		a.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::PlayerLeft { id: 1 }))
	);
}

#[test]
fn chat_broadcasts_to_all_including_sender() {
	let mut sim = flat_sim("chat");
	let mut a = connect(&mut sim, 1); // id 0
	let mut b = connect(&mut sim, 2); // id 1
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Chat {
			text: "hello world".into(),
		},
	}]);
	for c in [&mut a, &mut b] {
		c.drain();
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::Chat { from: 0, text } if text == "hello world"
			)),
			"chat missing (sender echo included): {:?}",
			c.msgs
		);
	}
}

#[test]
fn chat_flood_is_bounded_by_the_token_bucket() {
	let mut sim = flat_sim("chatflood");
	let mut a = connect(&mut sim, 1);
	let flood: Vec<SimEvent> = (0..20)
		.map(|i| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Chat {
				text: format!("spam {i}"),
			},
		})
		.collect();
	sim.tick_once(flood);
	a.drain();
	let heard = a
		.msgs
		.iter()
		.filter(|m| matches!(m, ServerMsg::Chat { .. }))
		.count();
	assert_eq!(
		heard,
		digblock_net::protocol::CHAT_BURST as usize,
		"flood must be capped at the burst, silently: {:?}",
		a.msgs
	);
}

/// A slash line prefixed with an invisible character is a COMMAND, not
/// chat. `is_command` used to be decided on the raw text while the
/// handler ran on the sanitized one, and `U+200B` splits them: it is
/// not `White_Space` (so `trim_start` leaves it) and not inside
/// `TEXT_RANGES` (so the sanitizer removes it). The line therefore read
/// as ordinary chat and was BROADCAST to every player as
/// "/ban griefer" — a non-admin publishing a command, or an admin
/// leaking one they meant to run.
#[test]
fn an_invisibly_prefixed_command_is_not_broadcast_as_chat() {
	let mut sim = flat_sim("zwsp");
	let mut a = connect(&mut sim, 1);
	let mut b = connect(&mut sim, 2);
	wait_live(&mut sim, 1);
	wait_live(&mut sim, 2);
	a.drain();
	b.drain();
	a.msgs.clear();
	b.msgs.clear();

	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Chat {
			text: "\u{200B}/ban griefer".into(),
		},
	}]);
	a.drain();
	b.drain();

	// Nobody sees it as chat...
	assert!(
		!b.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::Chat { from, text } if *from == 0 && text.contains("/ban")
		)),
		"the command leaked into the broadcast: {:?}",
		b.msgs
	);
	// ...and the sender is privately told it is admin-only, which is
	// what any non-admin slash line gets.
	assert!(
		a.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::Admin(digblock_net::messages::AdminReply::CommandsAreAdminOnly)
		)),
		"treated as a command and refused: {:?}",
		a.msgs
	);
}

/// Admin is an ACCOUNT compare, and the accountless connection must
/// never satisfy it. Every offline and test session carries
/// `AccountId::NONE`, so a set that admitted it would hand admin to all
/// of them at once — which is the shape of the bug this replaced, where
/// the key was something anyone could ask the master for.
#[test]
fn the_accountless_connection_is_never_admin() {
	let mut sim = flat_sim("adminacct");
	let boss = test_account(3);

	sim.set_admin_accounts([AccountId::NONE]);
	assert!(sim.admin_accounts.is_empty(), "NONE must not be stored");
	assert!(!sim.is_admin(AccountId::NONE));

	sim.set_admin_accounts([boss, AccountId::NONE]);
	assert!(sim.is_admin(boss));
	assert!(!sim.is_admin(AccountId::NONE));
	assert!(!sim.is_admin(test_account(4)));

	// Replaces rather than accumulates: an operator's list is the list.
	sim.set_admin_accounts([]);
	assert!(!sim.is_admin(boss));
}

#[test]
fn chat_before_hello_is_ignored() {
	let mut sim = flat_sim("chatlobby");
	let (tx, mut rx) = channel(super::OUT_CAP);
	sim.tick_once(vec![
		SimEvent::Connected {
			conn_id: 9,
			out: tx,
			addr: test_addr(9),
			account: AccountId::NONE,
		},
		SimEvent::Msg {
			conn_id: 9,
			msg: ClientMsg::Chat {
				text: "premature".into(),
			},
		},
	]);
	assert!(
		matches!(
			rx.try_recv(),
			Err(tokio::sync::mpsc::error::TryRecvError::Empty)
		),
		"lobby connection must hear nothing back"
	);
}

/// Reconnecting resumes where the player left off. Before this, a
/// rejoin was a free trip to spawn — the whole point of the
/// return-to-spawn cooldown, sidestepped by a disconnect.
#[test]
fn reconnect_resumes_at_the_last_position() {
	let dir = temp_dir("resume");
	let account = test_account(11);
	let mut sim = super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	let _c = connect_as_account(&mut sim, 1, account);
	wait_live(&mut sim, 1);

	// Walk well away from spawn.
	for t in (100..).take(240) {
		sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
	}
	let away = sim.test_player(1).unwrap().pos;
	assert!(away.distance(FLAT_SPAWN) > 5.0, "test needs to be away");

	sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
	let c2 = connect_as_account(&mut sim, 2, account);
	let spawn = c2
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Welcome { spawn, .. } => Some(glam::Vec3::from_array(*spawn)),
			_ => None,
		})
		.expect("Welcome");
	assert!(
		spawn.distance(away) < 1.0,
		"resumed at {spawn} instead of {away}"
	);
	assert_eq!(sim.test_player(2).unwrap().pos, spawn);
}

/// ...and the cooldown itself survives the reconnect, so rejoining
/// can't reset the timer either.
#[test]
fn reconnect_does_not_reset_the_spawn_cooldown() {
	let dir = temp_dir("resumecd");
	let account = test_account(12);
	let mut sim = super::Sim::new_with_source(SEED, &dir, 1, 2, flat_source(), FLAT_SPAWN).unwrap();
	let _c = connect_as_account(&mut sim, 1, account);
	wait_live(&mut sim, 1);

	// Walk away and use the return, consuming the cooldown.
	let mut t = 100;
	for _ in 0..240 {
		sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
		t += 1;
	}
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::ReturnToSpawn,
	}]);
	assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) < 1.0);

	// Walk away again, then reconnect and immediately re-request.
	for _ in 0..240 {
		sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
		t += 1;
	}
	sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
	let _c2 = connect_as_account(&mut sim, 2, account);
	wait_live(&mut sim, 2);
	let away = sim.test_player(2).unwrap().pos;
	assert!(away.distance(FLAT_SPAWN) > 5.0, "resumed away from spawn");
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 2,
		msg: ClientMsg::ReturnToSpawn,
	}]);
	assert!(
		sim.test_player(2).unwrap().pos.distance(away) < 1.0,
		"rejoining reset the return-to-spawn cooldown"
	);
}
