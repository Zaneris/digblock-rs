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

/// Growing the radius must request a sync scan. The edited-column walk is event-driven off
/// `needs_sync_scan`, which otherwise only rises when the center column moves — so a player who
/// widened their draw distance standing still gets no `JournalColumn` for the ring that just
/// became visible, and (because `broadcast_edit` gates on `synced_columns`) no live edits there.
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

/// The migration, end to end through the sim: a player joins a server they used to play on before
/// accounts, offers the key their old character is filed under, and gets it — once. The second
/// join is the half worth having: a client that still has its old identity file sends this claim
/// on EVERY join forever, so "claimed already" has to be as quiet and harmless as "never played
/// here" — and above all must not pay out twice.
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

/// SKILLS-DESIGN §5.3, server half — the whole ore loop in one test, because the parts only mean
/// anything together: (1) below the reveal a vein is stone and pays rubble; (2) at the reveal
/// without the siphon `DigStart` refuses with `NeedsTool` and opens no hold; (3) with the siphon
/// the completing dig writes the tier's SPENT ROCK rather than air, credits ESSENCE, and pays
/// BOTH skills.
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
	let want =
		digblock_core::alchemy::field_yield(ore, alchemy, mining, cell, sim.store.seed().value);
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

	// 4. And it is timed as the ore on THIS end too. Case 3 alone cannot show that: it holds for
	//    the ore's own length, which satisfies a server timing the draw by either number. An ore
	//    is several times Stone's hardness, so a server reading the cell's hardness would hand
	//    out an Aetherock for a fraction of the intended hold — far outside `dig_grace`.
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

	// 5. Owning the siphon is not holding it (SIM v20). With the bar on a block slot the tool
	//    axis still names the siphon — that is what it is FOR — and the server must answer
	//    exactly as it does for the bare hand above, or a player carrying a siphon draws every
	//    seam they look at while holding dirt.
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

/// SKILLS-DESIGN §5.6, server half: the Resonant Pick breaks the PLANE of the face it hits,
/// derived from the server's own raycast and its own world — the client names one cell and this
/// end decides what the swing touched. The three properties, in one test because they are one
/// mechanism: the width follows the level, every extra cell holds the same id, and the credit is
/// one per BLOCK broken — which is what `begin_multi`'s single `inv_delta` describes.
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

/// A protected cell ANYWHERE in the pick's plane refuses the whole swing — proved rather than
/// cited, because "the check already runs over the assembled `writes`" stops being true the moment
/// somebody assembles the writes differently. Nothing partially breaks: a refused action leaves
/// the aimed cell standing too.
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

/// XP is server-computed at the two chokepoints and lands as new TOTALS: a stone dig pays the
/// block's `dig_xp` on top of the harness's gate grant, crafting pays its `craft_xp` in Alchemy,
/// and the totals survive reconnect and a full server reboot via the account's stored record (join
/// sync always carries a `Skills` frame, empty for a fresh account).
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
		let timber: u32 = digblock_core::alchemy::craft_feedstock(BlockType::EssenceSiphon)
			.iter()
			.map(|&(_, n)| n)
			.sum();
		sim.test_grant(1, BlockType::Wood, timber);
		let siphon_xp = digblock_core::skills::craft_xp(BlockType::EssenceSiphon);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Alchemy {
				op: AlchemyOp::Craft {
					item: BlockType::EssenceSiphon as u16,
					count: 1,
				},
			},
		}]);
		c.drain();
		assert!(
			c.msgs
				.iter()
				.any(|m| matches!(m, ServerMsg::XpGain { skill: 1, total } if *total == siphon_xp)),
			"the vessel pays its craft_xp: {:?}",
			c.msgs
		);

		// Reconnect in the same run: the totals ride the Skills sync.
		sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
		let mut c2 = connect_as_account(&mut sim, 2, account);
		c2.drain();
		let siphon_xp = digblock_core::skills::craft_xp(BlockType::EssenceSiphon);
		assert!(
			c2.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::Skills { xp } if xp == &vec![(0, after), (1, siphon_xp)]
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
	let siphon_xp = digblock_core::skills::craft_xp(BlockType::EssenceSiphon);
	assert!(
		c3.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::Skills { xp } if xp == &vec![(0, after), (1, siphon_xp)]
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

/// §8.1's anti-cheat half over the bench's one remaining verb: a Craft costs the item's own
/// `craft_pace_ticks` out of the per-player bench bucket, charged per item, so a bulk `count`
/// cannot buy a batch's XP in one frame and a flood of single ops beyond the burst refuses with no
/// debit — sustained throughput can never beat one craft per pace interval.
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
	use digblock_net::protocol::{TELEPORT_COOLDOWN_SECS, TICK_RATE};
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
	for _ in 0..(TELEPORT_COOLDOWN_SECS * TICK_RATE) {
		sim.tick_once(vec![]);
		c.drain();
	}
	request(&mut sim);
	assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) < 1.0);
}

/// The two utility spells' server halves: a wisp is a debit and its
/// fare's XP (the light is per-viewer, client-side); a kindle validates
/// the rung, stands a light in the cell the caster's HEAD is in and
/// tells everyone — the caster included, since nothing is predicted —
/// for exactly `kindle::SECS`. Every figure read off the core tables.
#[test]
fn a_wisp_debits_and_a_kindle_conjures() {
	use digblock_core::skills::cast_xp;
	use digblock_net::messages::{CastKind, CastRefusal};
	let mut sim = flat_sim("wispkindle");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let cast = |sim: &mut super::Sim, kind: CastKind| {
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Cast(kind),
		}]);
	};

	// The wisp: broke refuses before any debit; funded debits exactly
	// and pays the fare's worth.
	let (w_essence, w_fare) =
		digblock_core::alchemy::wisp_price(digblock_core::skills::level_for_xp(
			sim.test_skill_xp(1, digblock_core::skills::Skill::Arcana),
		));
	cast(&mut sim, CastKind::Wisp);
	c.drain();
	assert!(c.msgs.iter().any(|m| matches!(
		m,
		ServerMsg::CastRefused {
			reason: CastRefusal::CantPay
		}
	)));
	sim.test_grant_essence(1, w_essence, w_fare);
	c.msgs.clear();
	cast(&mut sim, CastKind::Wisp);
	c.drain();
	assert_eq!(sim.test_essence(1, w_essence), Some(0), "the fare left");
	assert_eq!(
		sim.test_skill_xp(1, digblock_core::skills::Skill::Arcana),
		cast_xp(w_essence, w_fare)
	);

	// The kindle: below its rung, locked; at it, the light stands in
	// the head's own cell for the spell's whole span, and the retired
	// aimed form is refused whatever cell it names.
	let level = digblock_core::unlock::KINDLE_AT;
	let (k_essence, k_fare) = digblock_core::alchemy::kindle_price(level);
	sim.test_grant_essence(1, k_essence, k_fare * 2);
	c.msgs.clear();
	cast(&mut sim, CastKind::KindleHere);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::CastRefused {
				reason: CastRefusal::Locked
			}
		)),
		"kindle below its rung must refuse"
	);
	sim.bench_set_arcana_level(1, level);
	let head = digblock_core::kindle::host_cell(sim.test_player(1).unwrap().pos);
	c.msgs.clear();
	cast(&mut sim, CastKind::KindleHere);
	c.drain();
	let lit = c
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Kindled { cell, secs_left } => Some((*cell, *secs_left)),
			_ => None,
		})
		.expect("the caster is told too");
	assert_eq!(
		lit.0,
		[head.0, head.1, head.2],
		"the light stands at the head"
	);
	assert_eq!(
		u32::from(lit.1),
		digblock_core::kindle::SECS,
		"a fresh light carries the whole span"
	);
	assert_eq!(
		sim.test_essence(1, k_essence),
		Some(k_fare),
		"one fare left"
	);
	c.msgs.clear();
	cast(
		&mut sim,
		CastKind::Kindle {
			cell: [head.0, head.1, head.2],
			face: 0,
		},
	);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::CastRefused {
				reason: CastRefusal::BadTarget
			}
		)),
		"the retired aimed kindle is refused"
	);
	assert_eq!(
		sim.test_essence(1, k_essence),
		Some(k_fare),
		"and costs nothing"
	);
}

/// The ward frames one client was sent, in order — the WHOLE wire cost
/// of a cast, since the sphere's cells are derived rather than shipped.
fn ward_frames(c: &TestClient) -> Vec<&ServerMsg> {
	c.msgs
		.iter()
		.filter(|m| {
			matches!(
				m,
				ServerMsg::WardWinding { .. } | ServerMsg::WardEnded { .. }
			)
		})
		.collect()
}

