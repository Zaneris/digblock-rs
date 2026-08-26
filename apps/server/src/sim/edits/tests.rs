use super::super::harness::*;
use super::super::{Sim, SimEvent};
use digblock_core::block::BlockType;
use digblock_core::raycast::{REACH, raycast};
use digblock_net::messages::{ClientMsg, EditKind, RejectReason, ServerMsg};
use glam::Vec3;

#[test]
fn edits_validate_apply_and_journal() {
	let mut sim = flat_sim("edits");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let pos = sim.test_player(1).unwrap().pos;

	// The ground block straight down is in reach.
	let down = Vec3::NEG_Y;
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z))
		.expect("ground under spawn");

	let dig = |seq: u32, origin: Vec3, cell: (i32, i32, i32)| SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq,
			tick: 100,
			origin: origin.to_array(),
			dir: down.to_array(),
			kind: EditKind::Dig,
			cell: [cell.0, cell.1, cell.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	};

	// 1. Wrong cell → Occluded.
	sim.tick_once(vec![dig(1, pos, (hit.cell.0 + 1, hit.cell.1, hit.cell.2))]);
	// 2. Origin displaced past `EDIT_POS_TOLERANCE` → OutOfReach.
	sim.tick_once(vec![dig(2, pos + Vec3::new(3.0, 0.0, 0.0), hit.cell)]);
	// 3. Honest dig with the full timed hold → applied.
	let mut t = 0;
	let dug = dig_hold(&mut sim, 1, down, 90, 3, EditKind::Dig, &mut t);
	assert_eq!(dug, hit.cell);
	c.drain();

	let rejected: Vec<(u32, RejectReason)> = c
		.msgs
		.iter()
		.filter_map(|m| match m {
			ServerMsg::EditRejected { client_seq, reason } => Some((*client_seq, *reason)),
			_ => None,
		})
		.collect();
	assert!(
		rejected.contains(&(1, RejectReason::Occluded)),
		"{rejected:?}"
	);
	assert!(
		rejected.contains(&(2, RejectReason::OutOfReach)),
		"{rejected:?}"
	);
	let applied = c.msgs.iter().find_map(|m| match m {
		ServerMsg::EditApplied {
			client_seq, edit, ..
		} if *client_seq == 3 => Some(*edit),
		_ => None,
	});
	let edit = applied.expect("dig 3 applied");
	assert_eq!((edit.x, edit.y, edit.z), hit.cell);
	assert_eq!(edit.block, BlockType::Air as u16);
	assert_eq!(
		sim.test_block_at(edit.x, edit.y, edit.z),
		Some(BlockType::Air)
	);
	assert!(
		sim.journal
			.index
			.column(edit.x.div_euclid(16), edit.z.div_euclid(16))
			.is_some_and(|col| col.get(&(edit.x, edit.y, edit.z)) == Some(&(BlockType::Air as u16)))
	);
}

/// Nothing at all writes the world floor — every action the wire can carry, driven against it in
/// one world. `dig_hardness(BEDROCK_Y) == None` proves nothing about the call sites, because most
/// writes that reach the floor are DERIVED rather than aimed — a pick's plane, a door's second
/// half, a placement one cell PAST the ray, fluid falling — so each seam has to refuse on its own.
/// The floor is protected POSITIONALLY, not by block identity, so the phases stand a slab in it.
#[test]
fn nothing_writes_the_world_floor() {
	use digblock_core::block::BEDROCK_Y as FLOOR;

	/// Did the server refuse something as `Protected` since the last
	/// clear? Both shapes count: `DigRefused` answers a hold that is
	/// never allowed to open, `EditRejected` answers a write that was
	/// assembled and then thrown out, and the phases below land on
	/// different ones.
	fn said_protected(c: &TestClient) -> bool {
		c.msgs.iter().any(|m| {
			matches!(
				m,
				ServerMsg::EditRejected {
					reason: RejectReason::Protected,
					..
				} | ServerMsg::DigRefused {
					reason: RejectReason::Protected,
					..
				}
			)
		})
	}

	let mut sim = bedrock_sim("floorpin");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let pos = sim.test_player(1).unwrap().pos;
	let (sx, sz) = (pos.x.floor() as i32, pos.z.floor() as i32);
	let mut t = 0;

	// ---- 1. an ordinary hold-to-dig on the floor --------------------
	// Refused when the hold STARTS, so the player is never left holding
	// the button through a full timer for nothing.
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let dug = dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
	assert_eq!(dug.1, FLOOR, "the ray has to reach the floor layer");
	c.drain();
	assert!(said_protected(&c), "a plain dig: {:?}", c.msgs);
	assert_eq!(
		sim.test_block_at(dug.0, dug.1, dug.2),
		Some(BlockType::Stone),
		"a plain dig broke the floor"
	);
	c.msgs.clear();

	// ---- 2. a PLACE whose target lands in the floor plane ------------
	// A placement writes `hit + normal`, and every ray from open air meets the floor's TOP face,
	// whose normal points away from it — so the only exposure is a SIDE face, and that takes a hole
	// beside the cell. Stage one and shoot through it at the wall it exposes: the target is then
	// squarely a floor cell, and the protected-write scan is all that is left before the store.
	let hole = (sx - 1, FLOOR, sz);
	let wall = (sx - 2, FLOOR, sz);
	sim.test_set_cell(hole.0, hole.1, hole.2, BlockType::Air as u16);
	sim.test_grant(1, BlockType::Stone, 1);
	let dir = (Vec3::new(hole.0 as f32, FLOOR as f32 + 0.5, sz as f32 + 0.5) - pos).normalize();
	let hit = raycast(pos, dir, REACH, &|x, y, z| sim.test_cell_at(x, y, z))
		.expect("a wall through the hole");
	assert_eq!(
		(hit.cell, hit.normal),
		(wall, (1, 0, 0)),
		"the shot has to reach a SIDE face of the floor or this phase is vacuous"
	);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 2,
			tick: 0,
			origin: pos.to_array(),
			dir: dir.to_array(),
			kind: EditKind::Place {
				block: BlockType::Stone as u16,
			},
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(said_protected(&c), "a place into the floor: {:?}", c.msgs);
	assert_eq!(
		sim.test_block_at(hole.0, hole.1, hole.2),
		Some(BlockType::Air),
		"a placement landed in the floor plane"
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Stone),
		1,
		"stock spent on a refused place"
	);
	c.msgs.clear();

	// ---- 3. a door pair straddling the floor -------------------------
	// A door is ONE object across two cells, so both writes derive from whichever half the ray
	// found. Stand one with its lower half IN the floor and work the UPPER, unprotected half twice,
	// a toggle then a dig. Each assembles a write for the partner in the floor plane, and each has
	// to refuse the whole action rather than half-applying it.
	let upper = (sx, FLOOR + 1, sz - 3);
	let lower = (upper.0, FLOOR, upper.2);
	let lower_cell = make_cell(
		BlockType::LumberDoor,
		door_state(Facing::PosZ, false, false),
	);
	let upper_cell = make_cell(BlockType::LumberDoor, door_state(Facing::PosZ, false, true));
	sim.test_set_cell(lower.0, lower.1, lower.2, lower_cell);
	sim.test_set_cell(upper.0, upper.1, upper.2, upper_cell);
	// A closed PosZ leaf hugs the low-z face of its cell, so aim just
	// inside it; the player stands on the high-z side.
	let door_dir = (Vec3::new(
		upper.0 as f32 + 0.5,
		upper.1 as f32 + 0.5,
		upper.2 as f32 + 0.05,
	) - pos)
		.normalize();
	let door_hit = raycast(pos, door_dir, REACH, &|x, y, z| sim.test_cell_at(x, y, z))
		.expect("the upper leaf");
	assert_eq!(door_hit.cell, upper, "the shot has to find the upper half");
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 3,
			tick: 0,
			origin: pos.to_array(),
			dir: door_dir.to_array(),
			kind: EditKind::Interact,
			cell: [upper.0, upper.1, upper.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(said_protected(&c), "a door toggle: {:?}", c.msgs);
	assert_eq!(
		sim.test_cell_at(lower.0, lower.1, lower.2),
		Some(lower_cell),
		"a toggle swung the half standing in the floor"
	);
	assert_eq!(
		sim.test_cell_at(upper.0, upper.1, upper.2),
		Some(upper_cell),
		"nothing partially toggles"
	);
	c.msgs.clear();

	let door_hold = BlockType::LumberDoor.hardness_ticks().unwrap() as u32 + 6;
	dig_hold(&mut sim, 1, door_dir, door_hold, 4, EditKind::Dig, &mut t);
	c.drain();
	assert!(said_protected(&c), "a door dig: {:?}", c.msgs);
	assert_eq!(
		sim.test_cell_at(lower.0, lower.1, lower.2),
		Some(lower_cell),
		"digging the free half cleared its partner out of the floor"
	);
	assert_eq!(
		sim.test_cell_at(upper.0, upper.1, upper.2),
		Some(upper_cell),
		"nothing partially breaks"
	);
	c.msgs.clear();

	// ---- 4. a bucket scoop out of the floor --------------------------
	// A Collect empties its cell as surely as a dig does, and picks its target with the FLUID ray
	// rather than the plain one — a second entry into the same gate. Whether this player owns a
	// vessel at all never comes up, because protection answers at DigStart, ahead of every tool
	// question.
	let spring = (sx + 2, FLOOR, sz);
	sim.test_set_cell(spring.0, spring.1, spring.2, BlockType::Water as u16);
	let scoop_dir = (Vec3::new(
		spring.0 as f32 + 0.5,
		FLOOR as f32 + 0.6,
		spring.2 as f32 + 0.5,
	) - pos)
		.normalize();
	let scoop_hit = digblock_core::raycast::raycast_fluid(pos, scoop_dir, REACH, &|x, y, z| {
		sim.test_cell_at(x, y, z)
	})
	.expect("the spring");
	assert_eq!(scoop_hit.cell, spring, "the fluid ray has to stop in it");
	// The hold length is immaterial — the refusal lands at DigStart, so
	// no hold is ever recorded to complete against.
	dig_hold(&mut sim, 1, scoop_dir, 20, 5, EditKind::Collect, &mut t);
	c.drain();
	assert!(said_protected(&c), "a scoop: {:?}", c.msgs);
	assert_eq!(
		sim.test_block_at(spring.0, spring.1, spring.2),
		Some(BlockType::Water),
		"a scoop emptied a floor cell"
	);
	c.msgs.clear();

	// ---- 5. the Resonant Pick's plane --------------------------------
	// The one phase whose action must SUCCEED. A full-plane swing at the lower cell of a wall
	// standing on the floor puts the floor row squarely in the plane, holding the very same Stone
	// id, so only `pick::breaks_as_extra`'s own y test keeps it out. FILTERING is what is pinned,
	// NOT refusal: refusing the whole swing stops the pick working at the bottom of every mine.
	let px = sx + 3;
	for y in [FLOOR + 1, FLOOR + 2] {
		for z in (sz - 1)..=(sz + 1) {
			sim.test_set_cell(px, y, z, BlockType::Stone as u16);
		}
	}
	sim.bench_set_mining_level(1, digblock_core::unlock::PICK_PLANE_AT);
	sim.bench_set_active_tool(1, BlockType::ResonantPick);
	let face = (px, FLOOR + 1, sz);
	let swing = (Vec3::new(px as f32, FLOOR as f32 + 1.5, sz as f32 + 0.5) - pos).normalize();
	let swing_hit = raycast(pos, swing, REACH, &|x, y, z| sim.test_cell_at(x, y, z))
		.expect("the wall's lower cell");
	assert_eq!(
		(swing_hit.cell, swing_hit.normal),
		(face, (-1, 0, 0)),
		"the swing has to enter through a vertical face"
	);
	assert!(
		digblock_core::pick::cells(
			swing_hit.cell,
			swing_hit.normal,
			swing.to_array(),
			digblock_core::unlock::pick_cells(digblock_core::unlock::PICK_PLANE_AT),
			|_| true,
		)
		.contains(&(px, FLOOR, sz)),
		"the plane has to straddle the floor or this phase is vacuous"
	);
	let swing_hold = BlockType::Stone.hardness_ticks().unwrap() as u32 * 3 + 16;
	dig_hold(&mut sim, 1, swing, swing_hold, 6, EditKind::Dig, &mut t);
	c.drain();
	assert_eq!(
		sim.test_cell_at(face.0, face.1, face.2),
		Some(0),
		"the swing itself has to land, or the phase proves nothing: {:?}",
		c.msgs
	);
	for z in (sz - 1)..=(sz + 1) {
		assert_eq!(
			sim.test_block_at(px, FLOOR, z),
			Some(BlockType::Stone),
			"the pick's plane reached the floor at z = {z}"
		);
	}
	c.msgs.clear();

	// ---- 6. fluid falling into a hole in the floor -------------------
	// The write nobody requested. `water_cannot_flow_into_a_locked_region`
	// covers the region half of the same predicate; this covers the
	// floor half, which a lock test cannot — drop the `y <= WORLD_FLOOR`
	// clause from `protect::immutable` and that test still passes.
	let sink = (sx, FLOOR, sz + 3);
	let src = (sink.0 - 1, FLOOR + 1, sink.2);
	sim.test_set_cell(sink.0, sink.1, sink.2, BlockType::Air as u16);
	assert!(
		sim.store
			.set_block(src.0, src.1, src.2, BlockType::Water)
			.is_some()
	);
	sim.fluid.schedule_around(src.0, src.1, src.2);
	for _ in 0..240 {
		sim.tick_once(vec![]);
	}
	// The source rests on solid floor, so it spreads sideways and the
	// flow arrives OVER the hole — which is what makes the next
	// assertion about the floor rather than about a sim that never ran.
	assert!(
		sim.test_block_at(sink.0, sink.1 + 1, sink.2)
			.is_some_and(BlockType::is_water),
		"fluid never reached the hole; the phase proves nothing"
	);
	assert_eq!(
		sim.test_block_at(sink.0, sink.1, sink.2),
		Some(BlockType::Air),
		"water fell into the world floor"
	);

	// ---- 7. the extraction seam --------------------------------------
	// A field extraction is TIMED and CREDITED as the ore, so the
	// completion gate asks `dig_hardness` of a block that is not the one
	// in the world (`ore::dig_block`). That composition is the one gate
	// no staged scene can reach — ore is derived from position and
	// `deposit_in` keeps every blob clear of the floor, so there is no
	// vein in the floor plane to aim at — and it is exactly the seam
	// where a cell holding Stone could be dug on another block's
	// hardness.
	for &ore in &BlockType::ALL {
		assert!(
			digblock_core::worldgen::nodes::dig_block(
				digblock_core::worldgen::nodes::DigMode::Extract(ore),
				BlockType::Stone,
			)
			.dig_hardness(FLOOR)
			.is_none(),
			"an extraction timed as {ore:?} would complete at the floor"
		);
	}
}

