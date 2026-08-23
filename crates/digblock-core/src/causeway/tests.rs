use super::*;
use crate::chunk::make_cell;
use crate::unlock;

const RUNGS: [u32; 3] = [
	unlock::CAUSEWAY_AT,
	unlock::CAUSEWAY_LONG_AT,
	unlock::CAUSEWAY_VAST_AT,
];

fn eye_at(x: f32, feet: f32, z: f32) -> Vec3 {
	Vec3::new(x, feet + crate::player::EYE_TO_FEET, z)
}

/// A plank meets the feet from below and never closes over them: its top
/// lies at or below the feet, and no higher cell would. Derived from
/// [`crate::player::EYE_TO_FEET`] and the cell lattice, so a change to
/// either is caught here rather than by a player sinking into glass.
#[test]
fn a_plank_meets_the_feet_and_never_closes_over_them() {
	let mut n = 0;
	for step in 0..64 {
		let feet = 60.0 + step as f32 * 0.125;
		let c = step_cell(eye_at(3.4, feet, -7.9));
		let top = c.1 as f32 + 1.0;
		assert!(top <= feet, "a plank at {c:?} closes over feet at {feet}");
		assert!(
			top + 1.0 > feet,
			"a plank at {c:?} leaves a gap under feet at {feet}"
		);
		n += 1;
	}
	assert_eq!(n, 64, "the sweep examined nothing");
}

/// The plank sits in the caster's own column, so a road follows where
/// they walk rather than where they look.
#[test]
fn a_plank_stands_in_the_casters_own_column() {
	for (x, z) in [(0.1_f32, 0.9_f32), (-0.1, -0.9), (12.5, -3.5)] {
		let c = step_cell(eye_at(x, 40.0, z));
		assert_eq!((c.0, c.2), (x.floor() as i32, z.floor() as i32));
	}
}

/// A supported player needs nothing — which is why standing still spends
/// no planks, and why no velocity or facing enters the answer for the two
/// ends to disagree about.
#[test]
fn a_supported_player_lays_nothing() {
	let air = |_: i32, _: i32, _: i32| Some(make_cell(BlockType::Air, 0));
	let t = Trail::default();
	assert!(t.wants(eye_at(0.5, 40.0, 0.5), true, &air).is_none());
	assert!(t.wants(eye_at(0.5, 40.0, 0.5), false, &air).is_some());
}

/// Nowhere is not somewhere: a cell the reader has no answer for — an
/// unloaded column, or past the world's edge — takes no plank.
#[test]
fn a_causeway_does_not_reach_past_the_world() {
	let t = Trail::default();
	assert!(
		t.wants(eye_at(0.5, 40.0, 0.5), false, &|_, _, _| None)
			.is_none()
	);
}

/// A plank claims exactly what a Ward's shell claims. Derived by asking
/// `ward::plan` what it does with a world made of each block, rather than
/// restating the predicate — the two spells write one substance and must
/// not drift about where it may go. Counted, or a `claims` that always
/// said no would pass.
#[test]
fn a_plank_claims_exactly_what_a_ward_pane_claims() {
	let (mut yes, mut no) = (0, 0);
	for b in BlockType::ALL {
		let v = make_cell(b, 0);
		let warded = !crate::ward::plan((0, 40, 0), 2, |_| Some(v))
			.expect("a resident world refuses nothing")
			.is_empty();
		assert_eq!(claims(v), warded, "{b:?} splits the two glass spells");
		if warded { yes += 1 } else { no += 1 }
	}
	assert!(yes > 0 && no > 0, "the sweep saw one kind only");
}

/// The safety property the client's own assertion rests on: however many
/// planks are claimed, the road standing at any moment is [`TRAIL`] long,
/// and the plank retired to make room is always the oldest.
#[test]
fn the_road_standing_is_never_longer_than_the_trail() {
	let mut t = Trail::default();
	let mut laid = Vec::new();
	for i in 0..TRAIL * 20 {
		let c = (i as i32, 40, 0);
		let retired = t.lay(c);
		laid.push(c);
		assert!(t.len() <= TRAIL, "the road grew to {}", t.len());
		let owed = laid.len().checked_sub(TRAIL).and_then(|k| k.checked_sub(1));
		assert_eq!(retired, owed.map(|k| laid[k]), "retired out of order");
		assert!(t.holds(c), "the plank just laid is not standing");
	}
	assert_eq!(t.len(), TRAIL);
	assert_eq!(t.take().len(), TRAIL, "taking the road up leaves nothing");
	assert!(t.is_empty());
}