/// How many landed edits a client was pushed. A ward contributes none:
/// this is the count that used to grow with the radius.
fn applied_edits(c: &TestClient) -> usize {
	c.msgs
		.iter()
		.filter(|m| matches!(m, ServerMsg::EditApplied { .. }))
		.count()
}

/// `(landed, sealed)` of the last ward ending the client saw.
fn last_ending(c: &TestClient) -> Option<(u32, bool)> {
	c.msgs.iter().rev().find_map(|m| match m {
		ServerMsg::WardEnded { landed, sealed, .. } => Some((*landed, *sealed)),
		_ => None,
	})
}

/// The sphere this caster's rank would draw, as the server sees the
/// world right now.
fn ward_plan(sim: &super::Sim, conn_id: u64, r: i32) -> Vec<((i32, i32, i32), u16)> {
	let head = digblock_core::kindle::host_cell(sim.test_player(conn_id).unwrap().pos);
	digblock_core::ward::plan(head, r, |c| sim.test_cell_at(c.0, c.1, c.2))
		.expect("a resident sphere")
}

/// Tick until every cell of a sphere that wide is resident. The widest
/// ranks reach into the neighbouring columns, and `ward::plan` refuses
/// outright until the world can answer for all of them — so this waits
/// on the very predicate the cast will ask.
fn wait_for_sphere(sim: &mut super::Sim, conn_id: u64, r: i32) {
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
	loop {
		let head = digblock_core::kindle::host_cell(sim.test_player(conn_id).unwrap().pos);
		if digblock_core::ward::plan(head, r, |c| sim.test_cell_at(c.0, c.1, c.2)).is_ok() {
			return;
		}
		assert!(
			std::time::Instant::now() < deadline,
			"the sphere never became resident"
		);
		sim.tick_once(vec![]);
		std::thread::sleep(std::time::Duration::from_millis(1));
	}
}

/// Ask for a ward on this connection's behalf.
fn cast_ward(sim: &mut super::Sim, conn_id: u64) {
	sim.tick_once(vec![SimEvent::Msg {
		conn_id,
		msg: ClientMsg::Cast(digblock_net::messages::CastKind::Ward),
	}]);
}

/// Every distinct radius the ladder can cast at, with a level that
/// reaches it — derived from the unlock table, so a new rank sweeps in.
fn ward_ranks() -> Vec<(u32, i32)> {
	let mut out: Vec<(u32, i32)> = Vec::new();
	for level in 1..=100 {
		let r = digblock_core::unlock::ward_radius(level);
		if r > 0 && out.last().map(|&(_, last)| last) != Some(r) {
			out.push((level, r));
		}
	}
	assert!(out.len() >= 2, "the ladder has ranks");
	out
}

/// The Ward lands whole or not at all, from the server's own view of the caster: below its rung it
/// is refused before any debit; at it, every shell cell that was air stands as wardglass — except
/// inside a locked region, which the sphere leaves exactly as it found it — the interior untouched,
/// the fare gone and its XP paid at the seal. The CELLS never travel: both ends derive the same
/// plan from the head and the radius, so a dozen cells and a thousand cost the same wire.
#[test]
fn a_ward_closes_about_the_caster_as_a_derived_sphere() {
	use digblock_core::block::BlockType;
	use digblock_core::chunk::{cell_id, make_cell};
	use digblock_core::skills::cast_xp;
	use digblock_net::messages::CastRefusal;

	let mut sim = flat_sim("ward");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let cast = |sim: &mut super::Sim| cast_ward(sim, 1);
	let level = digblock_core::unlock::WARD_AT;
	let (essence, fare) = digblock_core::alchemy::ward_price(level);
	sim.test_grant_essence(1, essence, fare);
	c.msgs.clear();
	cast(&mut sim);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::CastRefused {
				reason: CastRefusal::Locked
			}
		)),
		"a ward below its rung must refuse"
	);
	assert_eq!(
		sim.test_essence(1, essence),
		Some(fare),
		"and costs nothing"
	);

	sim.bench_set_arcana_level(1, level);
	// A locked pad beside the caster, well inside the sphere.
	sim.spawn_store
		.set_lock(Some(crate::spawn_store::SpawnLock {
			eye: (FLAT_SPAWN + glam::Vec3::new(4.0, 0.0, 0.0)).to_array(),
		}));
	let regions = sim.protected_regions();
	let r = digblock_core::unlock::ward_radius(level);
	let span = digblock_core::unlock::channel_ticks(level);
	let head = digblock_core::kindle::host_cell(sim.test_player(1).unwrap().pos);
	let (shell, interior) = digblock_core::ward::sets(head, r);
	let plan_len = ward_plan(&sim, 1, r).len() as u32;
	let was: Vec<Option<u16>> = shell
		.iter()
		.map(|&(x, y, z)| sim.test_cell_at(x, y, z))
		.collect();
	c.msgs.clear();
	cast(&mut sim);
	c.drain();
	assert!(
		!c.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::CastRefused { .. })),
		"the ward lands"
	);
	// The one frame the whole climb costs, carrying what an observer
	// needs to draw the same sphere at the same pace.
	assert_matches!(
		ward_frames(&c).as_slice(),
		[ServerMsg::WardWinding {
			by,
			head: at,
			radius,
			span: s,
		}] if Some(*by) == sim.test_player_id(1)
			&& *at == [head.0, head.1, head.2]
			&& i32::from(*radius) == r
			&& *s == span,
		"a cast announces its sphere once"
	);
	let glass = make_cell(BlockType::Wardglass, 0);
	// The glass CLIMBS through the channel: on the cast's own tick some
	// of the shell is glass and some still air, nothing is announced,
	// and a `CastBreak` puts every pane back and refunds the fare.
	let shell_glass = |sim: &super::Sim| {
		shell
			.iter()
			.filter(|&&(x, y, z)| sim.test_cell_at(x, y, z) == Some(glass))
			.count()
	};
	let first_tick = shell_glass(&sim);
	assert!(first_tick > 0, "nothing landed on the cast's tick");
	assert_eq!(
		sim.test_essence(1, essence),
		Some(0),
		"the fare is taken as the channel begins"
	);
	for _ in 1..span / 2 {
		sim.tick_once(vec![]);
	}
	let half = shell_glass(&sim);
	assert!(
		half > first_tick,
		"the shell never grew past the first tick"
	);
	c.drain();
	assert_eq!(
		ward_frames(&c).len(),
		1,
		"a half-climbed sphere has said nothing more"
	);
	assert_eq!(
		applied_edits(&c),
		0,
		"not one cell of the sphere travelled as an edit"
	);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::CastBreak,
	}]);
	assert_eq!(shell_glass(&sim), 0, "the break put every pane back");
	for (&(x, y, z), &before) in shell.iter().zip(&was) {
		assert_eq!(sim.test_cell_at(x, y, z), before, "restored at {x},{y},{z}");
	}
	assert_eq!(
		sim.test_essence(1, essence),
		Some(fare),
		"and the fare came back"
	);
	c.drain();
	let (landed, sealed) = last_ending(&c).expect("the break is announced");
	assert!(!sealed, "a broken channel did not seal");
	assert!(
		landed > 0 && landed < plan_len,
		"a break mid-climb reports the prefix it had reached: {landed} of {plan_len}"
	);
	assert_eq!(applied_edits(&c), 0, "and the unwind travelled as no edits");
	c.msgs.clear();

	// A step breaks it too, whatever the client says: the caster moved
	// further than the server's stillness allows, and the sim unwinds
	// on its own.
	cast(&mut sim);
	assert!(shell_glass(&sim) > 0, "the second cast climbs");
	let from = sim.test_player(1).unwrap().pos;
	sim.test_teleport(1, from + glam::Vec3::new(1.5, 0.0, 0.0));
	sim.tick_once(vec![]);
	assert_eq!(shell_glass(&sim), 0, "a step away unwound the climb");
	assert_eq!(
		sim.test_essence(1, essence),
		Some(fare),
		"with the fare back"
	);
	sim.test_teleport(1, from);
	c.drain();
	c.msgs.clear();

	// Left alone, the channel seals: every cell, then the announcement.
	cast(&mut sim);
	for _ in 1..span {
		sim.tick_once(vec![]);
	}
	c.drain();
	assert_matches!(
		ward_frames(&c).as_slice(),
		[
			ServerMsg::WardWinding { .. },
			ServerMsg::WardEnded {
				by,
				head: at,
				radius,
				landed,
				sealed: true,
			}
		] if Some(*by) == sim.test_player_id(1)
			&& *at == [head.0, head.1, head.2]
			&& i32::from(*radius) == r
			&& *landed == plan_len,
		"a sealed sphere is one winding and one ending, and the seal is the whole plan"
	);
	let (mut glassed, mut spared) = (0, 0);
	for (&(x, y, z), &before) in shell.iter().zip(&was) {
		let before = before.expect("a flat world is resident about spawn");
		let now = sim.test_cell_at(x, y, z).unwrap();
		if digblock_core::protect::immutable(x, y, z, &regions) {
			assert_eq!(now, before, "a locked cell stands at {x},{y},{z}");
			spared += 1;
		} else if cell_id(before) == BlockType::Air as u16 {
			assert_eq!(now, glass, "air in the shell at {x},{y},{z}");
			glassed += 1;
		} else {
			assert_eq!(now, before, "terrain in the shell stands");
		}
	}
	assert!(
		glassed > 0 && spared > 0,
		"the shell met neither air nor the lock — the sweep proved nothing"
	);
	for &(x, y, z) in &interior {
		assert_ne!(
			cell_id(sim.test_cell_at(x, y, z).unwrap()),
			BlockType::Wardglass as u16,
			"nothing inside is glassed"
		);
	}
	assert_eq!(
		applied_edits(&c),
		0,
		"the whole sphere reached the caster as two frames and no edits"
	);
	assert_eq!(sim.test_essence(1, essence), Some(0), "the fare left");
	assert_eq!(
		sim.test_skill_xp(1, digblock_core::skills::Skill::Arcana),
		digblock_core::skills::total_xp_for(level) + cast_xp(essence, fare)
	);
}