#[test]
fn unhealthy_journal_rejects_edits_with_server_error() {
	let mut sim = flat_sim("jhealth");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();

	sim.journal.force_write_failure = true;
	// Each accepted timed dig hits one failing append; after three the
	// journal flips unhealthy and the next request is refused.
	let mut t = 0;
	for seq in 1..=3u32 {
		dig_hold(&mut sim, 1, down, 90, seq, EditKind::Dig, &mut t);
	}
	// The dig target changes as blocks vanish, so also fire a plain
	// guaranteed-processed request once unhealthy.
	assert!(!sim.journal.healthy(), "injected failures flipped health");
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 9,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Dig,
			cell: [0, 0, 0],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				reason: RejectReason::ServerError,
				..
			}
		)),
		"unhealthy journal surfaces as ServerError: {:?}",
		c.msgs
	);
}

/// An instant dig (no DigStart) and an under-held dig both fail the
/// timing gate; the honest hold at the grace boundary is accepted.
#[test]
fn dig_timing_gate() {
	let mut sim = flat_sim("digtime");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

	// No DigStart at all.
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Dig,
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	// Held for a fraction of the required hold — short even of
	// `dig_grace`.
	let mut t = 0;
	dig_hold(&mut sim, 1, down, 10, 2, EditKind::Dig, &mut t);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigCancel,
	}]);
	// Honest hold: accepted (dig_hold holds the full hardness).
	let dug = dig_hold(&mut sim, 1, down, 90, 3, EditKind::Dig, &mut t);
	c.drain();

	for early in [1u32, 2] {
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq,
					reason: RejectReason::DigIncomplete
				} if *client_seq == early
			)),
			"seq {early} should be DigIncomplete: {:?}",
			c.msgs
		);
	}
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditApplied { client_seq: 3, edit, .. }
				if (edit.x, edit.y, edit.z) == dug
		)),
		"{:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Rubble),
		1,
		"dig credited the drop (the rock rule: stone lands as rubble)"
	);
}

/// §7.1's rock rule from the server's side, in both directions: the mountain's own stone crumbles
/// to Rubble, and one a mason PLACED comes back whole. The authority for "placed" is the journal —
/// the same bit the ore reveal reads — and it must be sampled BEFORE the dig writes its own entry,
/// which is the one way this silently inverts: every dig would then find itself standing in an
/// edited cell and hand out clean stone for a quarried one.
#[test]
fn a_placed_stone_comes_back_whole_and_a_quarried_one_crumbles() {
	let mut sim = flat_sim("placedstone");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let mason = digblock_core::unlock::craft(BlockType::Stone).expect("clean stone is made");
	sim.bench_set_alchemy_level(1, mason);
	sim.test_grant(1, BlockType::Stone, 1);
	place_cell(&mut sim, &mut c, 1, BlockType::Stone as u16);

	let mut t = 0;
	dig_hold(&mut sim, 1, down, 90, 2, EditKind::Dig, &mut t);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Stone),
		1,
		"a mason takes their own wall apart and gets the wall back"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::Rubble), 0);

	// The floor it stood on was never written, so it is still the
	// world's and still breaks.
	dig_hold(&mut sim, 1, down, 90, 3, EditKind::Dig, &mut t);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Rubble),
		1,
		"quarrying is quarrying"
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Stone),
		1,
		"no second"
	);
}

/// The other half of the same rule: below the mason's own unlock a placed stone crumbles like any
/// other rock. Not a refusal and not a punishment — a player who could not have MADE one is not
/// undoing their own work, so they get the rubble and the XP that goes with it, and the
/// craft-and-extract loop still cannot close, because the levels that can make a stone are exactly
/// the levels it comes back whole to.
#[test]
fn below_the_masons_unlock_even_a_placed_stone_crumbles() {
	let mut sim = flat_sim("placedstonelow");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let mason = digblock_core::unlock::craft(BlockType::Stone).expect("clean stone is made");
	sim.bench_set_alchemy_level(1, mason - 1);
	sim.test_grant(1, BlockType::Stone, 1);
	place_cell(&mut sim, &mut c, 1, BlockType::Stone as u16);

	let mut t = 0;
	dig_hold(&mut sim, 1, down, 90, 2, EditKind::Dig, &mut t);
	assert_eq!(sim.test_inventory_count(1, BlockType::Rubble), 1);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Stone),
		0,
		"an apprentice cannot cut it out whole"
	);
}

/// Looking away for a SUSTAINED stretch (past DIG_OFF_TARGET_RESET)
/// ends the hold, and the decay then bleeds the banked ticks away —
/// so coming back long afterwards still cannot complete the block.
#[test]
fn look_away_mid_hold_resets() {
	let mut sim = flat_sim("lookaway");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// The flat world is stone: clear the strength wall, or DigStart is
	// refused outright and this test asserts nothing (it did — the
	// gate shipped in protocol v17 and silently hollowed it out).
	sim.bench_set_mining_level(1, digblock_core::unlock::mine(BlockType::Stone));
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

	let mut t = 0;
	t += 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				slot: 0,
			},
		},
		batch(vec![aim(t, down)], 1),
	]);
	// A partial run on target, then a stretch at the horizon longer
	// than `DIG_OFF_TARGET_RESET`, then back on target well past the
	// would-be completion time: the hold stopped advancing at the
	// abandon and the banked ticks bled out over the decay window
	// that followed.
	for _ in 0..30 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	let banked = |sim: &super::super::Sim| {
		sim.clients
			.get(&1)
			.unwrap()
			.digging
			.as_ref()
			.map(|d| (d.progress, d.idle))
	};
	assert_eq!(banked(&sim), Some((30, 0)), "30 verified ticks banked");
	// Miss long enough to trip the reset, then keep missing: everything
	// past the threshold is decay.
	use digblock_net::protocol::DIG_OFF_TARGET_RESET;
	const DECAY_TICKS: u32 = 15;
	for _ in 0..DIG_OFF_TARGET_RESET + DECAY_TICKS {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, Vec3::Z)], 1)]);
	}
	assert_eq!(
		banked(&sim),
		Some((30, DECAY_TICKS)),
		"sustained miss stopped the hold and started the decay"
	);
	for _ in 0..90 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Dig,
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	c.drain();
	assert_eq!(
		banked(&sim).map(|(p, i)| digblock_core::skills::dig_decayed(p, 90, i)),
		Some(0),
		"a full decay window later there is nothing left of the 30"
	);
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 1,
				reason: RejectReason::DigIncomplete
			}
		)),
		"look-away reset the hold: {:?}",
		c.msgs
	);
}