/// A causeway never takes up a pane it did not lay. The rule is
/// structural rather than checked: only what [`Trail::wants`] names can be
/// laid, and it names nothing that is not claimable — a standing pane of
/// the Ward's own glass least of all, which is what stops a road punching
/// a hole in a sphere it crosses.
#[test]
fn a_causeway_never_takes_up_a_pane_it_did_not_lay() {
	let glass = |_: i32, _: i32, _: i32| Some(make_cell(BlockType::Wardglass, 0));
	let mut t = Trail::default();
	let pos = eye_at(0.5, 40.0, 0.5);
	assert!(t.wants(pos, false, &glass).is_none());
	// And a cell already standing in the road is not claimed twice, so the
	// trail cannot hold one plank in two places at once.
	let c = step_cell(pos);
	assert_eq!(t.lay(c), None);
	assert!(
		t.wants(pos, false, &|_, _, _| Some(make_cell(BlockType::Air, 0)))
			.is_none()
	);
	assert_eq!(t.lay(c), None, "the same plank laid twice");
	assert_eq!(t.len(), 1);
}

/// The caster's own claim is always in bounds, and the slack is a bound
/// rather than a gesture — one cell past it is refused on every axis.
#[test]
fn a_claim_is_bounded_by_the_servers_own_feet() {
	let pos = eye_at(8.25, 40.5, -3.75);
	let own = step_cell(pos);
	assert!(claim_ok(own, pos));
	for axis in 0..3 {
		for sign in [-1, 1] {
			let mut c = own;
			let d = sign * CLAIM_SLACK;
			match axis {
				0 => c.0 += d,
				1 => c.1 += d,
				_ => c.2 += d,
			}
			assert!(claim_ok(c, pos), "the slack refuses its own edge");
			match axis {
				0 => c.0 += sign,
				1 => c.1 += sign,
				_ => c.2 += sign,
			}
			assert!(!claim_ok(c, pos), "axis {axis} is unbounded");
		}
	}
}

/// The road only ever lengthens, the fare only ever climbs, and a span of
/// zero is exactly "below the first rung" — the ladder's shape, swept
/// rather than spot-checked.
#[test]
fn the_road_and_its_fare_climb_together() {
	let mut seen = 0;
	for level in 1..=crate::skills::MAX_LEVEL {
		let (secs, fare) = (
			unlock::causeway_secs(level),
			crate::alchemy::causeway_price(level).1,
		);
		assert_eq!(
			secs > 0,
			level >= unlock::CAUSEWAY_AT,
			"level {level} disagrees about whether the spell is castable"
		);
		if level > 1 {
			assert!(secs >= unlock::causeway_secs(level - 1));
			assert!(fare >= crate::alchemy::causeway_price(level - 1).1);
		}
		if RUNGS.contains(&level) {
			seen += 1;
		}
	}
	assert_eq!(seen, RUNGS.len(), "a rung sits past the level cap");
}

/// A rung sells uninterrupted distance, never a discount on it: the fare
/// climbs FASTER than the road lengthens, so every rung costs more per
/// second than the one below. The Ward's "never a free upgrade" rule,
/// which a span that grew on its own would break — compared by cross
/// multiplication, so the property survives any retune of either table.
#[test]
fn a_longer_road_is_never_a_cheaper_second_of_one() {
	for w in RUNGS.windows(2) {
		let (a, b) = (w[0], w[1]);
		let (sa, fa) = (
			unlock::causeway_secs(a),
			crate::alchemy::causeway_price(a).1,
		);
		let (sb, fb) = (
			unlock::causeway_secs(b),
			crate::alchemy::causeway_price(b).1,
		);
		assert!(sb > sa, "the rung at {b} is no longer than the one at {a}");
		assert!(
			u64::from(fb) * u64::from(sa) > u64::from(fa) * u64::from(sb),
			"the rung at {b} ({fb} for {sb}s) undercuts {a} ({fa} for {sa}s)"
		);
	}
}

/// The Causeway is the Ward's glass, so it drains the Ward's essence —
/// one substance, one price line, asked of the tables rather than named.
#[test]
fn both_glass_spells_drain_one_essence() {
	for level in RUNGS {
		assert_eq!(
			crate::alchemy::causeway_price(level).0,
			crate::alchemy::ward_price(level).0
		);
	}
}