/// What a ward costs on the wire does not scale with the sphere. Every rank — the narrowest and
/// the widest the ladder can cast — spends the same frames to climb and the same to come apart,
/// because both ends derive the cells from the head and the radius. A per-cell broadcast at the
/// widest rank would be thousands of frames against an outbound queue whose refusal DROPS the
/// peer, so a caster could disconnect the server's population by breaking a channel.
#[test]
fn a_ward_costs_the_same_frames_at_every_rank() {
	let mut sim = flat_sim("wardframes");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let mut plans: Vec<usize> = Vec::new();
	let mut seal_cost: Vec<usize> = Vec::new();
	let mut break_cost: Vec<usize> = Vec::new();
	for (level, r) in ward_ranks() {
		sim.bench_set_arcana_level(1, level);
		wait_for_sphere(&mut sim, 1, r);
		let (essence, fare) = digblock_core::alchemy::ward_price(level);
		let span = digblock_core::unlock::channel_ticks(level);
		plans.push(ward_plan(&sim, 1, r).len());

		// A whole climb, to the seal.
		sim.test_grant_essence(1, essence, fare);
		c.drain();
		c.msgs.clear();
		cast_ward(&mut sim, 1);
		for _ in 1..span {
			sim.tick_once(vec![]);
		}
		c.drain();
		assert_eq!(applied_edits(&c), 0, "r={r}: the sphere sent no edits");
		assert_matches!(last_ending(&c), Some((_, true)), "r={r}: it sealed");
		seal_cost.push(ward_frames(&c).len());

		// And a climb broken halfway, which used to be the unbudgeted
		// one: the whole landed plan restored inside a single tick.
		sim.test_grant_essence(1, essence, fare);
		c.msgs.clear();
		cast_ward(&mut sim, 1);
		for _ in 1..span / 2 {
			sim.tick_once(vec![]);
		}
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::CastBreak,
		}]);
		c.drain();
		assert_eq!(applied_edits(&c), 0, "r={r}: the unwind sent no edits");
		assert_matches!(last_ending(&c), Some((_, false)), "r={r}: it unwound");
		break_cost.push(ward_frames(&c).len());
	}
	assert!(
		plans.windows(2).all(|w| w[1] > w[0]),
		"the ranks must differ in size or the sweep proves nothing: {plans:?}"
	);
	assert!(
		seal_cost.windows(2).all(|w| w[0] == w[1]),
		"a seal costs the same at every rank: {seal_cost:?} for plans {plans:?}"
	);
	assert!(
		break_cost.windows(2).all(|w| w[0] == w[1]),
		"an unwind costs the same at every rank: {break_cost:?} for plans {plans:?}"
	);
}

/// The shell closes on nobody: a second player standing where a pane
/// would land refuses the whole cast — the Arc's rule — before any
/// debit and before a single cell is written.
#[test]
fn a_ward_refuses_to_close_on_another_player() {
	use digblock_core::block::BlockType;
	use digblock_core::chunk::make_cell;
	use digblock_net::messages::CastRefusal;

	let mut sim = flat_sim("wardonplayer");
	let mut c = connect(&mut sim, 1);
	let mut other = connect(&mut sim, 2);
	wait_live(&mut sim, 1);
	wait_live(&mut sim, 2);
	let level = digblock_core::unlock::WARD_AT;
	let (essence, fare) = digblock_core::alchemy::ward_price(level);
	sim.bench_set_arcana_level(1, level);
	sim.test_grant_essence(1, essence, fare);
	let r = digblock_core::unlock::ward_radius(level);
	let head = digblock_core::kindle::host_cell(sim.test_player(1).unwrap().pos);
	let glass = make_cell(BlockType::Wardglass, 0);
	// Somewhere a pane would land on them — asked of the same predicate
	// the server asks, rather than guessed from the geometry.
	let (shell, _) = digblock_core::ward::sets(head, r);
	let stand = shell
		.iter()
		.copied()
		.map(|cell| {
			(
				cell,
				glam::Vec3::new(
					cell.0 as f32 + 0.5,
					cell.1 as f32 + 0.5,
					cell.2 as f32 + 0.5,
				),
			)
		})
		.find(|&(cell, pos)| digblock_core::player::placement_blocked(pos, cell, glass))
		.expect("a shell cell a player can stand in");
	sim.test_teleport(2, stand.1);
	c.msgs.clear();
	cast_ward(&mut sim, 1);
	c.drain();
	other.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::CastRefused {
				reason: CastRefusal::BadTarget
			}
		)),
		"a sphere that would close on somebody is refused whole"
	);
	assert_eq!(
		sim.test_essence(1, essence),
		Some(fare),
		"and costs nothing"
	);
	assert!(ward_frames(&c).is_empty(), "nothing began");
	assert_eq!(
		shell
			.iter()
			.filter(|&&(x, y, z)| sim.test_cell_at(x, y, z) == Some(glass))
			.count(),
		0,
		"not one pane landed"
	);
}

/// One ward per caster, and one fare per ward: a second cast while the
/// first climbs is refused with the first still climbing, and a second
/// `CastBreak` after the channel is already broken refunds nothing more.
#[test]
fn a_second_ward_is_refused_and_a_second_break_refunds_nothing() {
	use digblock_core::block::BlockType;
	use digblock_core::chunk::make_cell;
	use digblock_net::messages::CastRefusal;

	let mut sim = flat_sim("wardtwice");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let level = digblock_core::unlock::WARD_AT;
	let (essence, fare) = digblock_core::alchemy::ward_price(level);
	sim.bench_set_arcana_level(1, level);
	// Two fares in hand, so a second debit would be affordable — the
	// refusal has to be the ward, not the purse.
	sim.test_grant_essence(1, essence, fare * 2);
	let glass = make_cell(BlockType::Wardglass, 0);
	let head = digblock_core::kindle::host_cell(sim.test_player(1).unwrap().pos);
	let r = digblock_core::unlock::ward_radius(level);
	let (shell, _) = digblock_core::ward::sets(head, r);
	let shell_glass = |sim: &super::Sim| {
		shell
			.iter()
			.filter(|&&(x, y, z)| sim.test_cell_at(x, y, z) == Some(glass))
			.count()
	};
	c.msgs.clear();
	cast_ward(&mut sim, 1);
	let after_first = shell_glass(&sim);
	assert!(after_first > 0, "the first cast climbs");
	c.drain();
	c.msgs.clear();
	cast_ward(&mut sim, 1);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::CastRefused {
				reason: CastRefusal::BadTarget
			}
		)),
		"a second ward is refused while one is climbing"
	);
	assert!(
		ward_frames(&c).is_empty(),
		"and nothing about it reaches the wire"
	);
	assert_eq!(
		sim.test_essence(1, essence),
		Some(fare),
		"one fare taken, not two"
	);
	assert!(shell_glass(&sim) >= after_first, "the first still climbs");

	let breakit = |sim: &mut super::Sim| {
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::CastBreak,
		}]);
	};
	breakit(&mut sim);
	assert_eq!(shell_glass(&sim), 0, "the break put the climb back");
	assert_eq!(
		sim.test_essence(1, essence),
		Some(fare * 2),
		"the fare came back once"
	);
	c.msgs.clear();
	breakit(&mut sim);
	assert_eq!(
		sim.test_essence(1, essence),
		Some(fare * 2),
		"a second break refunds nothing"
	);
	assert!(
		ward_frames(&c).is_empty(),
		"and announces nothing — there is no ward to end"
	);
	// Nothing was stranded: the next cast is accepted.
	c.msgs.clear();
	cast_ward(&mut sim, 1);
	c.drain();
	assert!(
		!c.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::CastRefused { .. })),
		"a fresh cast is accepted once the last one is done"
	);
}