/// Releasing the button no longer bins the hold: it bleeds down over
/// `DIG_DECAY_TICKS` and a restart inside that window inherits exactly
/// what is left. The client runs the identical arithmetic on the
/// identical interval, which is what keeps its bar honest.
#[test]
fn a_released_hold_decays_and_a_restart_inherits_the_remainder() {
	let mut sim = flat_sim("digdecay");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_mining_level(1, digblock_core::unlock::mine(BlockType::Stone));
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	let need = digblock_core::skills::dig_required(
		hit.block,
		90,
		digblock_core::unlock::mine(BlockType::Stone),
	);
	let start = || SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigStart {
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			slot: 0,
		},
	};
	let state = |sim: &super::super::Sim| {
		sim.clients
			.get(&1)
			.unwrap()
			.digging
			.as_ref()
			.map(|d| (d.progress, d.idle, d.banked(need)))
	};

	let mut t = 0;
	t += 1;
	sim.tick_once(vec![start(), batch(vec![aim(t, down)], 1)]);
	// Most of the way through: enough banked that a partial decay
	// window leaves something behind to inherit.
	for _ in 0..(need - 20) {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	assert_eq!(
		state(&sim),
		Some((need - 20, 0, need - 20)),
		"banked in full, none decayed"
	);

	// Button up, still aiming at it — the hold survives, shrinking.
	t += 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigCancel,
		},
		batch(vec![aim(t, down)], 1),
	]);
	for _ in 0..29 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	// Half the decay window idle costs half a BAR's worth of ticks,
	// which is more than half of what was banked. (The peak is
	// one past the loop: the cancel tick's verify ran before the
	// queued cancel did, which is honest either way.)
	let (peak, idle, banked) = state(&sim).expect("the hold survived the release");
	assert_eq!((peak, idle), (need - 19, 30), "peak kept, decay counted");
	assert_eq!(banked, peak - need / 2, "half a second cost half a bar");

	// Restarting inherits the remainder, not zero and not the peak.
	// `verify_digs` runs ahead of the action queue, so the restart
	// tick charges one more tick of decay before it resumes.
	t += 1;
	sim.tick_once(vec![start(), batch(vec![aim(t, down)], 1)]);
	let left = digblock_core::skills::dig_decayed(peak, need, idle + 1);
	assert!(left > 0 && left < peak, "a partial remainder: {left}");
	assert_eq!(
		state(&sim),
		Some((left, 0, left)),
		"the restart picked up where the decay left it"
	);

	// And it completes from there in what is still owed, not a whole
	// fresh bar — with `dig_grace` doing what it always does.
	for _ in 0..(need - left - digblock_net::protocol::dig_grace(need)) {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Dig,
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditApplied { client_seq: 1, edit, .. }
				if (edit.x, edit.y, edit.z) == hit.cell
		)),
		"the resumed hold completed: {:?}",
		c.msgs
	);
}

/// A hold left to bleed all the way out is worth nothing — a client
/// cannot park a nearly-broken block and cash it in later.
#[test]
fn a_fully_decayed_hold_is_worth_nothing() {
	let mut sim = flat_sim("digspent");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_mining_level(1, digblock_core::unlock::mine(BlockType::Stone));
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	let need = digblock_core::skills::dig_required(
		hit.block,
		90,
		digblock_core::unlock::mine(BlockType::Stone),
	);

	let mut t = 0;
	t += 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				slot: 0,
			},
		},
		batch(vec![aim(t, down)], 1),
	]);
	// One tick short of breaking it, then let go for well past the
	// whole decay window.
	for _ in 0..(need - 1) {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	t += 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigCancel,
		},
		batch(vec![aim(t, down)], 1),
	]);
	for _ in 0..120 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	// Restart and immediately claim it: worth zero, so the claim is
	// short by everything but the grace.
	t += 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				slot: 0,
			},
		},
		batch(vec![aim(t, down)], 1),
	]);
	assert_eq!(
		sim.clients
			.get(&1)
			.unwrap()
			.digging
			.as_ref()
			.map(|d| d.banked(need)),
		Some(0),
		"a spent hold restarts at zero"
	);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Dig,
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 1,
				reason: RejectReason::DigIncomplete
			}
		)),
		"a two-second-old hold cashed in: {:?}",
		c.msgs
	);
}

/// A brief aim miss (longer than the old consecutive-miss reset, well
/// under `DIG_OFF_TARGET_RESET`) PAUSES the hold: the missed ticks
/// don't count toward completion, but the banked progress survives and
/// the dig finishes once enough verified ticks accumulate.
#[test]
fn aim_miss_pauses_hold_instead_of_resetting() {
	let mut sim = flat_sim("digpause");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// The flat world is stone: clear the strength wall (it has its
	// own test; this one is about the aim-miss pause).
	sim.bench_set_mining_level(1, digblock_core::unlock::mine(BlockType::Stone));
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	// Derived, never hardcoded: the Rock family's curve is a tuning
	// number and this test is about the aim-miss pause, not its value.
	let need = digblock_core::skills::dig_required(
		hit.block,
		90,
		digblock_core::unlock::mine(BlockType::Stone),
	) as usize;

	let mut t = 0;
	t += 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				slot: 0,
			},
		},
		batch(vec![aim(t, down)], 1),
	]);
	// A partial run of verified ticks, then a brief stretch off
	// target: the wall clock counts both but only the verified ones
	// do — completing now is premature (this also pins that misses
	// are a pause, not elapsed time).
	for _ in 0..40 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	for _ in 0..15 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, Vec3::Z)], 1)]);
	}
	let dig = |seq: u32| SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Dig,
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	};
	sim.tick_once(vec![dig(1)]);
	// Back on target: the banked ticks survive the miss stretch (which
	// would have reset the hold under the old consecutive-miss rule),
	// so another `need` verified ticks complete the stone — a whole
	// bar's worth clears `need - grace` from any starting bank.
	for _ in 0..need {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	sim.tick_once(vec![dig(2)]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 1,
				reason: RejectReason::DigIncomplete
			}
		)),
		"paused ticks don't count toward completion: {:?}",
		c.msgs
	);
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditApplied { client_seq: 2, edit, .. }
				if (edit.x, edit.y, edit.z) == hit.cell
		)),
		"hold survived the brief miss and completed: {:?}",
		c.msgs
	);
}

/// Regression: back-to-back digs put "complete block A" and "start
/// block B" in the SAME tick drain. Wire order must hold — the new
/// DigStart must not clobber the hold A's completion validates
/// against (it did, and honest rapid digging bounced blocks back).
#[test]
fn same_tick_complete_and_restart_keeps_wire_order() {
	let mut sim = flat_sim("digrace");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// Stone world: clear the strength wall (not under test here).
	sim.bench_set_mining_level(1, digblock_core::unlock::mine(BlockType::Stone));
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	// Derived, never hardcoded — the curve is a tuning number and this
	// test is about wire order.
	let need = digblock_core::skills::dig_required(
		hit.block,
		90,
		digblock_core::unlock::mine(BlockType::Stone),
	) as usize;

	let mut t = 0;
	t += 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				slot: 0,
			},
		},
		batch(vec![aim(t, down)], 1),
	]);
	// A full bar of verified ticks at the level the strength wall
	// grants, so the completing request below is honest whatever the
	// family curve says.
	for _ in 0..need {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	// One tick carries both the completing EditRequest for A and the
	// DigStart for the block beneath it (what a client does the tick
	// after finishing A).
	let below = (hit.cell.0, hit.cell.1 - 1, hit.cell.2);
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		},
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [below.0, below.1, below.2],
				slot: 0,
			},
		},
	]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditApplied { client_seq: 1, edit, .. }
				if (edit.x, edit.y, edit.z) == hit.cell
		)),
		"completion processed before the same-tick restart: {:?}",
		c.msgs
	);
	let digging = sim
		.clients
		.get(&1)
		.unwrap()
		.digging
		.as_ref()
		.map(|d| (d.cell, d.cell_value));
	assert_eq!(
		digging,
		Some((below, BlockType::Stone as u16)),
		"the new hold took over after the completion"
	);
}

/// The cell's content changing mid-hold resets the record — a
/// collaborator can't swap a cheap block in under a running timer.
#[test]
fn block_swap_mid_hold_resets() {
	let mut sim = flat_sim("swap");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

	let mut t = 0;
	t += 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				slot: 0,
			},
		},
		batch(vec![aim(t, down)], 1),
	]);
	for _ in 0..30 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	// The target vanishes out from under the hold (authoritative
	// change by "someone else"), then reappears.
	sim.store
		.set_block(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Air);
	for _ in 0..12 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	sim.store
		.set_block(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Stone);
	for _ in 0..60 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Dig,
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 1,
				reason: RejectReason::DigIncomplete
			}
		)),
		"content swap reset the hold: {:?}",
		c.msgs
	);
}

/// Dig → place round trip: the credit funds exactly one placement;
/// the second placement is NoStock and carries the repair snapshot.
#[test]
fn place_consumes_stock_then_no_stock() {
	let mut sim = flat_sim("stock");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let mut t = 0;
	dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
	assert_eq!(sim.test_inventory_count(1, BlockType::Rubble), 1);

	// Place onto ground a few blocks out — outside our own capsule.
	let pos = sim.test_player(1).unwrap().pos;
	let out = Vec3::new(3.0, -2.0, 0.0).normalize();
	let place = |sim: &mut super::super::Sim, seq: u32| {
		let hit =
			raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).expect("ground out");
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::Place {
					block: BlockType::Rubble as u16,
				},
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				slot: 0,
			},
		}]);
	};
	place(&mut sim, 2);
	assert_eq!(sim.test_inventory_count(1, BlockType::Rubble), 0);
	place(&mut sim, 3);
	c.drain();
	assert!(
			c.msgs.iter().any(
				|m| matches!(m, ServerMsg::EditApplied { client_seq: 2, edit, .. } if edit.block == BlockType::Rubble as u16)
			),
			"funded placement applied: {:?}",
			c.msgs
		);
	let no_stock_at = c
		.msgs
		.iter()
		.position(|m| {
			matches!(
				m,
				ServerMsg::EditRejected {
					client_seq: 3,
					reason: RejectReason::NoStock
				}
			)
		})
		.expect("unfunded placement refused");
	assert!(
		matches!(
			c.msgs.get(no_stock_at + 1),
			Some(ServerMsg::InventoryFull { .. })
		),
		"NoStock is followed by the repair snapshot: {:?}",
		c.msgs
	);
}