/// The whole spell, walked: a player standing on ground casts and NOTHING happens, runs off a
/// cliff and glass takes each step as they cross into air, turns twice without the road losing
/// them, and falls only once the span runs out. Driven through the real
/// [`crate::player::Player::tick`] with nothing stubbed. The control is the same walk with no
/// causeway: without one the player is in free fall by the same tick.
#[test]
fn a_road_carries_a_running_player_off_a_cliff_and_through_two_turns() {
	use crate::player::{EYE_TO_FEET, Player, PlayerIntent, TICK};
	use glam::Vec2;

	/// Solid ground up to the cliff at `EDGE`, air past it, all the way
	/// down. The plank cells the walk lays are the only other writes.
	const EDGE: i32 = 4;
	const GROUND_TOP: f32 = 64.0;
	let base = |x: i32, y: i32, _z: i32| {
		let solid = y < GROUND_TOP as i32 && x <= EDGE;
		make_cell(
			if solid {
				BlockType::Stone
			} else {
				BlockType::Air
			},
			0,
		)
	};

	// Three legs: straight out over the drop, then 45° one way, then 90°
	// back across it. Long enough at `WALK_SPEED` that each leg crosses
	// several cells, so the trail must retire to keep up.
	let legs = [0.0_f32, 45.0, -45.0];
	const LEG_TICKS: usize = 90;

	let walk = |road: bool| {
		let mut world: std::collections::HashMap<Cell, u16> = std::collections::HashMap::new();
		let mut trail = Trail::default();
		let mut player = Player::new(Vec3::new(EDGE as f32 - 0.5, GROUND_TOP + EYE_TO_FEET, 0.5));
		let mut laid: Vec<Cell> = Vec::new();
		let mut lowest_feet = f32::MAX;
		let mut standing_peak = 0;
		for (leg, deg) in legs.iter().enumerate() {
			let (s, c) = deg.to_radians().sin_cos();
			let dir = Vec3::new(c, 0.0, s);
			for _ in 0..LEG_TICKS {
				let intent = PlayerIntent {
					move_axis: Vec2::new(0.0, 1.0),
					jump: false,
				};
				{
					let cell_at = |x: i32, y: i32, z: i32| {
						Some(*world.get(&(x, y, z)).unwrap_or(&base(x, y, z)))
					};
					player.tick(TICK, dir, intent, &cell_at);
				}
				// Sampled AFTER the tick and BEFORE the plank, so a road
				// that only ever catches up cannot hide a drop.
				if leg > 0 || !laid.is_empty() {
					lowest_feet = lowest_feet.min(player.pos.y - EYE_TO_FEET);
				}
				if !road {
					continue;
				}
				let (plank, was_grounded) = {
					let cell_at = |x: i32, y: i32, z: i32| {
						Some(*world.get(&(x, y, z)).unwrap_or(&base(x, y, z)))
					};
					let grounded = player.grounded(&cell_at);
					(trail.wants(player.pos, grounded, &cell_at), grounded)
				};
				// The road lays as the feet cross into air, never ahead of
				// them: a plank laid while still supported is a LEAD, which
				// is a different spell — one that cannot follow a turn,
				// because it has to guess which way the next step goes.
				assert!(
					plank.is_none() || !was_grounded,
					"a plank was laid under a supported player"
				);
				if let Some(cell) = plank {
					world.insert(cell, make_cell(BlockType::Wardglass, 0));
					laid.push(cell);
					if let Some(old) = trail.lay(cell) {
						world.insert(old, make_cell(BlockType::Air, 0));
					}
					standing_peak = standing_peak.max(trail.len());
				}
			}
			let _ = leg;
		}
		(player, world, trail, laid, lowest_feet, standing_peak)
	};

	// The control: the same walk with no road under it.
	let (fell, ..) = walk(false);
	let fell_to = fell.pos.y - EYE_TO_FEET;
	assert!(
		fell_to < GROUND_TOP - 1.0,
		"the control never left the ground, so the cliff is not a cliff"
	);

	let (mut player, mut world, mut trail, laid, lowest_feet, standing_peak) = walk(true);

	// Standing still on the ground before any of it: the cast alone lays
	// nothing, which is why a road spends no planks until it is needed.
	assert!(
		!laid.is_empty(),
		"the road laid nothing over a cliff it was walked off"
	);
	assert!(
		laid.iter().all(|c| c.0 > EDGE),
		"a plank landed over solid ground"
	);

	// The whole promise: the feet never dropped below the ground they
	// walked off. Derived from the cliff's own height, not a tolerance.
	assert!(
		lowest_feet >= GROUND_TOP,
		"the road let the player fall to {lowest_feet}, below the cliff at {GROUND_TOP}"
	);
	assert_eq!(
		player.pos.y - EYE_TO_FEET,
		GROUND_TOP,
		"the walk did not end level with the cliff it started on"
	);

	// It kept up through both turns, and retired to do it.
	let turned: Vec<i32> = laid.iter().map(|c| c.2).collect();
	assert!(
		turned.iter().copied().max() > turned.iter().copied().min(),
		"the road never left the axis it started on, so no turn was tested"
	);
	// More than a full road's worth laid, so the trail turned over at
	// least once and retirement is on the path this test walked.
	assert!(
		laid.len() >= TRAIL * 2,
		"only {} planks over three legs — the trail never turned over",
		laid.len()
	);
	assert!(
		player.pos.x > EDGE as f32 + TRAIL as f32,
		"the walk ended at x={}, barely past the cliff at {EDGE}",
		player.pos.x
	);
	assert_eq!(standing_peak, TRAIL, "the road never filled");
	assert_eq!(trail.len(), TRAIL);
	let glass = |w: &std::collections::HashMap<Cell, u16>| {
		w.values()
			.filter(|&&v| BlockType::from_id(crate::chunk::cell_id(v)) == BlockType::Wardglass)
			.count()
	};
	assert_eq!(glass(&world), TRAIL, "more road stood than the trail holds");

	// The span runs out: the road comes up, and the player falls.
	for c in trail.take() {
		world.insert(c, make_cell(BlockType::Air, 0));
	}
	assert_eq!(glass(&world), 0, "expiry left glass standing");
	for _ in 0..LEG_TICKS {
		let cell_at =
			|x: i32, y: i32, z: i32| Some(*world.get(&(x, y, z)).unwrap_or(&base(x, y, z)));
		player.tick(
			TICK,
			Vec3::X,
			PlayerIntent {
				move_axis: Vec2::ZERO,
				jump: false,
			},
			&cell_at,
		);
	}
	assert!(
		player.pos.y - EYE_TO_FEET < GROUND_TOP - 1.0,
		"the player did not fall once the road came up"
	);
}