/// An unwind puts back what the climb WROTE, and only where its glass still stands: a cell someone
/// has since changed is left exactly as they left it. It is driven by the climb's own record rather
/// than by re-deriving the sphere, which is what makes that possible — a plan drawn against a
/// standing sphere names precisely the cells missing from it.
#[test]
fn an_unwind_leaves_a_cell_that_no_longer_holds_its_glass() {
	use digblock_core::block::BlockType;
	use digblock_core::chunk::make_cell;

	let mut sim = flat_sim("wardhole");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let level = digblock_core::unlock::WARD_AT;
	let (essence, fare) = digblock_core::alchemy::ward_price(level);
	sim.bench_set_arcana_level(1, level);
	sim.test_grant_essence(1, essence, fare);
	let glass = make_cell(BlockType::Wardglass, 0);
	let head = digblock_core::kindle::host_cell(sim.test_player(1).unwrap().pos);
	let r = digblock_core::unlock::ward_radius(level);
	let span = digblock_core::unlock::channel_ticks(level);
	let (shell, _) = digblock_core::ward::sets(head, r);
	let was: Vec<Option<u16>> = shell
		.iter()
		.map(|&(x, y, z)| sim.test_cell_at(x, y, z))
		.collect();
	cast_ward(&mut sim, 1);
	for _ in 1..span / 2 {
		sim.tick_once(vec![]);
	}
	// A pane that landed, replaced by somebody else's block. Air would
	// be the dig, and the same case — but a restored cell and a dug one
	// are both air, so the marker is what makes the difference visible.
	let (dug, _) = shell
		.iter()
		.zip(&was)
		.find(|&(&(x, y, z), _)| sim.test_cell_at(x, y, z) == Some(glass))
		.expect("the half-climb landed something");
	let marker = make_cell(BlockType::Stone, 0);
	sim.test_set_cell(dug.0, dug.1, dug.2, marker);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::CastBreak,
	}]);
	assert_eq!(
		sim.test_cell_at(dug.0, dug.1, dug.2),
		Some(marker),
		"the unwind left the cell it no longer owned"
	);
	for (&(x, y, z), &before) in shell.iter().zip(&was) {
		if (x, y, z) == *dug {
			continue;
		}
		assert_eq!(
			sim.test_cell_at(x, y, z),
			before,
			"every other pane came back at {x},{y},{z}"
		);
	}
	c.drain();
	assert_matches!(last_ending(&c), Some((_, false)));
}

/// A caster who LEAVES mid-channel breaks it: the climb unwinds on the
/// sim's own word, and the fare is forfeit — the refund is a correction
/// to a live client's predicted balance, and their record was written
/// on the way out.
#[test]
fn a_caster_who_leaves_mid_channel_forfeits_the_fare() {
	use digblock_core::block::BlockType;
	use digblock_core::chunk::make_cell;

	let mut sim = flat_sim("wardleave");
	let account = test_account(7);
	let mut c = connect_as_account(&mut sim, 1, account);
	wait_live(&mut sim, 1);
	let level = digblock_core::unlock::WARD_AT;
	let (essence, fare) = digblock_core::alchemy::ward_price(level);
	sim.bench_set_arcana_level(1, level);
	sim.test_grant_essence(1, essence, fare);
	let glass = make_cell(BlockType::Wardglass, 0);
	let head = digblock_core::kindle::host_cell(sim.test_player(1).unwrap().pos);
	let r = digblock_core::unlock::ward_radius(level);
	let span = digblock_core::unlock::channel_ticks(level);
	let (shell, _) = digblock_core::ward::sets(head, r);
	// Read through the JOURNAL: the last player leaving evicts every
	// column, so the store stops answering for the sphere the same tick
	// the unwind runs — and the journal is the copy a rejoin reads.
	let journalled_glass = |sim: &super::Sim| {
		shell
			.iter()
			.filter(|&&(x, y, z)| sim.journal_cell(x, y, z) == Some(glass))
			.count()
	};
	cast_ward(&mut sim, 1);
	for _ in 1..span / 2 {
		sim.tick_once(vec![]);
	}
	let landed = journalled_glass(&sim);
	assert!(landed > 0, "the climb had landed panes to unwind");
	c.drain();
	sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
	assert_eq!(
		journalled_glass(&sim),
		0,
		"a departure unwinds the climb out of the record too"
	);
	// The same account back again reads the record their departure
	// wrote: the fare stayed spent.
	let mut back = connect_as_account(&mut sim, 2, account);
	wait_live(&mut sim, 2);
	back.drain();
	assert_eq!(
		sim.test_essence(2, essence),
		Some(0),
		"a caster who left mid-channel is not refunded"
	);
	// And nothing is left climbing: the next cast is accepted.
	sim.test_grant_essence(2, essence, fare);
	sim.bench_set_arcana_level(2, level);
	back.msgs.clear();
	cast_ward(&mut sim, 2);
	back.drain();
	assert!(
		!back
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::CastRefused { .. })),
		"the departed ward left nothing behind"
	);
}

/// Claim a causeway plank for this connection, as its client would.
fn claim_plank(sim: &mut super::Sim, conn_id: u64, seq: u32, cell: (i32, i32, i32)) {
	let origin = sim
		.test_player(conn_id)
		.expect("a live client has a player")
		.pos;
	sim.tick_once(vec![SimEvent::Msg {
		conn_id,
		msg: ClientMsg::EditRequest {
			seq,
			tick: 0,
			origin: origin.to_array(),
			dir: [0.0, -1.0, 0.0],
			kind: digblock_net::messages::EditKind::CausewayStep,
			cell: [cell.0, cell.1, cell.2],
			normal: [0, 1, 0],
			slot: 0,
		},
	}]);
	// An edit rides the action queue, so it is applied on the tick after
	// the one that carried its message.
	sim.tick_once(vec![]);
}

/// Claim a plank and report the reason it was refused, or `None` if it
/// landed. Asserting the REASON is the point: "no glass appeared" passes
/// for every refusal there is, including ones the fixture caused, so a
/// bound could be deleted outright and a test that only looked at the
/// world would not notice.
fn claim_reason(
	sim: &mut super::Sim,
	c: &mut TestClient,
	seq: u32,
	cell: (i32, i32, i32),
) -> Option<RejectReason> {
	c.msgs.clear();
	claim_plank(sim, 1, seq, cell);
	c.drain();
	c.msgs.iter().find_map(|m| match m {
		ServerMsg::EditRejected { client_seq, reason } if *client_seq == seq => Some(*reason),
		_ => None,
	})
}

/// A cell only the claim BOUND can refuse: claimable, unoccupied and
/// loaded, but outside `CLAIM_SLACK` of the cell under the caster's
/// feet. Searched rather than assumed — the previous fixture computed one
/// by hand and got a cell the server had not loaded, so the refusal it
/// asserted was `NotLoaded` and the bound was never exercised at all.
fn out_of_bound_cell(sim: &super::Sim, conn_id: u64) -> (i32, i32, i32) {
	use digblock_core::causeway::{CLAIM_SLACK, claim_ok, claims, step_cell};
	let pos = sim
		.test_player(conn_id)
		.expect("a live client has a player")
		.pos;
	let base = step_cell(pos);
	let glass = digblock_core::chunk::make_cell(digblock_core::block::BlockType::Wardglass, 0);
	let reach = CLAIM_SLACK + 3;
	for dy in -reach..=reach {
		for dx in -reach..=reach {
			for dz in -reach..=reach {
				let c = (base.0 + dx, base.1 + dy, base.2 + dz);
				if claim_ok(c, pos) {
					continue;
				}
				let Some(v) = sim.test_cell_at(c.0, c.1, c.2) else {
					continue;
				};
				if claims(v) && !digblock_core::player::placement_blocked(pos, c, glass) {
					return c;
				}
			}
		}
	}
	panic!("no loaded, claimable cell sits outside the claim bound");
}