/// Water tool round trip: place a source (funded), collect it back
/// with the fluid ray + hold, and a Collect aimed at a flow cell
/// can't name the flow (the ray passes it) — Occluded.
#[test]
fn collect_water_end_to_end() {
	let mut sim = flat_sim("collect");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// The vessel is crafted now, so the rig hands one over like any
	// other earned tool.
	sim.bench_set_active_tool(1, BlockType::Bucket);
	sim.clients
		.get_mut(&1)
		.unwrap()
		.inventory
		.add(BlockType::Water, 1);

	// Place the source on the ground a few blocks out.
	let pos = sim.test_player(1).unwrap().pos;
	let out = Vec3::new(3.0, -2.0, 0.0).normalize();
	let ground = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 0,
			origin: pos.to_array(),
			dir: out.to_array(),
			kind: EditKind::Place {
				block: BlockType::Water as u16,
			},
			cell: [ground.cell.0, ground.cell.1, ground.cell.2],
			normal: [
				ground.normal.0 as i8,
				ground.normal.1 as i8,
				ground.normal.2 as i8,
			],
			// Water places through the tool's slot (pinned last).
			slot: digblock_core::hotbar::HOTBAR_SLOTS as u8 - 1,
		},
	}]);
	assert_eq!(sim.test_inventory_count(1, BlockType::Water), 0);
	let source = (
		ground.cell.0 + ground.normal.0,
		ground.cell.1 + ground.normal.1,
		ground.cell.2 + ground.normal.2,
	);
	assert_eq!(
		sim.test_block_at(source.0, source.1, source.2),
		Some(BlockType::Water)
	);
	// Let the fluid spread so flow cells exist around the source.
	for _ in 0..40 {
		sim.tick_once(vec![]);
	}

	// Collect naming a flow cell: the fluid ray never stops there.
	let flow_cell = (source.0 + 1, source.1, source.2);
	assert!(
		sim.test_block_at(flow_cell.0, flow_cell.1, flow_cell.2)
			.is_some_and(|b| b.is_water() && !b.is_fluid_source())
	);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 2,
			tick: 0,
			origin: pos.to_array(),
			dir: out.to_array(),
			kind: EditKind::Collect,
			cell: [flow_cell.0, flow_cell.1, flow_cell.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);

	// Collect the source with water's real timed hold.
	let toward = (Vec3::new(
		source.0 as f32 + 0.5,
		source.1 as f32 + 0.5,
		source.2 as f32 + 0.5,
	) - pos)
		.normalize();
	let mut t = 100;
	let collected = dig_hold(&mut sim, 1, toward, 20, 3, EditKind::Collect, &mut t);
	assert_eq!(collected, source);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 2,
				reason: RejectReason::Occluded
			}
		)),
		"flow cell is not collectible: {:?}",
		c.msgs
	);
	assert!(
			c.msgs.iter().any(
				|m| matches!(m, ServerMsg::EditApplied { client_seq: 3, edit, .. } if edit.block == BlockType::Air as u16)
			),
			"source collected: {:?}",
			c.msgs
		);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Water),
		1,
		"water credited back"
	);
	// The scooped pool drains (lone source's spread, no regen pair).
	for _ in 0..600 {
		sim.tick_once(vec![]);
	}
	assert!(
		sim.test_block_at(flow_cell.0, flow_cell.1, flow_cell.2)
			.is_some_and(|b| !b.is_water())
	);
}

/// The bucket carries one charge, and the server owns it: an empty
/// one can't pour, a full one can't scoop. The second half matters
/// most — a refused scoop must leave the spring standing, not consume
/// it into a bucket that had no room.
#[test]
fn the_bucket_holds_exactly_one_charge() {
	let mut sim = flat_sim("charge");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_active_tool(1, BlockType::Bucket);
	let tool_slot = digblock_core::hotbar::HOTBAR_SLOTS as u8 - 1;
	let pos = sim.test_player(1).unwrap().pos;
	let out = Vec3::new(3.0, -2.0, 0.0).normalize();
	let ground = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	let pour = |seq: u32| SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq,
			tick: 0,
			origin: pos.to_array(),
			dir: out.to_array(),
			kind: EditKind::Place {
				block: BlockType::Water as u16,
			},
			cell: [ground.cell.0, ground.cell.1, ground.cell.2],
			normal: [
				ground.normal.0 as i8,
				ground.normal.1 as i8,
				ground.normal.2 as i8,
			],
			slot: tool_slot,
		},
	};

	// An empty bucket pours nothing.
	sim.tick_once(vec![pour(1)]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 1,
				reason: RejectReason::NoStock
			}
		)),
		"an empty bucket poured: {:?}",
		c.msgs
	);

	// Charge it and pour for real.
	sim.clients
		.get_mut(&1)
		.unwrap()
		.inventory
		.add(BlockType::Water, 1);
	sim.tick_once(vec![pour(2)]);
	let source = (
		ground.cell.0 + ground.normal.0,
		ground.cell.1 + ground.normal.1,
		ground.cell.2 + ground.normal.2,
	);
	assert_eq!(
		sim.test_block_at(source.0, source.1, source.2),
		Some(BlockType::Water)
	);
	assert!(
		!sim.clients[&1].inventory.bucket_full(),
		"the pour emptied it"
	);

	// Charge it again by hand and aim the scoop at the source we just
	// poured: a full bucket has nowhere to put it.
	sim.clients
		.get_mut(&1)
		.unwrap()
		.inventory
		.add(BlockType::Water, 1);
	let toward = (Vec3::new(
		source.0 as f32 + 0.5,
		source.1 as f32 + 0.5,
		source.2 as f32 + 0.5,
	) - pos)
		.normalize();
	let mut t = 100;
	dig_hold(&mut sim, 1, toward, 20, 3, EditKind::Collect, &mut t);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 3,
				reason: RejectReason::BucketFull
			}
		)),
		"a charged bucket scooped again: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_block_at(source.0, source.1, source.2),
		Some(BlockType::Water),
		"the refused scoop must not have consumed the source"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::Water), 1);
}

/// Two placed sources one apart: the middle regenerates into a source
/// server-side and broadcasts as an ordinary SERVER_ID edit.
#[test]
fn regen_broadcasts_as_server_edit() {
	let mut sim = flat_sim("regen");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let y = 127; // first air layer over the flat stone ground
	assert!(sim.store.set_block(6, y, 8, BlockType::Water).is_some());
	assert!(sim.store.set_block(8, y, 8, BlockType::Water).is_some());
	sim.fluid.schedule_around(6, y, 8);
	sim.fluid.schedule_around(8, y, 8);
	let mut regenerated = false;
	for _ in 0..300 {
		sim.tick_once(vec![]);
		if sim.test_block_at(7, y, 8) == Some(BlockType::Water) {
			regenerated = true;
			break;
		}
	}
	assert!(regenerated, "middle cell became a source");
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditApplied { by, edit, .. }
				if *by == digblock_net::protocol::SERVER_ID
					&& (edit.x, edit.y, edit.z) == (7, y, 8)
					&& edit.block == BlockType::Water as u16
		)),
		"regen broadcast as a SERVER_ID edit: {:?}",
		c.msgs
	);
}

/// Same-tick DigStarts resolve in wire order — the LAST start wins,
/// exactly like the client's retarget stream. The old first-wins
/// per-tick cap silently dropped the restart half of a cancel +
/// restart pair bunched into one tick drain, leaving an honest hold
/// with no server record (guaranteed DigIncomplete on completion).
#[test]
fn same_tick_dig_starts_resolve_in_wire_order() {
	let mut sim = flat_sim("digflood");
	let _c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// Stone world: clear the strength wall (not under test here).
	sim.bench_set_mining_level(1, digblock_core::unlock::mine(BlockType::Stone));
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	let other = (hit.cell.0, hit.cell.1 - 1, hit.cell.2);
	let start = |cell: (i32, i32, i32)| SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigStart {
			cell: [cell.0, cell.1, cell.2],
			slot: 0,
		},
	};
	let cancel = || SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigCancel,
	};
	// A retarget flick bunched into one server tick: start A, then
	// cancel + restart on B — B's hold must stand.
	sim.tick_once(vec![start(hit.cell), cancel(), start(other)]);
	let digging = |sim: &super::super::Sim| {
		sim.clients
			.get(&1)
			.unwrap()
			.digging
			.as_ref()
			.map(|d| d.cell)
	};
	assert_eq!(digging(&sim), Some(other), "last start wins the tick");
	// And the next tick's start replaces it as usual.
	sim.tick_once(vec![start(hit.cell)]);
	assert_eq!(digging(&sim), Some(hit.cell));
}

#[test]
fn place_into_player_and_rate_limit() {
	let mut sim = flat_sim("place");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::NEG_Y;
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();

	// Placing on the ground's top face targets our own body cell.
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 100,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Place {
				block: BlockType::Dirt as u16,
			},
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 1,
				reason: RejectReason::InsidePlayer
			}
		)),
		"{:?}",
		c.msgs
	);

	// Hammer digs: the token bucket runs dry.
	let digs: Vec<SimEvent> = (10..30)
		.map(|seq| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 100,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Dig,
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [0, 0, 0],
				slot: 0,
			},
		})
		.collect();
	sim.tick_once(digs);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				reason: RejectReason::RateLimited,
				..
			}
		)),
		"rate limiter engaged"
	);
}

// ---- sub-block shapes (v8) ------------------------------------------

use digblock_core::chunk::{STATE_SHIFT, cell_state, make_cell};
use digblock_core::shapes::{Facing, door_state, stair_state};

/// Aim at the ground and place `cell_value`; returns (support cell,
/// target cell). The caller grants stock first.
fn place_cell(sim: &mut Sim, c: &mut TestClient, seq: u32, cell_value: u16) -> (i32, i32, i32) {
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Place { block: cell_value },
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			slot: 0,
		},
	}]);
	c.drain();
	(
		hit.cell.0 + hit.normal.0,
		hit.cell.1 + hit.normal.1,
		hit.cell.2 + hit.normal.2,
	)
}

#[test]
fn place_stair_with_facing_applies_full_cell() {
	let mut sim = flat_sim("stairplace");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::StoneStairs, 1);
	let want = make_cell(BlockType::StoneStairs, stair_state(Facing::NegZ));
	let target = place_cell(&mut sim, &mut c, 1, want);
	assert_eq!(
		sim.test_cell_at(target.0, target.1, target.2),
		Some(want),
		"state nibble survives to the store"
	);
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditApplied { edit, .. } if edit.block == want
		)),
		"broadcast carries the full cell: {:?}",
		c.msgs
	);
}

#[test]
fn stateful_place_gates() {
	let mut sim = flat_sim("stategates");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::WoodStairs, 5);
	sim.test_grant(1, BlockType::LumberDoor, 5);
	let bad = [
		// Stateless block with state bits: still BadBlock.
		(BlockType::Dirt as u16) | (1 << STATE_SHIFT),
		// Stair with the non-facing state bits set.
		make_cell(BlockType::WoodStairs, 0b0100),
		// Door requests must be lower+closed — server derives upper.
		make_cell(BlockType::LumberDoor, door_state(Facing::PosX, true, false)),
		make_cell(BlockType::LumberDoor, door_state(Facing::PosX, false, true)),
		// A cell's spare bits, between the id and the state nibble.
		(BlockType::WoodStairs as u16) | (1 << 10),
	];
	for (i, cell) in bad.into_iter().enumerate() {
		place_cell(&mut sim, &mut c, 10 + i as u32, cell);
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditRejected {
					client_seq,
					reason: RejectReason::BadBlock,
				} if *client_seq == 10 + i as u32
			)),
			"cell {cell:#06x} refused: {:?}",
			c.msgs
		);
	}
}

// ---- stackable slabs ------------------------------------------------

/// Aim from the eye at `point` and place `cell_value`, declaring
/// `slot`. The shared angled-down ray enters a grown slab through its
/// SIDE face (no grow), so slab tests aim at the top face directly.
fn place_aimed(
	sim: &mut Sim,
	c: &mut TestClient,
	seq: u32,
	cell_value: u16,
	point: Vec3,
	slot: u8,
) {
	let pos = sim.test_player(1).unwrap().pos;
	let dir = (point - pos).normalize();
	let hit = raycast(pos, dir, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq,
			tick: 0,
			origin: pos.to_array(),
			dir: dir.to_array(),
			kind: EditKind::Place { block: cell_value },
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			slot,
		},
	}]);
	c.drain();
}