/// A held jump climbs the road. Not a side effect to be fixed later: the rule is "the feet have
/// no support, so give them some", and a jump is the feet having no support. Pinned so it reads
/// as INTENDED — stopping it would need a predicate separating a fall from a jump, per-tick
/// movement state the two ends would have to agree about, the surface MAGIC-DESIGN §1 avoids.
#[test]
fn a_held_jump_climbs_the_road() {
	use crate::player::{EYE_TO_FEET, Player, PlayerIntent, TICK};
	use glam::Vec2;

	const GROUND_TOP: f32 = 64.0;
	let base = |_x: i32, y: i32, _z: i32| {
		make_cell(
			if y < GROUND_TOP as i32 {
				BlockType::Stone
			} else {
				BlockType::Air
			},
			0,
		)
	};
	let mut world: std::collections::HashMap<Cell, u16> = std::collections::HashMap::new();
	let mut trail = Trail::default();
	let mut player = Player::new(Vec3::new(0.5, GROUND_TOP + EYE_TO_FEET, 0.5));
	let mut heights: Vec<i32> = Vec::new();
	// Long enough for several hops at the jump's own hang time, which is
	// what `JUMP_IMPULSE` and `GRAVITY` decide between them.
	for _ in 0..900 {
		let intent = PlayerIntent {
			move_axis: Vec2::ZERO,
			jump: true,
		};
		{
			let cell_at =
				|x: i32, y: i32, z: i32| Some(*world.get(&(x, y, z)).unwrap_or(&base(x, y, z)));
			player.tick(TICK, Vec3::X, intent, &cell_at);
		}
		let plank = {
			let cell_at =
				|x: i32, y: i32, z: i32| Some(*world.get(&(x, y, z)).unwrap_or(&base(x, y, z)));
			let grounded = player.grounded(&cell_at);
			trail.wants(player.pos, grounded, &cell_at)
		};
		if let Some(cell) = plank {
			world.insert(cell, make_cell(BlockType::Wardglass, 0));
			heights.push(cell.1);
			if let Some(old) = trail.lay(cell) {
				world.insert(old, make_cell(BlockType::Air, 0));
			}
		}
	}
	assert!(
		heights.len() > TRAIL,
		"only {} planks — the climb never turned the trail over",
		heights.len()
	);
	// A staircase, not a pile: every plank stands above the last.
	assert!(
		heights.windows(2).all(|w| w[1] > w[0]),
		"the climb doubled back: {heights:?}"
	);
	assert!(
		player.pos.y - EYE_TO_FEET > GROUND_TOP + TRAIL as f32,
		"a held jump climbed to {} from {GROUND_TOP}",
		player.pos.y - EYE_TO_FEET
	);
	// And the road behind it is still just a road.
	assert_eq!(trail.len(), TRAIL);
}