/// Stand this caster well clear of spawn's own lock sphere, and wait for
/// the ground under them to become resident. Every cell beside the pad is
/// protected, so a plank claimed there is refused for a reason that has
/// nothing to do with the Causeway.
fn stand_clear_of_spawn(sim: &mut super::Sim, conn_id: u64) {
	let pad = sim.test_player(conn_id).expect("live").pos;
	let away = pad + glam::Vec3::new(digblock_core::spawn::LOCK_RADIUS as f32 * 2.0, 0.0, 0.0);
	sim.test_teleport(conn_id, away);
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
	loop {
		sim.tick_once(vec![]);
		// The cell the caster's own feet name has to be RESIDENT, not
		// merely some cell near it: an unloaded one refuses a claim with
		// `NotLoaded`, which reads as any other refusal to a test that
		// only looks at the world.
		let pos = sim.test_player(conn_id).expect("live").pos;
		let base = digblock_core::causeway::step_cell(pos);
		if sim.test_cell_at(base.0, base.1, base.2).is_some()
			&& !claimable_near(sim, conn_id).is_empty()
		{
			return;
		}
		assert!(
			std::time::Instant::now() < deadline,
			"the ground under the caster never became resident"
		);
		std::thread::sleep(std::time::Duration::from_millis(1));
	}
}

/// A loaded, in-bound cell a plank may NOT claim — searched, for the same
/// reason [`out_of_bound_cell`] is.
fn occupied_in_bound_cell(sim: &super::Sim, conn_id: u64) -> (i32, i32, i32) {
	use digblock_core::causeway::{CLAIM_SLACK, claim_ok, claims, step_cell};
	let pos = sim.test_player(conn_id).expect("live").pos;
	let base = step_cell(pos);
	for dy in -CLAIM_SLACK..=CLAIM_SLACK {
		for dx in -CLAIM_SLACK..=CLAIM_SLACK {
			for dz in -CLAIM_SLACK..=CLAIM_SLACK {
				let c = (base.0 + dx, base.1 + dy, base.2 + dz);
				if !claim_ok(c, pos) {
					continue;
				}
				if sim.test_cell_at(c.0, c.1, c.2).is_some_and(|v| !claims(v)) {
					return c;
				}
			}
		}
	}
	panic!("every loaded cell in reach is claimable, so nothing tests the claim gate");
}

/// Cells this caster could honestly claim a plank in: within the claim
/// bound of the cell under their feet, holding something a plank may
/// claim, and not blocked by anybody standing there. Derived from the
/// same three predicates the server checks, so the fixture cannot drift
/// from the rule.
fn claimable_near(sim: &super::Sim, conn_id: u64) -> Vec<(i32, i32, i32)> {
	use digblock_core::causeway::{CLAIM_SLACK, claims, step_cell};
	let pos = sim
		.test_player(conn_id)
		.expect("a live client has a player")
		.pos;
	let base = step_cell(pos);
	let mut out = Vec::new();
	for dy in -CLAIM_SLACK..=CLAIM_SLACK {
		for dx in -CLAIM_SLACK..=CLAIM_SLACK {
			for dz in -CLAIM_SLACK..=CLAIM_SLACK {
				let c = (base.0 + dx, base.1 + dy, base.2 + dz);
				let Some(v) = sim.test_cell_at(c.0, c.1, c.2) else {
					continue;
				};
				let glass =
					digblock_core::chunk::make_cell(digblock_core::block::BlockType::Wardglass, 0);
				if claims(v) && !digblock_core::player::placement_blocked(pos, c, glass) {
					out.push(c);
				}
			}
		}
	}
	out
}

/// Every wardglass cell standing in a box around the caster — the road,
/// counted from the world rather than from the server's bookkeeping.
fn glass_around(sim: &super::Sim, at: (i32, i32, i32), r: i32) -> Vec<(i32, i32, i32)> {
	use digblock_core::block::BlockType;
	let mut out = Vec::new();
	for x in at.0 - r..=at.0 + r {
		for y in at.1 - r..=at.1 + r {
			for z in at.2 - r..=at.2 + r {
				let held = sim
					.test_cell_at(x, y, z)
					.map(|v| BlockType::from_id(digblock_core::chunk::cell_id(v)));
				if held == Some(BlockType::Wardglass) {
					out.push((x, y, z));
				}
			}
		}
	}
	out
}

/// The road stands at most `causeway::TRAIL` planks however many are
/// claimed, and the plank retired is the oldest — the property the
/// client's own assertion rests on, checked against the WORLD rather
/// than against the trail that decides it.
#[test]
fn a_road_never_stands_longer_than_its_trail() {
	use digblock_core::causeway::TRAIL;
	let mut sim = flat_sim("causeway_trail");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	stand_clear_of_spawn(&mut sim, 1);
	let level = digblock_core::unlock::CAUSEWAY_AT;
	sim.bench_set_arcana_level(1, level);
	let (essence, fare) = digblock_core::alchemy::causeway_price(level);
	sim.test_grant_essence(1, essence, fare);
	c.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Cast(digblock_net::messages::CastKind::Causeway),
	}]);
	assert_eq!(sim.test_essence(1, essence), Some(0), "the fare left");

	let base = digblock_core::causeway::step_cell(sim.test_player(1).expect("live").pos);
	let claims = claimable_near(&sim, 1);
	assert!(
		claims.len() > TRAIL,
		"the fixture offers no more than a full road to claim"
	);
	for (i, &cell) in claims.iter().enumerate() {
		claim_plank(&mut sim, 1, i as u32 + 1, cell);
		let standing = glass_around(&sim, base, digblock_core::causeway::CLAIM_SLACK + 2);
		assert_eq!(
			standing.len(),
			(i + 1).min(TRAIL),
			"claim {i} left {} planks standing",
			standing.len()
		);
	}
}

/// A plank is a claim, and the server bounds it three ways: no road
/// standing at all, a cell out of reach of the SERVER's own copy of the
/// feet, and a cell that is not the caster's to claim. Each refusal
/// leaves the world exactly as it was.
#[test]
fn a_claimed_plank_is_bounded_three_ways() {
	use digblock_core::causeway::CLAIM_SLACK;
	let mut sim = flat_sim("causeway_bounds");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	stand_clear_of_spawn(&mut sim, 1);
	let level = digblock_core::unlock::CAUSEWAY_AT;
	sim.bench_set_arcana_level(1, level);
	let pos = sim.test_player(1).expect("live").pos;
	let base = digblock_core::causeway::step_cell(pos);
	let honest = *claimable_near(&sim, 1)
		.first()
		.expect("somewhere to lay one");
	let r = CLAIM_SLACK + 2;

	// No road: an assertion into a spell that is not being cast.
	assert_eq!(
		claim_reason(&mut sim, &mut c, 1, honest),
		Some(RejectReason::BadBlock),
		"a plank was taken with no causeway behind it"
	);
	assert!(glass_around(&sim, base, r).is_empty());

	let (essence, fare) = digblock_core::alchemy::causeway_price(level);
	sim.test_grant_essence(1, essence, fare);
	c.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Cast(digblock_net::messages::CastKind::Causeway),
	}]);

	// Out of reach: one cell past the slack measured from BASE, which is what the bound compares
	// against — at a height and depth already known claimable, so nothing but the bound can refuse
	// it. The premise is asserted first: a cell the server has not loaded refuses with `NotLoaded`,
	// which would pass a weaker test for the wrong reason.
	let far = out_of_bound_cell(&sim, 1);
	assert!(
		!digblock_core::causeway::claim_ok(far, pos),
		"the far cell is inside the bound"
	);
	assert_eq!(
		claim_reason(&mut sim, &mut c, 2, far),
		Some(RejectReason::OutOfReach),
		"a plank was taken past the claim bound"
	);
	assert!(!glass_around(&sim, base, r + 4).contains(&far));

	// Not claimable: a loaded cell in reach that already holds something.
	let solid = occupied_in_bound_cell(&sim, 1);
	let before = sim.test_cell_at(solid.0, solid.1, solid.2);
	assert_eq!(
		claim_reason(&mut sim, &mut c, 3, solid),
		Some(RejectReason::Occluded),
		"a plank overwrote a cell that was not air"
	);
	assert_eq!(
		sim.test_cell_at(solid.0, solid.1, solid.2),
		before,
		"and left it as it found it"
	);

	// And the honest one lands, refused for nothing.
	assert_eq!(claim_reason(&mut sim, &mut c, 4, honest), None);
	assert!(
		glass_around(&sim, base, r).contains(&honest),
		"an honest plank was refused"
	);
}