/// Center of a cell's top surface at `height` twelfths, a hair inside
/// the box so the aimed ray enters through the TOP face.
fn top_point(cell: (i32, i32, i32), height_twelfths: u8) -> Vec3 {
	Vec3::new(
		cell.0 as f32 + 0.5,
		cell.1 as f32 + height_twelfths as f32 / 12.0 - 0.03,
		cell.2 as f32 + 0.5,
	)
}

/// Three top-face places: fresh 1/3, grow to 2/3 IN PLACE, grow to
/// the plain base block — one slab debit each, nothing written above.
#[test]
fn slab_top_place_grows_to_base_block_with_per_step_debits() {
	let mut sim = flat_sim("slabgrow");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::WoodSlab, 3);
	let slab0 = BlockType::WoodSlab as u16;

	let cell = place_cell(&mut sim, &mut c, 1, slab0);
	assert_eq!(sim.test_cell_at(cell.0, cell.1, cell.2), Some(slab0));
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 2);

	// The ray now stops in the slab itself; place_cell returns
	// hit.cell + normal (the cell above) — the write must land in
	// the slab cell, not there.
	let above = place_cell(&mut sim, &mut c, 2, slab0);
	assert_eq!(above, (cell.0, cell.1 + 1, cell.2), "ray hit the slab top");
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(make_cell(BlockType::WoodSlab, 1)),
		"grew in place to 2/3"
	);
	assert_eq!(
		sim.test_block_at(above.0, above.1, above.2),
		Some(BlockType::Air),
		"nothing written above"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 1);

	place_aimed(&mut sim, &mut c, 3, slab0, top_point(cell, 8), 0);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(BlockType::Wood as u16),
		"third grow completes into the plain base block"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 0);
	// The full cube is ordinary Wood now: no fourth grow — placing
	// on its top writes a fresh 1/3 in the cell above.
	sim.test_grant(1, BlockType::WoodSlab, 1);
	place_aimed(&mut sim, &mut c, 4, slab0, top_point(cell, 12), 0);
	assert_eq!(sim.test_cell_at(above.0, above.1, above.2), Some(slab0));
}

/// Both halves of the client's natural double-click — fresh 1/3, then grow — enqueued in ONE tick,
/// the racing shape whose acks supersede each other client-side. The server accepts and debits
/// BOTH: this is the truth the client's deferred-delta rule (`PendingEdits`) has to match, and a
/// client that refunds the second debit at supersede time drifts a phantom item for good.
#[test]
fn racing_slab_places_in_one_tick_both_debit() {
	let mut sim = flat_sim("slabrace");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::WoodSlab, 2);
	let start = sim.test_inventory_count(1, BlockType::WoodSlab);
	let slab0 = BlockType::WoodSlab as u16;

	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	let cell = (
		hit.cell.0 + hit.normal.0,
		hit.cell.1 + hit.normal.1,
		hit.cell.2 + hit.normal.2,
	);
	// The second request is aimed the way the CLIENT aimed it — at the
	// predicted slab's top face, before any ack came back. The server
	// re-raycasts it after the first write has landed, so it verifies
	// against the grown world exactly as an honest client computed it.
	let grow_dir = (top_point(cell, 4) - pos).normalize();
	c.drain();
	c.msgs.clear();
	let place = |seq: u32, dir: Vec3, target: (i32, i32, i32), normal: [i8; 3]| SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq,
			tick: 0,
			origin: pos.to_array(),
			dir: dir.to_array(),
			kind: EditKind::Place { block: slab0 },
			cell: [target.0, target.1, target.2],
			normal,
			slot: 0,
		},
	};
	sim.tick_once(vec![
		place(
			1,
			down,
			hit.cell,
			[hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
		),
		place(2, grow_dir, cell, [0, 1, 0]),
	]);
	c.drain();

	for seq in [1u32, 2] {
		assert!(
			c.msgs.iter().any(|m| matches!(
				m,
				ServerMsg::EditApplied { client_seq, .. } if *client_seq == seq
			)),
			"place {seq} applied: {:?}",
			c.msgs
		);
	}
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(digblock_core::shapes::slab_grown(slab0)),
		"the second place grew the first in place"
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::WoodSlab),
		start - 2,
		"one debit per accepted place, racing or not"
	);
}

/// A WrongSlot rejection carries the same `InventoryFull` rebase as
/// NoStock: a drifted count presents as a slot mismatch once the two
/// ends' layouts reconcile apart, and without the rebase the drift
/// would repeat on every retry instead of healing.
#[test]
fn wrong_slot_rejection_carries_the_rebase() {
	let mut sim = flat_sim("wrongslot");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::Dirt, 5);

	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	// Declare a slot that does not hold Dirt (the grant filed it in the
	// first free slot; the last one is empty).
	let bare = (digblock_core::hotbar::HOTBAR_SLOTS - 1) as u8;
	c.drain();
	c.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Place {
				block: BlockType::Dirt as u16,
			},
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			slot: bare,
		},
	}]);
	c.drain();

	let rejected = c.msgs.iter().position(|m| {
		matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 1,
				reason: RejectReason::WrongSlot,
			}
		)
	});
	let rejected = rejected.expect("WrongSlot rejection");
	assert!(
		c.msgs[rejected..].iter().any(
			|m| matches!(m, ServerMsg::InventoryFull { counts } if *counts == sim.clients[&1].inventory.to_wire()),
		),
		"the authoritative rebase follows the rejection: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Dirt),
		5,
		"a refused place debits nothing"
	);
}

/// A pre-grown 2/3 request is BadBlock (one item can't buy two
/// thirds), and a cross-material slab on a slab top does NOT grow.
#[test]
fn slab_grow_gates() {
	let mut sim = flat_sim("slabgates");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::WoodSlab, 1);
	sim.test_grant(1, BlockType::StoneSlab, 1);

	place_cell(&mut sim, &mut c, 1, make_cell(BlockType::WoodSlab, 1));
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 1,
				reason: RejectReason::BadBlock,
			}
		)),
		"pre-grown state refused: {:?}",
		c.msgs
	);

	let cell = place_cell(&mut sim, &mut c, 2, BlockType::WoodSlab as u16);
	// Stone slab aimed at the wood slab's top: fresh 1/3 above, no
	// grow. Declare the slot the stone slab actually landed in.
	let stone_slot = sim.clients[&1]
		.hotbar
		.slots()
		.iter()
		.position(|s| {
			*s == Some(digblock_core::hotbar::HotbarItem::Block(
				BlockType::StoneSlab,
			))
		})
		.expect("stone slab on the bar") as u8;
	let above = (cell.0, cell.1 + 1, cell.2);
	place_aimed(
		&mut sim,
		&mut c,
		3,
		BlockType::StoneSlab as u16,
		top_point(cell, 4),
		stone_slot,
	);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(BlockType::WoodSlab as u16),
		"wood slab untouched"
	);
	assert_eq!(
		sim.test_cell_at(above.0, above.1, above.2),
		Some(BlockType::StoneSlab as u16),
		"stone slab placed fresh above"
	);
}

/// Flat ground with a stone roof at `ROOF_Y` — a reachable ceiling
/// over the spawn, which the flat world otherwise has nowhere.
fn roofed_sim(tag: &str) -> Sim {
	use digblock_core::chunk::{AIR_CHUNK, ChunkData, WORLD_VCHUNK};
	use std::sync::Arc;
	const ROOF_Y: usize = 130;
	let source: crate::store::ColumnSource = Arc::new(|_cx, _cz| {
		let mut full = ChunkData::default();
		let mut top = ChunkData::default();
		let mut roof = ChunkData::default();
		for z in 0..16 {
			for x in 0..16 {
				roof.set(x, ROOF_Y % 16, z, BlockType::Stone);
				for y in 0..16 {
					full.set(x, y, z, BlockType::Stone);
					if y <= FLAT_GROUND_TOP % 16 {
						top.set(x, y, z, BlockType::Stone);
					}
				}
			}
		}
		let (full, top, roof) = (Arc::new(full), Arc::new(top), Arc::new(roof));
		(0..WORLD_VCHUNK as usize)
			.map(|cy| match (cy.cmp(&(FLAT_GROUND_TOP / 16)), cy) {
				(std::cmp::Ordering::Less, _) => full.clone(),
				(std::cmp::Ordering::Equal, _) => top.clone(),
				(_, cy) if cy == ROOF_Y / 16 => roof.clone(),
				_ => AIR_CHUNK.clone(),
			})
			.collect()
	});
	Sim::new_with_source(SEED, &temp_dir(tag), 1, 2, source, FLAT_SPAWN).unwrap()
}

/// The mirror of `slab_top_place_grows_to_base_block_with_per_step_debits`,
/// against a ceiling: the first slab hangs from the roof instead of
/// falling to the floor of the cell, and each further place grows it
/// DOWNWARD from the same anchor — the grow face is the slab's open
/// side, which is now its underside.
#[test]
fn slab_placed_against_a_ceiling_hangs_and_grows_downward() {
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::slab_state;
	let mut sim = roofed_sim("slabceiling");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::WoodSlab, 3);
	let slab0 = BlockType::WoodSlab as u16;
	let cell = (8, 129, 8); // under the roof block at y = 130
	let up = Vec3::new(8.5, 129.97, 8.5);

	place_aimed(&mut sim, &mut c, 1, slab0, up, 0);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(make_cell(BlockType::WoodSlab, slab_state(false, true))),
		"a slab built against the roof hangs from it"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 2);

	// The ray now stops at the hanging slab's underside; the write
	// must grow that cell, not land in the one below it.
	place_aimed(&mut sim, &mut c, 2, slab0, up, 0);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(make_cell(BlockType::WoodSlab, slab_state(true, true))),
		"grew in place to a 2/3 still hanging"
	);
	assert_eq!(
		sim.test_block_at(cell.0, cell.1 - 1, cell.2),
		Some(BlockType::Air),
		"nothing written below"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 1);

	place_aimed(&mut sim, &mut c, 3, slab0, up, 0);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(BlockType::Wood as u16),
		"third grow completes into the plain base block"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 0);
}

/// Digging a ceiling slab shrinks it toward its ceiling, one third
/// per hold — the anchor is the dig's too, so a hanging slab never
/// drops to the floor of its cell partway through being removed.
#[test]
fn ceiling_slab_dig_shrinks_upward() {
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::slab_state;
	let mut sim = roofed_sim("slabceildig");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::WoodSlab, 2);
	let slab0 = BlockType::WoodSlab as u16;
	let cell = (8, 129, 8);
	let up = Vec3::new(8.5, 129.97, 8.5);
	place_aimed(&mut sim, &mut c, 1, slab0, up, 0);
	place_aimed(&mut sim, &mut c, 2, slab0, up, 0);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(make_cell(BlockType::WoodSlab, slab_state(true, true)))
	);

	// A slab is timed per third, and WoodSlab's third is short — this
	// hold covers one of them outright.
	let mut t = 0;
	let dug = dig_hold(&mut sim, 1, Vec3::Y, 25, 3, EditKind::Dig, &mut t);
	assert_eq!(dug, cell);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(make_cell(BlockType::WoodSlab, slab_state(false, true))),
		"back to a 1/3 still hanging from the roof"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 1);

	let dug = dig_hold(&mut sim, 1, Vec3::Y, 25, 4, EditKind::Dig, &mut t);
	assert_eq!(dug, cell);
	assert_eq!(
		sim.test_block_at(cell.0, cell.1, cell.2),
		Some(BlockType::Air),
		"the last third clears"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 2);
}

/// Digging shrinks one third per completed hold (2/3 → 1/3 → Air),
/// crediting one slab item each time.
#[test]
fn slab_dig_shrinks_by_thirds() {
	let mut sim = flat_sim("slabshrink");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::WoodSlab, 2);
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();

	let cell = place_cell(&mut sim, &mut c, 1, BlockType::WoodSlab as u16);
	place_cell(&mut sim, &mut c, 2, BlockType::WoodSlab as u16);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(make_cell(BlockType::WoodSlab, 1))
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 0);

	// A slab is timed per third, and WoodSlab's third is short — this
	// hold covers one of them outright.
	let mut t = 0;
	let dug = dig_hold(&mut sim, 1, down, 25, 3, EditKind::Dig, &mut t);
	assert_eq!(dug, cell);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(BlockType::WoodSlab as u16),
		"2/3 shrank to 1/3, not Air"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 1);

	let dug = dig_hold(&mut sim, 1, down, 25, 4, EditKind::Dig, &mut t);
	assert_eq!(dug, cell);
	assert_eq!(
		sim.test_block_at(cell.0, cell.1, cell.2),
		Some(BlockType::Air),
		"1/3 dug away"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), 2);
}

/// A grow landing mid-hold changes the cell's content, which resets
/// the hold (verify_digs compares FULL cells) — a banked 1/3-priced
/// timer can't complete against the taller fill.
#[test]
fn slab_grow_mid_hold_resets_dig_timer() {
	let mut sim = flat_sim("slabmidhold");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::WoodSlab, 2);
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();

	let cell = place_cell(&mut sim, &mut c, 1, BlockType::WoodSlab as u16);
	// Bank a complete hold on the 1/3 slab, but DON'T send the edit yet.
	let pos = sim.test_player(1).unwrap().pos;
	let mut t = 1;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::DigStart {
				cell: [cell.0, cell.1, cell.2],
				slot: 0,
			},
		},
		batch(vec![aim(t, down)], 1),
	]);
	for _ in 0..25 {
		t += 1;
		sim.tick_once(vec![batch(vec![aim(t, down)], 1)]);
	}
	// Grow the cell under the hold, then try to complete the dig.
	place_cell(&mut sim, &mut c, 2, BlockType::WoodSlab as u16);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 3,
			tick: 0,
			origin: pos.to_array(),
			dir: down.to_array(),
			kind: EditKind::Dig,
			cell: [cell.0, cell.1, cell.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 3,
				reason: RejectReason::DigIncomplete,
			}
		)),
		"banked hold reset by the grow: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(make_cell(BlockType::WoodSlab, 1)),
		"cell still the grown 2/3"
	);
}

#[test]
fn door_place_writes_both_halves_one_debit() {
	let mut sim = flat_sim("doorplace");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::LumberDoor, 1);
	let lower_cell = make_cell(
		BlockType::LumberDoor,
		door_state(Facing::PosZ, false, false),
	);
	let target = place_cell(&mut sim, &mut c, 1, lower_cell);
	let upper = (target.0, target.1 + 1, target.2);
	assert_eq!(
		sim.test_cell_at(target.0, target.1, target.2),
		Some(lower_cell)
	);
	assert_eq!(
		sim.test_cell_at(upper.0, upper.1, upper.2),
		Some(make_cell(
			BlockType::LumberDoor,
			door_state(Facing::PosZ, false, true)
		)),
		"server derived the upper half"
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::LumberDoor),
		0,
		"one debit"
	);
	// Two broadcasts: the lower carries our seq, the upper seq 0.
	let applied: Vec<_> = c
		.msgs
		.iter()
		.filter_map(|m| match m {
			ServerMsg::EditApplied {
				client_seq, edit, ..
			} => Some((*client_seq, (edit.x, edit.y, edit.z))),
			_ => None,
		})
		.collect();
	assert!(applied.contains(&(1, target)), "{applied:?}");
	assert!(applied.contains(&(0, upper)), "{applied:?}");
}

/// An OPEN door has empty collision boxes, so the plain place ray runs straight through its leaf
/// and stops on the floor beyond — making the door's own cell the placement target. The target has
/// to be checked for replaceability: checking only the derived UPPER half, and with `!collides`,
/// which an open door passes, let one right-click destroy any player's door — no drop, no
/// hold-to-dig timer, and an orphaned upper half whose partner no longer resolved.
#[test]
fn placing_into_an_open_door_is_refused() {
	let mut sim = flat_sim("doorover");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// Stage the door directly and stock ONLY stone, so it lands in the
	// first bar slot and the slot gate cannot mask the result —
	// without the replaceable check this request must actually land on
	// the door.
	let pos = sim.test_player(1).unwrap().pos;
	let lower = (
		pos.x.floor() as i32 + 1,
		FLAT_GROUND_TOP as i32 + 1,
		pos.z.floor() as i32,
	);
	let upper = (lower.0, lower.1 + 1, lower.2);
	// Open: its collision boxes empty out, so the plain ray sees through.
	sim.test_set_cell(
		lower.0,
		lower.1,
		lower.2,
		make_cell(BlockType::LumberDoor, door_state(Facing::PosZ, true, false)),
	);
	sim.test_set_cell(
		upper.0,
		upper.1,
		upper.2,
		make_cell(BlockType::LumberDoor, door_state(Facing::PosZ, true, true)),
	);
	sim.test_grant(1, BlockType::Stone, 1);

	// Aim from inside the doorway down at the floor under the leaf: the
	// ray passes through the open door and the support hit's +y normal
	// puts the target squarely on the door's lower cell.
	let origin = Vec3::new(
		lower.0 as f32 + 0.5,
		lower.1 as f32 + 1.6,
		lower.2 as f32 + 0.5,
	);
	let floor = (lower.0, lower.1 - 1, lower.2);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 2,
			tick: 0,
			origin: origin.to_array(),
			dir: [0.0, -1.0, 0.0],
			kind: EditKind::Place {
				block: BlockType::Stone as u16,
			},
			cell: [floor.0, floor.1, floor.2],
			normal: [0, 1, 0],
			slot: 0,
		},
	}]);
	c.drain();

	assert_eq!(
		sim.test_cell_at(lower.0, lower.1, lower.2),
		Some(make_cell(
			BlockType::LumberDoor,
			door_state(Facing::PosZ, true, false)
		)),
		"the open door survived the placement"
	);
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 2,
				reason: RejectReason::Occluded
			}
		)),
		"refused, and told the client why: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Stone),
		1,
		"no stock spent on the refused place"
	);
}

#[test]
fn door_toggle_flips_both_halves_and_dig_clears_both() {
	let mut sim = flat_sim("doortoggle");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::LumberDoor, 1);
	let lower_cell = make_cell(
		BlockType::LumberDoor,
		door_state(Facing::PosZ, false, false),
	);
	let target = place_cell(&mut sim, &mut c, 1, lower_cell);
	let upper = (target.0, target.1 + 1, target.2);

	// Toggle open: aim at the lower half.
	let pos = sim.test_player(1).unwrap().pos;
	let dir = (Vec3::new(
		target.0 as f32 + 0.5,
		target.1 as f32 + 0.5,
		target.2 as f32 + 0.05,
	) - pos)
		.normalize();
	let hit = raycast(pos, dir, REACH, &|x, y, z| sim.test_cell_at(x, y, z))
		.expect("door under the pick ray");
	assert_eq!(hit.cell, target);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 2,
			tick: 0,
			origin: pos.to_array(),
			dir: dir.to_array(),
			kind: EditKind::Interact,
			cell: [target.0, target.1, target.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	c.drain();
	let lower_now = sim.test_cell_at(target.0, target.1, target.2).unwrap();
	let upper_now = sim.test_cell_at(upper.0, upper.1, upper.2).unwrap();
	assert!(cell_state(lower_now) & 0b0100 != 0, "lower open");
	assert!(cell_state(upper_now) & 0b0100 != 0, "upper open");

	// Close it again by clicking the SWUNG leaf, which now hugs a
	// different cell face, then hold-dig the lower half: both cells
	// clear, one credit. (The dig would land on the open leaf too —
	// the pick ray finds it — but closing first is the older path and
	// keeps this test covering both aims.)
	let leaf_box = digblock_core::shapes::interaction_boxes(lower_now)[0];
	let (leaf_lo, leaf_hi) = leaf_box.aabb(target);
	let dir2 = ((leaf_lo + leaf_hi) * 0.5 - pos).normalize();
	let hit2 = raycast(pos, dir2, REACH, &|x, y, z| sim.test_cell_at(x, y, z))
		.expect("open leaf under the pick ray");
	assert_eq!(hit2.cell, target, "swung leaf still clickable");
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 3,
			tick: 0,
			origin: pos.to_array(),
			dir: dir2.to_array(),
			kind: EditKind::Interact,
			cell: [target.0, target.1, target.2],
			normal: [0, 0, 0],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		cell_state(sim.test_cell_at(target.0, target.1, target.2).unwrap()) & 0b0100 == 0,
		"closed again"
	);
	let mut t = 100;
	let hold = BlockType::LumberDoor.hardness_ticks().unwrap() as u32 + 6;
	let dug = dig_hold(&mut sim, 1, dir, hold, 4, EditKind::Dig, &mut t);
	assert_eq!(dug, target);
	c.drain();
	assert_eq!(sim.test_cell_at(target.0, target.1, target.2), Some(0));
	assert_eq!(
		sim.test_cell_at(upper.0, upper.1, upper.2),
		Some(0),
		"partner cleared too"
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::LumberDoor),
		1,
		"one credit for the pair"
	);
}