/// The road comes up on its own clock, and on a departure — a trail
/// nobody holds could never retire itself. Both spans read off the
/// unlock table, never a written-down number.
#[test]
fn a_road_is_taken_up_by_its_span_and_by_leaving() {
	let mut sim = flat_sim("causeway_lifecycle");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	stand_clear_of_spawn(&mut sim, 1);
	let level = digblock_core::unlock::CAUSEWAY_AT;
	sim.bench_set_arcana_level(1, level);
	let (essence, fare) = digblock_core::alchemy::causeway_price(level);
	let secs = digblock_core::unlock::causeway_secs(level);

	let lay = |sim: &mut super::Sim, c: &mut TestClient| {
		sim.test_grant_essence(1, essence, fare);
		c.msgs.clear();
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Cast(digblock_net::messages::CastKind::Causeway),
		}]);
		let base = digblock_core::causeway::step_cell(sim.test_player(1).expect("live").pos);
		let cell = *claimable_near(sim, 1)
			.first()
			.expect("somewhere to lay one");
		claim_plank(sim, 1, 1, cell);
		assert!(!glass_around(sim, base, 4).is_empty(), "no road to take up");
		base
	};

	// The span: one tick past it, nothing stands.
	let base = lay(&mut sim, &mut c);
	for _ in 0..secs * digblock_net::protocol::TICK_RATE + 2 {
		sim.tick_once(vec![]);
	}
	assert!(
		glass_around(&sim, base, 4).is_empty(),
		"the road outlived its span"
	);

	// The departure. A SECOND client stands nearby and stays: with
	// nobody left the columns unload and `test_cell_at` answers None for
	// every cell, which would make this pass whatever the road did.
	let mut _c2 = connect(&mut sim, 2);
	wait_live(&mut sim, 2);
	let watcher = sim.test_player(1).expect("live").pos + glam::Vec3::new(6.0, 0.0, 0.0);
	sim.test_teleport(2, watcher);
	for _ in 0..8 {
		sim.tick_once(vec![]);
	}
	let base = lay(&mut sim, &mut c);
	assert!(
		!glass_around(&sim, base, 4).is_empty(),
		"the fixture lost its road before the departure"
	);
	sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
	assert!(
		sim.test_is_live(2),
		"the watcher left too, so the columns are gone and this proves nothing"
	);
	assert!(
		glass_around(&sim, base, 4).is_empty(),
		"a road outlived the caster who held it"
	);
}

/// Recasting takes the road up rather than paying twice — the row is a
/// toggle, because a caster who wants to descend cannot wait one out.
#[test]
fn recasting_takes_the_road_up_and_charges_nothing() {
	let mut sim = flat_sim("causeway_toggle");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	stand_clear_of_spawn(&mut sim, 1);
	let level = digblock_core::unlock::CAUSEWAY_AT;
	sim.bench_set_arcana_level(1, level);
	let (essence, fare) = digblock_core::alchemy::causeway_price(level);
	// Two fares in the purse, so a second debit would be payable and is
	// therefore a real thing to assert did not happen.
	sim.test_grant_essence(1, essence, fare * 2);
	let cast = |sim: &mut super::Sim| {
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Cast(digblock_net::messages::CastKind::Causeway),
		}]);
	};
	// The level was set by seeding a total, so the award is a DELTA.
	let arcana = digblock_core::skills::Skill::Arcana;
	let before = sim.test_skill_xp(1, arcana);
	c.msgs.clear();
	cast(&mut sim);
	let paid = digblock_core::skills::cast_xp(essence, fare);
	assert_eq!(sim.test_essence(1, essence), Some(fare));
	assert_eq!(sim.test_skill_xp(1, arcana), before + paid);
	let base = digblock_core::causeway::step_cell(sim.test_player(1).expect("live").pos);
	let cell = *claimable_near(&sim, 1)
		.first()
		.expect("somewhere to lay one");
	claim_plank(&mut sim, 1, 1, cell);
	assert!(!glass_around(&sim, base, 4).is_empty());

	cast(&mut sim);
	assert!(
		glass_around(&sim, base, 4).is_empty(),
		"a recast stood a second road instead of taking the first up"
	);
	assert_eq!(
		sim.test_essence(1, essence),
		Some(fare),
		"taking a road up charged a second fare"
	);
	assert_eq!(
		sim.test_skill_xp(1, arcana),
		before + paid,
		"taking a road up paid a second time"
	);
}

/// Tick until `done`, or fail saying what never happened. Streaming runs
/// on real worker threads, so this waits on the wall clock the way
/// [`stand_clear_of_spawn`] does rather than on a tick count.
fn tick_until(sim: &mut super::Sim, what: &str, done: impl Fn(&super::Sim) -> bool) {
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
	while !done(sim) {
		sim.tick_once(vec![]);
		assert!(std::time::Instant::now() < deadline, "{what}");
		std::thread::sleep(std::time::Duration::from_millis(1));
	}
}

/// Is this cell the road's own glass?
fn is_plank(sim: &super::Sim, c: (i32, i32, i32)) -> bool {
	sim.test_cell_at(c.0, c.1, c.2).is_some_and(|v| {
		digblock_core::block::BlockType::from_id(digblock_core::chunk::cell_id(v))
			== digblock_core::block::BlockType::Wardglass
	})
}

/// A plank whose column has gone is OWED, never forgotten. Only the trail can retire a plank and
/// only a resident column can be written, so a lift that found nowhere to write would drop the
/// trail AND leave the glass standing for good, journaled, with nothing left that could take it up.
/// Driven by carrying the caster out of residency, taking the road up, and bringing them back.
#[test]
fn a_plank_owed_to_an_unloaded_column_comes_up_when_it_returns() {
	let mut sim = flat_sim("causeway_owed");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	stand_clear_of_spawn(&mut sim, 1);
	let level = digblock_core::unlock::CAUSEWAY_AT;
	sim.bench_set_arcana_level(1, level);
	let (essence, fare) = digblock_core::alchemy::causeway_price(level);
	sim.test_grant_essence(1, essence, fare);
	c.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Cast(digblock_net::messages::CastKind::Causeway),
	}]);
	let home = sim.test_player(1).expect("live").pos;
	let cell = *claimable_near(&sim, 1)
		.first()
		.expect("somewhere to lay one");
	claim_plank(&mut sim, 1, 1, cell);
	assert!(is_plank(&sim, cell), "no plank to owe");

	// Out of residency entirely — the road's column is nobody's now.
	sim.test_teleport(1, home + glam::Vec3::new(2048.0, 0.0, 0.0));
	tick_until(&mut sim, "the road's column never unloaded", |s| {
		s.test_cell_at(cell.0, cell.1, cell.2).is_none()
	});
	// The road comes up with nowhere to write it.
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::CausewayEnd,
	}]);

	// And back. The column returns holding its journaled plank — which
	// is what makes this a real test and not a test of eviction.
	sim.test_teleport(1, home);
	tick_until(&mut sim, "the road's column never came back", |s| {
		s.test_cell_at(cell.0, cell.1, cell.2).is_some()
	});
	sim.tick_once(vec![]);
	assert!(
		!is_plank(&sim, cell),
		"a plank outlived the road that laid it"
	);
}

/// A teleport takes the road up — a road a world away is one nothing can
/// write — and keeps the SPAN, because the fare bought seconds and the
/// caster still has them: the road resumes under them where they land.
#[test]
fn a_teleport_takes_the_road_up_and_keeps_the_span() {
	let mut sim = flat_sim("causeway_teleport");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	stand_clear_of_spawn(&mut sim, 1);
	let level = digblock_core::unlock::CAUSEWAY_AT;
	sim.bench_set_arcana_level(1, level);
	let (essence, fare) = digblock_core::alchemy::causeway_price(level);
	sim.test_grant_essence(1, essence, fare);
	c.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Cast(digblock_net::messages::CastKind::Causeway),
	}]);
	let stood = sim.test_player(1).expect("live").pos;
	let cell = *claimable_near(&sim, 1)
		.first()
		.expect("somewhere to lay one");
	claim_plank(&mut sim, 1, 1, cell);
	assert!(is_plank(&sim, cell), "no road to leave behind");

	// Home, by the spell that does it — `test_teleport` moves the player
	// without going through the arrival the server runs.
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::ReturnToSpawn,
	}]);
	assert!(
		sim.test_player(1).expect("live").pos.distance(stood)
			> digblock_core::spawn::LOCK_RADIUS as f32,
		"the caster is still standing where the road is, so nothing was left behind"
	);
	assert!(
		!is_plank(&sim, cell),
		"a road stood on where the caster used to be"
	);

	// The span survived the trip: a plank still lands at the far end.
	// (Refused if `causeway_until` had been cleared — `BadBlock`.)
	stand_clear_of_spawn(&mut sim, 1);
	let landed = *claimable_near(&sim, 1)
		.first()
		.expect("somewhere to lay one");
	assert_eq!(
		claim_reason(&mut sim, &mut c, 2, landed),
		None,
		"the teleport ended the spell instead of taking the road up"
	);
	assert!(is_plank(&sim, landed), "the road did not resume");
}