#[test]
fn alchemy_craft_and_same_tick_place() {
	use digblock_core::block::Essence;
	use digblock_net::messages::AlchemyOp;
	let mut sim = flat_sim("alchemy");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// One timber cube plus enough Vitae for one slab craft's catalyst
	// at the level-1 waste, with the remainder asserted below.
	sim.test_grant(1, BlockType::Wood, 1);
	sim.test_grant_essence(1, Essence::Vitae, 10);

	// Craft + place, both queued in ONE tick: wire order must debit
	// before the place's stock gate runs. The WOOD slab is the craft a
	// fresh player can afford — under §7.2's grid the material carries
	// the progression, so clean stone in any shape waits for its own
	// material unlock.
	let pos = sim.test_player(1).unwrap().pos;
	let down = Vec3::new(0.45, -1.0, 0.0).normalize();
	let hit = raycast(pos, down, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	let slab = make_cell(BlockType::WoodSlab, 0);
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Alchemy {
				op: AlchemyOp::Craft {
					item: BlockType::WoodSlab as u16,
					count: 1,
				},
			},
		},
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq: 1,
				tick: 0,
				origin: pos.to_array(),
				dir: down.to_array(),
				kind: EditKind::Place { block: slab },
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				// The crafted slab is the only item, so the reconcile
				// files it in the first bar slot — the same derivation
				// an honest client runs.
				slot: 0,
			},
		},
	]);
	c.drain();
	let target = (
		hit.cell.0 + hit.normal.0,
		hit.cell.1 + hit.normal.1,
		hit.cell.2 + hit.normal.2,
	);
	assert_eq!(
		sim.test_cell_at(target.0, target.1, target.2),
		Some(slab),
		"craft-then-place landed in one tick: {:?}",
		c.msgs
	);
	// The level-1 economy: the craft spends the timber cube and the
	// catalyst at that level's waste, leaving the Vitae remainder.
	let (catalyst, n) =
		digblock_core::alchemy::craft_ideal(BlockType::WoodSlab).expect("a slab is bench-made")[0];
	assert_eq!(catalyst, Essence::Vitae, "the slab's catalyst");
	let left = 10 - digblock_core::alchemy::craft_cost(n, 1);
	let want = (
		digblock_core::inventory::SUB_WIRE_FLAG | catalyst as u16,
		left,
	);
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::InventoryFull { counts }
				if counts.contains(&want)
		)),
		"Vitae remainder in the rebase: {:?}",
		c.msgs
	);

	// Invalid ops still answer with the authoritative rebase — here a
	// real recipe at an impossible count, so it is the affordability
	// gate refusing rather than the unlock.
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::Alchemy {
			op: AlchemyOp::Craft {
				item: BlockType::WoodSlab as u16,
				count: 999,
			},
		},
	}]);
	let before = sim.test_inventory_count(1, BlockType::WoodSlab);
	c.drain();
	assert_eq!(sim.test_inventory_count(1, BlockType::WoodSlab), before);
	assert!(
		c.msgs
			.iter()
			.rev()
			.any(|m| matches!(m, ServerMsg::InventoryFull { .. })),
		"failed op still rebases"
	);
}

/// Slot enforcement: a place must use the item in its declared slot.
/// Declaring another slot rejects with WrongSlot and mutates nothing;
/// the honest slot lands. A Layout update in the same tick applies in
/// wire order, so assign-then-place works.
#[test]
fn wrong_slot_place_rejected_and_layout_applies_in_order() {
	let mut sim = flat_sim("wrongslot");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// Each grant reconciles onto the bar in block-id order: Dirt first,
	// then Stone.
	sim.test_grant(1, BlockType::Dirt, 5);
	sim.test_grant(1, BlockType::Stone, 5);
	let pos = sim.test_player(1).unwrap().pos;
	let out = Vec3::new(3.0, -2.0, 0.0).normalize();
	let hit = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).unwrap();
	let place = |seq: u32, block: BlockType, slot: u8| SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq,
			tick: 0,
			origin: pos.to_array(),
			dir: out.to_array(),
			kind: EditKind::Place {
				block: block as u16,
			},
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			slot,
		},
	};
	let target = (
		hit.cell.0 + hit.normal.0,
		hit.cell.1 + hit.normal.1,
		hit.cell.2 + hit.normal.2,
	);

	// Dirt declared from Stone's slot: rejected, nothing changes.
	sim.tick_once(vec![place(1, BlockType::Dirt, 1)]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				client_seq: 1,
				reason: RejectReason::WrongSlot
			}
		)),
		"{:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_block_at(target.0, target.1, target.2),
		Some(BlockType::Air),
		"rejected place never lands"
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::Dirt), 5, "no debit");
	// An out-of-range slot is a wrong slot too.
	sim.tick_once(vec![place(2, BlockType::Dirt, 200)]);
	c.drain();
	assert!(c.msgs.iter().any(|m| matches!(
		m,
		ServerMsg::EditRejected {
			client_seq: 2,
			reason: RejectReason::WrongSlot
		}
	)));

	// Same tick: move Dirt to a different bar slot via Layout, then
	// place from that slot — wire order means the arrangement lands
	// first.
	use digblock_core::hotbar::{WIRE_EMPTY, WIRE_TOOL};
	let mut hotbar = vec![WIRE_EMPTY; digblock_core::hotbar::HOTBAR_SLOTS];
	hotbar[4] = BlockType::Dirt as u16;
	hotbar[1] = BlockType::Stone as u16;
	hotbar[8] = WIRE_TOOL;
	sim.tick_once(vec![
		SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::Layout {
				hotbar,
				grid: vec![WIRE_EMPTY; digblock_core::inv_grid::GRID_SLOTS],
			},
		},
		place(3, BlockType::Dirt, 4),
	]);
	c.drain();
	assert_eq!(
		sim.test_block_at(target.0, target.1, target.2),
		Some(BlockType::Dirt),
		"assign-then-place in one tick lands: {:?}",
		c.msgs
	);
	assert_eq!(sim.test_inventory_count(1, BlockType::Dirt), 4);
}

/// The held item rides snapshots: the selected slot's item (hotbar
/// wire encoding) appears on the player's snapshot entry and tracks
/// slot switches.
#[test]
fn held_item_rides_snapshots() {
	use digblock_core::hotbar::{HOTBAR_SLOTS, WIRE_TOOL};
	let mut sim = flat_sim("held");
	let mut c1 = connect(&mut sim, 1);
	let mut c2 = connect(&mut sim, 2);
	wait_live(&mut sim, 1);
	wait_live(&mut sim, 2);
	sim.test_grant(1, BlockType::Dirt, 1); // → the first bar slot
	sim.bench_set_active_tool(1, BlockType::Bucket);

	// Player 1 selects the Dirt slot.
	let mut input0 = aim(1, Vec3::NEG_Y);
	input0.active_slot = 0;
	sim.tick_once(vec![batch(vec![input0], 1)]);
	for _ in 0..6 {
		sim.tick_once(vec![]);
	}
	c2.drain();
	let held_of = |snaps: &[digblock_net::messages::Snapshot], id: u8| {
		snaps
			.iter()
			.rev()
			.find_map(|s| s.others.iter().find(|p| p.id == id).map(|p| p.held))
	};
	assert_eq!(
		held_of(&c2.snaps, 0),
		Some(BlockType::Dirt as u16),
		"player 1 shows the dirt in hand"
	);

	// Switch to the tool slot: the broadcast follows.
	let mut input1 = aim(2, Vec3::NEG_Y);
	input1.active_slot = HOTBAR_SLOTS as u8 - 1;
	sim.tick_once(vec![batch(vec![input1], 1)]);
	for _ in 0..6 {
		sim.tick_once(vec![]);
	}
	c2.snaps.clear();
	c2.drain();
	assert_eq!(
		held_of(&c2.snaps, 0),
		Some(WIRE_TOOL),
		"slot switch reaches remotes"
	);
	let _ = &mut c1;
}

/// The Mason's Arc: one placement lays the FULL line along the face
/// normal — length read off `unlock::arc_cells`, stock debited per
/// block — and a line that does not fit places NOTHING (all or nothing,
/// so both ends always derive one set). Every expectation derived.
#[test]
fn an_arc_places_a_full_line_or_nothing() {
	use digblock_core::skills::{Skill, arc_xp, total_xp_for};
	use digblock_core::unlock::{ARC_AT, arc_cells};
	let mut sim = flat_sim("arcline");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.bench_set_arcana_level(1, ARC_AT);
	let n = arc_cells(ARC_AT) as u32;
	assert!(n > 1, "the first rank must widen the line");
	sim.test_grant(1, BlockType::Stone, n * 2);

	let pos = sim.test_player(1).unwrap().pos;
	// Aim down-and-out at open ground: the line grows straight up off
	// the struck top face.
	let out = Vec3::new(3.0, -2.0, 0.0).normalize();
	let hit = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).expect("ground");
	assert_eq!(hit.normal, (0, 1, 0), "the fixture needs a top face");
	let arc = |sim: &mut Sim, seq: u32| {
		sim.tick_once(vec![SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::PlaceArc {
					block: BlockType::Stone as u16,
				},
				cell: [hit.cell.0, hit.cell.1, hit.cell.2],
				normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
				slot: 0,
			},
		}]);
	};
	c.drain();
	c.msgs.clear();
	arc(&mut sim, 1);
	for i in 1..=n as i32 {
		assert_eq!(
			sim.test_cell_at(hit.cell.0, hit.cell.1 + i, hit.cell.2),
			Some(BlockType::Stone as u16),
			"cell {i} of the line"
		);
	}
	// The award is the line's REACH, sent as the new total.
	c.drain();
	let arcana_total = total_xp_for(ARC_AT) + arc_xp(n as usize);
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::XpGain { skill, total } if *skill == Skill::Arcana as u8 && *total == arcana_total
		)),
		"an arc pays Arcana for the cells past the first: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Stone),
		n,
		"one debit per block of the line"
	);

	// A line that cannot fit places NOTHING: a second clear column with
	// a cap hung where the line's SECOND cell would go, so the first
	// cell fits and the whole line still bounces, stock intact.
	let out2 = Vec3::new(0.0, -2.0, 3.0).normalize();
	let hit2 = raycast(pos, out2, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).expect("ground 2");
	assert_eq!(
		hit2.normal,
		(0, 1, 0),
		"the second fixture needs a top face"
	);
	sim.test_set_cell(
		hit2.cell.0,
		hit2.cell.1 + 2,
		hit2.cell.2,
		BlockType::Stone as u16,
	);
	c.drain();
	c.msgs.clear();
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 2,
			tick: 0,
			origin: pos.to_array(),
			dir: out2.to_array(),
			kind: EditKind::PlaceArc {
				block: BlockType::Stone as u16,
			},
			cell: [hit2.cell.0, hit2.cell.1, hit2.cell.2],
			normal: [
				hit2.normal.0 as i8,
				hit2.normal.1 as i8,
				hit2.normal.2 as i8,
			],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				reason: RejectReason::Occluded,
				..
			}
		)),
		"an obstructed line must bounce wholesale: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Stone),
		n,
		"a bounced line debits nothing"
	);
	assert!(
		!c.msgs.iter().any(|m| matches!(m, ServerMsg::XpGain { .. })),
		"a bounced line pays nothing: {:?}",
		c.msgs
	);
}

/// Placing into a locked region is refused too, and reports
/// `Protected` rather than some incidental reason — the client picks
/// its wording ("can't build" vs "won't break") from this.
#[test]
fn locked_spawn_refuses_a_place() {
	let mut sim = flat_sim("lockplace");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::Stone, 4);
	// Lock a region offset from the player so the target is inside it
	// but the player is not standing in the way of their own place.
	let eye = FLAT_SPAWN + Vec3::new(4.0, 0.0, 0.0);
	sim.spawn_store
		.set_lock(Some(crate::spawn_store::SpawnLock {
			eye: eye.to_array(),
		}));
	let region = sim.protected_regions()[0];

	let pos = sim.test_player(1).unwrap().pos;
	let out = Vec3::new(3.0, -2.0, 0.0).normalize();
	let hit = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).expect("ground");
	let target = (
		hit.cell.0 + hit.normal.0,
		hit.cell.1 + hit.normal.1,
		hit.cell.2 + hit.normal.2,
	);
	assert!(
		region.contains(target.0, target.1, target.2),
		"test aims outside the locked region: {target:?}"
	);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 0,
			origin: pos.to_array(),
			dir: out.to_array(),
			kind: EditKind::Place {
				block: BlockType::Stone as u16,
			},
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				reason: RejectReason::Protected,
				..
			}
		)),
		"protected place should be refused as Protected: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_inventory_count(1, BlockType::Stone),
		4,
		"a refused place must not debit stock"
	);
}

/// The operator who moves spawn and stamps the sites must also be able
/// to repair them: an admin's place inside the lock lands, and their
/// hold is not refused at DigStart — while the sim's own writers keep
/// answering to the full region set (`requester_regions` names the
/// split). The non-admin refusals are the tests around this one.
#[test]
fn an_admin_edits_inside_the_arrival_regions() {
	let mut sim = flat_sim("adminlock");
	sim.set_admin_accounts([admin_account()]);
	let mut c = connect_admin(&mut sim, 1, "boss");
	wait_live(&mut sim, 1);
	sim.test_grant(1, BlockType::Stone, 4);
	let eye = FLAT_SPAWN + Vec3::new(4.0, 0.0, 0.0);
	sim.spawn_store
		.set_lock(Some(crate::spawn_store::SpawnLock {
			eye: eye.to_array(),
		}));
	let region = sim.protected_regions()[0];

	let pos = sim.test_player(1).unwrap().pos;
	let out = Vec3::new(3.0, -2.0, 0.0).normalize();
	let hit = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z)).expect("ground");
	let target = (
		hit.cell.0 + hit.normal.0,
		hit.cell.1 + hit.normal.1,
		hit.cell.2 + hit.normal.2,
	);
	assert!(
		region.contains(target.0, target.1, target.2),
		"test aims outside the locked region: {target:?}"
	);
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::EditRequest {
			seq: 1,
			tick: 0,
			origin: pos.to_array(),
			dir: out.to_array(),
			kind: EditKind::Place {
				block: BlockType::Stone as u16,
			},
			cell: [hit.cell.0, hit.cell.1, hit.cell.2],
			normal: [hit.normal.0 as i8, hit.normal.1 as i8, hit.normal.2 as i8],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		!c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				reason: RejectReason::Protected,
				..
			}
		)),
		"an admin place inside the lock was refused: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_cell_at(target.0, target.1, target.2),
		Some(BlockType::Stone as u16),
		"the admin's block should land"
	);

	// And the hold: a dig start on a protected cell is not refused for
	// the admin (a non-admin's is — the refusal test beside this one).
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigStart {
			cell: [target.0, target.1, target.2],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		!c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::DigRefused {
				reason: RejectReason::Protected,
				..
			}
		)),
		"an admin hold inside the lock was refused: {:?}",
		c.msgs
	);
}

/// Unlocking gives the cells back — the lock is a toggle, not a
/// permanent property of the terrain.
#[test]
fn unlocking_restores_digging() {
	let mut sim = flat_sim("lockundig");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.spawn_store
		.set_lock(Some(crate::spawn_store::SpawnLock {
			eye: FLAT_SPAWN.to_array(),
		}));
	sim.spawn_store.set_lock(None);
	let down = Vec3::new(0.0, -1.0, 0.0);
	let mut t = 0;
	let cell = dig_hold(&mut sim, 1, down, 90, 1, EditKind::Dig, &mut t);
	c.drain();
	assert!(
		!c.msgs.iter().any(|m| matches!(
			m,
			ServerMsg::EditRejected {
				reason: RejectReason::Protected,
				..
			}
		)),
		"dig still refused after unlocking: {:?}",
		c.msgs
	);
	assert_eq!(
		sim.test_cell_at(cell.0, cell.1, cell.2),
		Some(BlockType::Air as u16),
		"the dig did not land after unlocking"
	);
}

/// Water placed OUTSIDE a locked region must not flow into it. This is
/// the case the write-refusal covers that a schedule filter would not:
/// the source is legal, only its destination is protected.
#[test]
fn water_cannot_flow_into_a_locked_region() {
	let mut sim = flat_sim("lockflow");
	// Held, not dropped: dropping the client closes its channel, the
	// server drops the player, and residency evicts the whole world.
	let _c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let spawn = FLAT_SPAWN;
	let sz = spawn.z.floor() as i32;
	let h = spawn.y.floor() as i32 - 2;
	sim.spawn_store
		.set_lock(Some(crate::spawn_store::SpawnLock {
			eye: spawn.to_array(),
		}));
	let region = sim.protected_regions()[0];
	let x1 = region.bounds()[1];

	// A source one column outside the region, level with the capsule,
	// with a clear run straight at it.
	let (wx, wy, wz) = (x1 + 1, h + 1, sz);
	assert!(!region.contains(wx, wy, wz), "source must start outside");
	assert!(sim.store.set_block(wx, wy, wz, BlockType::Water).is_some());
	sim.fluid.schedule_around(wx, wy, wz);
	// Plenty of flow steps for water to spread its full lateral reach.
	for _ in 0..240 {
		sim.tick_once(vec![]);
	}

	for (x, y, z) in region.cells() {
		let cell = sim.test_cell_at(x, y, z);
		assert!(
			!cell.is_some_and(|c| BlockType::from_id(digblock_core::chunk::cell_id(c)).is_water()),
			"water reached protected cell ({x},{y},{z})"
		);
	}
	// And the source itself did flow somewhere — otherwise this test
	// would pass on a fluid sim that simply never ran.
	assert!(
		sim.test_cell_at(x1 + 2, h + 1, sz)
			.is_some_and(|c| BlockType::from_id(digblock_core::chunk::cell_id(c)).is_water()),
		"fluid never spread at all; the test proves nothing"
	);
}

/// The refusal lands when the hold STARTS, not when it completes.
/// Before this, a player held the button through a stone block's full
/// dig time and only then learned it was never going to break.
#[test]
fn a_protected_dig_is_refused_at_dig_start() {
	let mut sim = flat_sim("lockdigstart");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	sim.spawn_store
		.set_lock(Some(crate::spawn_store::SpawnLock {
			eye: FLAT_SPAWN.to_array(),
		}));
	// The block underfoot: inside the locked region and in reach.
	let cell = (
		FLAT_SPAWN.x.floor() as i32,
		FLAT_SPAWN.y.floor() as i32 - 2,
		FLAT_SPAWN.z.floor() as i32,
	);
	assert!(sim.protected_regions()[0].contains(cell.0, cell.1, cell.2));

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
				reason: RejectReason::Protected,
				..
			}
		)),
		"DigStart on a protected cell should be refused immediately: {:?}",
		c.msgs
	);
	// And no hold was recorded, so the timer never runs.
	assert!(
		sim.test_digging(1).is_none(),
		"a refused DigStart still opened a hold"
	);
}

/// An ordinary undiggable target stays silent — the client's own
/// `diggable` check already agrees, so answering would be chatter on
/// every mistimed swing.
#[test]
fn an_ordinary_dig_start_is_not_answered() {
	let mut sim = flat_sim("digstartquiet");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	// Air well above the ground: undiggable, but not protected.
	sim.tick_once(vec![SimEvent::Msg {
		conn_id: 1,
		msg: ClientMsg::DigStart {
			cell: [8, FLAT_GROUND_TOP as i32 + 40, 8],
			slot: 0,
		},
	}]);
	c.drain();
	assert!(
		!c.msgs
			.iter()
			.any(|m| matches!(m, ServerMsg::DigRefused { .. })),
		"undiggable-but-unprotected should stay silent: {:?}",
		c.msgs
	);
}

/// A sealing place starves the grass beneath it into dirt, journaled
/// and broadcast as the SERVER's own consequence; a poured source — a
/// cover you can see past — leaves the turf alive. Both halves land on
/// staged grass via the top face the server's own ray finds.
#[test]
fn a_place_smothers_the_grass_beneath() {
	use digblock_net::protocol::SERVER_ID;
	let mut sim = flat_sim("smother");
	let mut c = connect(&mut sim, 1);
	wait_live(&mut sim, 1);
	let pos = sim.test_player(1).unwrap().pos;

	// Two turf cells, one per ray, each met on its TOP face so the
	// placement lands directly above the grass.
	let mut turf_under = |out: Vec3| {
		let hit = raycast(pos, out, REACH, &|x, y, z| sim.test_cell_at(x, y, z))
			.expect("ground in reach");
		assert_eq!(
			hit.normal,
			(0, 1, 0),
			"the shot must land on top of the turf"
		);
		sim.test_set_cell(hit.cell.0, hit.cell.1, hit.cell.2, BlockType::Grass as u16);
		hit.cell
	};
	let stone_turf = turf_under(Vec3::new(3.0, -2.0, 0.0).normalize());
	let water_turf = turf_under(Vec3::new(-3.0, -2.0, 0.0).normalize());
	let place =
		|seq: u32, out: Vec3, cell: (i32, i32, i32), b: BlockType, slot: u8| SimEvent::Msg {
			conn_id: 1,
			msg: ClientMsg::EditRequest {
				seq,
				tick: 0,
				origin: pos.to_array(),
				dir: out.to_array(),
				kind: EditKind::Place { block: b as u16 },
				cell: [cell.0, cell.1, cell.2],
				normal: [0, 1, 0],
				slot,
			},
		};

	sim.test_grant(1, BlockType::Stone, 1);
	sim.tick_once(vec![place(
		1,
		Vec3::new(3.0, -2.0, 0.0).normalize(),
		stone_turf,
		BlockType::Stone,
		0,
	)]);
	assert_eq!(
		sim.test_block_at(stone_turf.0, stone_turf.1 + 1, stone_turf.2),
		Some(BlockType::Stone)
	);
	assert_eq!(
		sim.test_block_at(stone_turf.0, stone_turf.1, stone_turf.2),
		Some(BlockType::Dirt),
		"a sealing cover smothers"
	);
	c.drain();
	let smother = c.msgs.iter().find_map(|m| match m {
		ServerMsg::EditApplied { by, edit, .. }
			if *by == SERVER_ID && (edit.x, edit.y, edit.z) == stone_turf =>
		{
			Some(edit.block)
		}
		_ => None,
	});
	assert_eq!(
		smother,
		Some(BlockType::Dirt as u16),
		"the consequence broadcasts as the server's own edit"
	);

	// The pour: water above the turf, and the grass lives.
	sim.bench_set_active_tool(1, BlockType::Bucket);
	sim.clients
		.get_mut(&1)
		.unwrap()
		.inventory
		.add(BlockType::Water, 1);
	sim.tick_once(vec![place(
		2,
		Vec3::new(-3.0, -2.0, 0.0).normalize(),
		water_turf,
		BlockType::Water,
		digblock_core::hotbar::HOTBAR_SLOTS as u8 - 1,
	)]);
	assert_eq!(
		sim.test_block_at(water_turf.0, water_turf.1 + 1, water_turf.2),
		Some(BlockType::Water)
	);
	assert_eq!(
		sim.test_block_at(water_turf.0, water_turf.1, water_turf.2),
		Some(BlockType::Grass),
		"a poured source never smothers"
	);
}