/// A kindled light is the SIM's, not the caster's: it reaches a joiner
/// as a backlog entry counting down from the cast, after the caster has
/// left, and is gone from the backlog once its span has run — on the
/// sim's clock, with nobody there to watch it.
#[test]
fn a_kindled_light_outlives_its_caster_and_not_its_span() {
	use digblock_net::messages::CastKind;
	use digblock_net::protocol::TICK_RATE;
	let mut sim = flat_sim("kindleleave");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let level = digblock_core::unlock::KINDLE_AT;
	let (k_essence, k_fare) = digblock_core::alchemy::kindle_price(level);
	sim.test_grant_essence(1, k_essence, k_fare);
	sim.bench_set_arcana_level(1, level);
	let head = digblock_core::kindle::host_cell(sim.test_player(1).unwrap().pos);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Cast(CastKind::KindleHere),
	}]);
	c.drain();
	// The caster leaves; a little of the span passes with nobody on.
	sim.tick_once(vec![SimEvent::Disconnected { conn_id: 1 }]);
	let span = digblock_core::kindle::SECS * TICK_RATE;
	for _ in 0..TICK_RATE {
		sim.tick_once(vec![]);
	}
	let mut joiner = connect(&mut sim, 2);
	wait_live(&mut sim, 2);
	joiner.drain();
	let backlog: Vec<([i32; 3], u16)> = joiner
		.msgs
		.iter()
		.filter_map(|m| match m {
			ServerMsg::Kindled { cell, secs_left } => Some((*cell, *secs_left)),
			_ => None,
		})
		.collect();
	assert_eq!(
		backlog.len(),
		1,
		"the departed caster's light is handed over"
	);
	assert_eq!(backlog[0].0, [head.0, head.1, head.2]);
	assert!(
		u32::from(backlog[0].1) < digblock_core::kindle::SECS,
		"the joiner's copy is already counting down"
	);
	// Run the rest of the span out, then a third arrival finds nothing.
	for _ in 0..span {
		sim.tick_once(vec![]);
	}
	let mut late = connect(&mut sim, 3);
	wait_live(&mut sim, 3);
	late.drain();
	assert!(
		!late
			.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::Kindled { .. })),
		"a spent light is not handed to anyone"
	);
}

/// The resolved sites ride join sync, right behind Skills — the client
/// cannot derive them, and the spellbook's roster is empty without them.
#[test]
fn waystones_ride_join_sync() {
	let mut sim = flat_sim("wsync");
	sim.bench_add_waystone(2, FLAT_SPAWN + glam::Vec3::new(150.0, 0.0, 0.0));
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	c.drain();
	let entries = c
		.msgs
		.iter()
		.find_map(|m| match m {
			ServerMsg::Waystones { entries } => Some(entries.clone()),
			_ => None,
		})
		.expect("join sync carries the waystones");
	assert!(entries.iter().any(|&(k, _)| k == 2));
}

/// The two real gates of a port, in refusal order — the unlock, then
/// the fare (refused BEFORE any debit) — and the pay: fare-derived cast
/// XP, the same on every trip including the first, with every
/// expectation read off the core tables.
#[test]
fn a_port_validates_the_unlock_and_the_fare_and_pays_its_fare_every_trip() {
	use digblock_core::skills::{cast_xp, total_xp_for};
	use digblock_core::worldgen::SurfaceKind;
	use digblock_net::messages::{CastKind, CastRefusal};
	let kind = SurfaceKind::Plains;
	let idx = SurfaceKind::ALL
		.iter()
		.position(|&k| k == kind)
		.expect("plains is a kind") as u8;
	let port_level = digblock_core::unlock::waystone(kind).expect("plains has a port");
	let (essence, fare) = digblock_core::alchemy::cast_price(kind).expect("and a fare");

	let mut sim = flat_sim("port");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let dest = FLAT_SPAWN + glam::Vec3::new(200.0, 0.0, 0.0);
	sim.bench_add_waystone(idx, dest);
	let cast = |sim: &mut super::Sim| {
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Cast(CastKind::Waystone(idx)),
		}]);
	};

	// Below the port's level: refused as locked, unmoved, undebited.
	cast(&mut sim);
	c.drain();
	assert!(c.msgs.iter().any(|m| matches!(
		m,
		ServerMsg::CastRefused {
			reason: CastRefusal::Locked
		}
	)));
	assert!(sim.test_player(1).unwrap().pos.distance(dest) > 100.0);

	// Unlocked but broke: refused for the fare, BEFORE any debit.
	sim.bench_set_arcana_level(1, port_level);
	c.msgs.clear();
	cast(&mut sim);
	c.drain();
	assert!(c.msgs.iter().any(|m| matches!(
		m,
		ServerMsg::CastRefused {
			reason: CastRefusal::CantPay
		}
	)));

	// Funded for two trips: each pays the fare's worth, the first no
	// differently from the second.
	sim.test_grant_essence(1, essence, fare * 2);
	c.msgs.clear();
	cast(&mut sim);
	c.drain();
	assert!(sim.test_player(1).unwrap().pos.distance(dest) < 3.0);
	let total_after = |c: &TestClient| {
		c.msgs
			.iter()
			.filter_map(|m| match m {
				ServerMsg::XpGain { skill, total }
					if *skill == digblock_core::skills::Skill::Arcana as u8 =>
				{
					Some(*total)
				}
				_ => None,
			})
			.next_back()
	};
	assert_eq!(
		total_after(&c),
		Some(total_xp_for(port_level) + cast_xp(essence, fare)),
		"the first arrival pays the fare's worth"
	);
	// The second trip waits out the shared teleport clock — its own
	// contract is `a_port_and_a_return_share_one_cooldown`; here it is
	// only what stands between one arrival and the next.
	c.msgs.clear();
	for _ in 0..(digblock_net::protocol::TELEPORT_COOLDOWN_SECS * digblock_net::protocol::TICK_RATE)
	{
		sim.tick_once(vec![]);
		c.drain();
	}
	c.msgs.clear();
	cast(&mut sim);
	c.drain();
	assert_eq!(
		total_after(&c),
		Some(total_xp_for(port_level) + cast_xp(essence, fare) * 2),
		"a second arrival pays exactly the same"
	);
	// And the fare really left: two trips, two fares.
	assert_eq!(
		sim.test_essence(1, essence),
		Some(0),
		"both fares debited exactly"
	);
}

/// A site stamp lands once and converges: every cell the pure builder
/// names holds its value afterwards, and a second stamp broadcasts
/// nothing at all — boot after boot is a no-op, not a journal leak.
#[test]
fn a_site_stamp_lands_once_and_converges() {
	let mut sim = flat_sim("stamp");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	c.drain();
	c.msgs.clear();
	sim.stamp_site(FLAT_SPAWN);
	sim.tick_once(vec![]);
	c.drain();
	let landed = c
		.msgs
		.iter()
		.filter(|m| matches!(m, ServerMsg::EditApplied { .. }))
		.count();
	assert!(landed > 0, "the stamp wrote nothing");
	for (x, y, z, v) in digblock_core::waystones::stamp(FLAT_SPAWN) {
		assert_eq!(sim.test_cell_at(x, y, z), Some(v), "cell ({x},{y},{z})");
	}
	// Idempotence: the journal already holds every value.
	c.msgs.clear();
	sim.stamp_site(FLAT_SPAWN);
	sim.tick_once(vec![]);
	c.drain();
	assert_eq!(
		c.msgs
			.iter()
			.filter(|m| matches!(m, ServerMsg::EditApplied { .. }))
			.count(),
		0,
		"a re-stamp re-broadcast cells it already owns"
	);
}

/// The cast chokepoint: a home jump pays Arcana by the DISTANCE it
/// covered (every figure derived from `skills::home_xp`), and the gate
/// widens to the caster's own rung — a request inside the base cooldown
/// but past the swift one teleports.
#[test]
fn a_home_cast_pays_its_distance_and_the_gate_reads_the_rung() {
	use digblock_core::skills::{Skill, home_xp, total_xp_for};
	use digblock_core::unlock::{TELEPORT_SWIFT_AT, teleport_cooldown_secs};
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
	sim.bench_set_arcana_level(1, TELEPORT_SWIFT_AT);
	let base_total = total_xp_for(TELEPORT_SWIFT_AT);

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
	for _ in 0..(teleport_cooldown_secs(TELEPORT_SWIFT_AT) * TICK_RATE) {
		sim.tick_once(vec![]);
		c.drain();
	}
	assert!(teleport_cooldown_secs(TELEPORT_SWIFT_AT) < teleport_cooldown_secs(1));
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

/// A slash line prefixed with an invisible character is a COMMAND, not chat. Deciding `is_command`
/// on the raw text while the handler runs on the sanitized one splits on `U+200B`: it is not
/// `White_Space` (so `trim_start` leaves it) and not inside `TEXT_RANGES` (so the sanitizer removes
/// it), and the line is then BROADCAST as ordinary chat — a non-admin publishing a command, or an
/// admin leaking one they meant to run.
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

/// One clock, both spells: a port arms the same cooldown Return Home
/// does and waits on it, so a player cannot chain hops by alternating
/// them. Both orders are swept — a per-spell timer leaves exactly one
/// direction open, and the bug is invisible from the other.
#[test]
fn a_port_and_a_return_share_one_cooldown() {
	use digblock_core::worldgen::SurfaceKind;
	use digblock_net::messages::CastKind;
	use digblock_net::protocol::{TELEPORT_COOLDOWN_SECS, TICK_RATE};
	let kind = SurfaceKind::Plains;
	let idx = SurfaceKind::ALL
		.iter()
		.position(|&k| k == kind)
		.expect("plains is a kind") as u8;
	let (essence, fare) = digblock_core::alchemy::cast_price(kind).expect("a port has a fare");
	let port_level = digblock_core::unlock::waystone(kind).expect("plains has a port");

	for port_first in [true, false] {
		let mut sim = flat_sim(if port_first { "cdport" } else { "cdhome" });
		let mut c = connect(&mut sim, 1);
		wait_live(&mut sim, 1);
		let dest = FLAT_SPAWN + glam::Vec3::new(300.0, 0.0, 0.0);
		sim.bench_add_waystone(idx, dest);
		sim.bench_set_arcana_level(1, port_level);
		// Two fares, so nothing the cooldown blocks can be blamed on
		// an empty purse.
		sim.test_grant_essence(1, essence, fare * 2);
		let port = |sim: &mut super::Sim| {
			sim.tick_once(vec![SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::Cast(CastKind::Waystone(idx)),
			}]);
		};
		let home = |sim: &mut super::Sim| {
			sim.tick_once(vec![SimEvent::Msg {
				conn_id: 1,
				msg: ClientMsg::ReturnToSpawn,
			}]);
		};

		// The first teleport lands and arms the clock.
		if port_first {
			port(&mut sim);
			assert!(sim.test_player(1).unwrap().pos.distance(dest) < 3.0);
		} else {
			// Walk off spawn so a return has somewhere to come back from.
			for t in 100..340 {
				sim.tick_once(vec![batch(vec![input(t, true, no_claim())], 1)]);
			}
			home(&mut sim);
			assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) < 1.0);
		}
		c.drain();

		// The OTHER spell, immediately: refused by the shared clock, and
		// refused before any debit.
		let at = sim.test_player(1).unwrap().pos;
		let purse = sim.test_essence(1, essence);
		if port_first {
			home(&mut sim);
		} else {
			port(&mut sim);
		}
		assert!(
			sim.test_player(1).unwrap().pos.distance(at) < 1.0,
			"the second teleport ignored the shared cooldown"
		);
		assert_eq!(
			sim.test_essence(1, essence),
			purse,
			"a teleport refused by the clock must not charge a fare"
		);

		// Past the window it works.
		for _ in 0..(TELEPORT_COOLDOWN_SECS * TICK_RATE) {
			sim.tick_once(vec![]);
			c.drain();
		}
		if port_first {
			home(&mut sim);
			assert!(sim.test_player(1).unwrap().pos.distance(FLAT_SPAWN) < 1.0);
		} else {
			port(&mut sim);
			assert!(sim.test_player(1).unwrap().pos.distance(dest) < 3.0);
		}
	}
}

/// Conjured light is bounded in both directions a caster could push it. A second kindle in an
/// occupied cell REFRESHES that light rather than standing another, so the list counts lit PLACES;
/// past `MAX_KINDLED` places a fresh one is refused before any debit. Both matter for the same
/// reason: the list is a backlog every joiner is handed, and an unbounded one is a server nobody
/// new can enter — so the joiner at the end of this walks in with the world at its ceiling.
#[test]
fn kindled_lights_are_capped_and_one_to_a_cell() {
	use digblock_net::messages::{CastKind, CastRefusal};
	use digblock_net::protocol::TICK_RATE;

	let mut sim = flat_sim("kindlecap");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let level = digblock_core::unlock::KINDLE_AT;
	sim.bench_set_arcana_level(1, level);
	let (essence, fare) = digblock_core::alchemy::kindle_price(level);
	let cap = super::spawn::MAX_KINDLED;
	sim.test_grant_essence(1, essence, fare * (cap as u32 + 2));
	// Ticks between casts, so the cast bucket is never the thing under
	// test here — derived from its own refill rate.
	let spacing = TICK_RATE / super::spawn::CAST_RATE_PER_SEC;
	// A distinct air cell per light, all inside the resident column: the
	// caster's HEAD is the host, so standing somewhere new is what makes
	// a new light.
	let stand = |i: usize| {
		glam::Vec3::new(
			(i % 16) as f32 + 0.5,
			(FLAT_GROUND_TOP + 1 + i / 16) as f32 + 0.5,
			8.5,
		)
	};
	let kindle = |sim: &mut super::Sim, c: &mut TestClient, at: glam::Vec3| {
		sim.test_teleport(1, at);
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Cast(CastKind::KindleHere),
		}]);
		for _ in 0..spacing {
			sim.tick_once(vec![]);
			c.drain();
		}
		c.drain();
	};
	let latest_span = |c: &TestClient| {
		c.msgs.iter().rev().find_map(|m| match m {
			ServerMsg::Kindled { secs_left, .. } => Some(u32::from(*secs_left)),
			_ => None,
		})
	};

	// One cell, twice: one light, its clock wound back to the full span.
	kindle(&mut sim, &mut c, stand(0));
	assert_eq!(sim.test_kindled_len(), 1);
	for _ in 0..TICK_RATE {
		sim.tick_once(vec![]);
		c.drain();
	}
	c.msgs.clear();
	kindle(&mut sim, &mut c, stand(0));
	assert_eq!(
		sim.test_kindled_len(),
		1,
		"a second cast in one cell stands no second light"
	);
	assert_eq!(
		latest_span(&c),
		Some(digblock_core::kindle::SECS),
		"it refreshes the one that was there"
	);

	// Fill the rest of the ceiling, then ask for one more place.
	for i in 1..cap {
		kindle(&mut sim, &mut c, stand(i));
	}
	assert_eq!(sim.test_kindled_len(), cap, "the world is at its ceiling");
	let left = sim.test_essence(1, essence).expect("a live caster");
	c.msgs.clear();
	kindle(&mut sim, &mut c, stand(cap));
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::CastRefused {
				reason: CastRefusal::BadTarget
			}
		)),
		"a light with nowhere in the list to stand is refused"
	);
	assert_eq!(sim.test_kindled_len(), cap, "and stands nothing");
	assert_eq!(
		sim.test_essence(1, essence),
		Some(left),
		"a refusal costs nothing"
	);
	// A refresh still works at the ceiling — it takes no room.
	c.msgs.clear();
	kindle(&mut sim, &mut c, stand(0));
	assert_eq!(sim.test_kindled_len(), cap);
	assert_eq!(latest_span(&c), Some(digblock_core::kindle::SECS));

	// The join this used to fail: a full world's backlog reaches a
	// joiner through the sync pump, a few frames a tick, instead of
	// racing its handshake into the outbound queue.
	let mut joiner = connect(&mut sim, 2);
	wait_live(&mut sim, 2);
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
	let backlog = |j: &TestClient| {
		j.msgs
			.iter()
			.filter(|m| matches!(m, ServerMsg::Kindled { .. }))
			.count()
	};
	loop {
		joiner.drain();
		if backlog(&joiner) >= cap {
			break;
		}
		assert!(
			std::time::Instant::now() < deadline,
			"the joiner never got the standing lights: {} of {cap}",
			backlog(&joiner)
		);
		sim.tick_once(vec![]);
	}
	assert_eq!(backlog(&joiner), cap, "every standing light, exactly once");
	assert!(
		sim.test_is_live(2),
		"and the joiner survived being handed them"
	);
}
