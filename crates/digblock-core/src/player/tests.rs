use super::*;
use crate::worldgen::Seed;

/// Test spawn: in the air above the flat_world ground.
const TEST_SPAWN: Vec3 = Vec3::new(0.5, 35.0, 0.5);

fn new_player() -> Player {
	Player::new(TEST_SPAWN)
}

/// The top of [`flat_world`]'s ground, and the eye height of somebody
/// standing on it. Named because the resting eye was written out as a
/// bare literal in some twenty places: it is the fixture's floor plus
/// the capsule's own offset, so retuning the capsule moves this line
/// instead of every test that lands on the ground.
const GROUND: i32 = 10;
const GROUND_EYE: f32 = GROUND as f32 + EYE_TO_FEET;

/// Flat ground: solid below the surface line, air above.
fn flat_world(x: i32, y: i32, z: i32) -> Option<u16> {
	let _ = (x, z);
	some_cell(if y < GROUND {
		BlockType::Dirt
	} else {
		BlockType::Air
	})
}

/// The anchor each seed resolves to. A drift here means players of an existing world wake up
/// somewhere else, so these move only for a deliberate rule change — never as a side effect.
///
/// Last moved when the disc test became a real levelness check: the old rule permitted neighbours
/// at `h + 1` and said nothing about those below `h`, so spawns could sit boxed in or on a lip.
#[test]
fn spawn_positions_are_pinned() {
	for (seed, want) in [
		(42.0f64, Vec3::new(0.5, 159.5, -7.5)),
		(1234.5678, Vec3::new(120.5, 137.5, 32.5)),
		(7.0, Vec3::new(-15.5, 136.5, 24.5)),
		(99.0, Vec3::new(200.5, 132.5, -135.5)),
		(2024.0, Vec3::new(72.5, 133.5, -215.5)),
		(0.0, Vec3::new(-39.5, 153.5, 72.5)),
		(-3.5, Vec3::new(-71.5, 136.5, -87.5)),
		(555.0, Vec3::new(-15.5, 155.5, -15.5)),
		(31337.0, Vec3::new(112.5, 146.5, -47.5)),
		(8675309.0, Vec3::new(96.5, 132.5, -95.5)),
	] {
		assert_eq!(spawn(Seed::current(seed)), want, "seed {seed}");
	}
}

/// `ascend_clear` steps over blocks built into the spawn capsule,
/// leaves a clear spawn alone, treats unloaded as clear, and gives up
/// (returns the input) on a column solid to the ceiling.
#[test]
fn ascend_clear_steps_over_built_obstructions() {
	let eye = Vec3::new(0.5, GROUND_EYE, 0.5); // standing on flat_world ground
	assert_eq!(ascend_clear(eye, &flat_world), eye);
	assert_eq!(ascend_clear(eye, &|_, _, _| None), eye);

	// Two blocks stacked through the body: the landing clears them.
	let tower = |x: i32, y: i32, z: i32| {
		if (x, z) == (0, 0) && (11..=12).contains(&y) {
			some_cell(BlockType::Stone)
		} else {
			flat_world(x, y, z)
		}
	};
	let up = ascend_clear(eye, &tower);
	assert!(up.y > eye.y, "raised above the tower");
	assert!(
		!occupied_cells(up).any(|c| solid(tower(c.0, c.1, c.2))),
		"landing is clear"
	);

	let solid_world = |_: i32, _: i32, _: i32| some_cell(BlockType::Stone);
	assert_eq!(ascend_clear(eye, &solid_world), eye, "ceiling gives up");
}

fn tick_n(p: &mut Player, n: usize, intent: PlayerIntent) {
	for _ in 0..n {
		p.tick(TICK, Vec3::Z, intent, &flat_world);
	}
}

#[test]
fn falls_and_lands_snapped() {
	let mut p = new_player();
	tick_n(&mut p, 600, PlayerIntent::default());
	// Landing snaps the eye to EYE_TO_FEET over the ground's top plane.
	assert!((p.pos.y - GROUND_EYE).abs() < 0.001, "eye at {}", p.pos.y);
}

#[test]
fn walks_at_max_speed_once_accelerated() {
	let mut p = new_player();
	tick_n(&mut p, 600, PlayerIntent::default()); // land
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	tick_n(&mut p, 60, forward); // accelerate to max
	let z0 = p.pos.z;
	tick_n(&mut p, 60, forward);
	let walked = p.pos.z - z0;
	assert!((walked - WALK_SPEED).abs() < 0.1, "walked {walked} in 1s");
}

#[test]
fn walk_accelerates_from_a_standstill() {
	let mut p = new_player();
	tick_n(&mut p, 600, PlayerIntent::default()); // land
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	// Over the spin-up window speed averages roughly half of max:
	// well short of full-speed distance, well above zero.
	let window = WALK_SPEED / WALK_ACCEL;
	let ticks = (window / TICK) as usize;
	let z0 = p.pos.z;
	tick_n(&mut p, ticks, forward);
	let walked = p.pos.z - z0;
	let full = WALK_SPEED * window;
	assert!(walked < 0.7 * full, "accelerating, not instant: {walked}");
	assert!(walked > 0.3 * full, "but moving: {walked}");
	// Releasing input stops instantly; the next first-tick step
	// starts from zero again.
	tick_n(&mut p, 10, PlayerIntent::default());
	let z1 = p.pos.z;
	tick_n(&mut p, 1, forward);
	let step = p.pos.z - z1;
	assert!(step < 0.5 * WALK_SPEED * TICK, "restarted slow: {step}");
}

#[test]
fn airtime_grants_only_air_control_speed() {
	let mut p = new_player();
	tick_n(&mut p, 600, PlayerIntent::default()); // land
	// Jump from a standstill, holding forward for the whole flight.
	p.tick(
		TICK,
		Vec3::Z,
		PlayerIntent {
			move_axis: Vec2::new(0.0, 1.0),
			jump: true,
		},
		&flat_world,
	);
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..200 {
		p.tick(TICK, Vec3::Z, forward, &flat_world);
		if (p.pos.y - GROUND_EYE).abs() < 0.001 {
			break; // landed
		}
	}
	// Landing speed is just the air-control drift accumulated over
	// the flight — well under max walk speed.
	let z0 = p.pos.z;
	tick_n(&mut p, 1, forward);
	let step = p.pos.z - z0;
	assert!(step < 0.5 * WALK_SPEED * TICK, "landed slow: {step}");
}

#[test]
fn jumps_and_returns() {
	let mut p = new_player();
	tick_n(&mut p, 600, PlayerIntent::default());
	let ground_y = p.pos.y;
	p.tick(
		TICK,
		Vec3::Z,
		PlayerIntent {
			move_axis: Vec2::ZERO,
			jump: true,
		},
		&flat_world,
	);
	let mut peak = ground_y;
	for _ in 0..120 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &flat_world);
		peak = peak.max(p.pos.y);
	}
	assert!(peak > ground_y + 0.8, "peak {peak} vs ground {ground_y}");
	assert!((p.pos.y - ground_y).abs() < 0.001, "returned to ground");
}

#[test]
fn walking_off_an_edge_keeps_momentum() {
	// Cliff: the ground stops at an edge ahead. Walk forward off it
	// at full speed and check the first airborne step keeps it (the
	// Java ×0.5 damping lost half).
	let world = |_x: i32, y: i32, z: i32| -> Option<u16> {
		some_cell(if y < 10 && z < 12 {
			BlockType::Dirt
		} else {
			BlockType::Air
		})
	};
	let mut p = new_player();
	for _ in 0..600 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
	}
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	let mut prev_z = p.pos.z;
	let mut exit_step = 0.0f32;
	for _ in 0..600 {
		let grounded_before = (p.pos.y - GROUND_EYE).abs() < 0.001;
		p.tick(TICK, Vec3::Z, forward, &world);
		let step = p.pos.z - prev_z;
		prev_z = p.pos.z;
		if grounded_before && p.pos.y < GROUND_EYE - 0.001 {
			exit_step = step;
			break;
		}
	}
	assert!(exit_step > 0.0, "walked off the edge");
	assert!(
		exit_step > 0.75 * WALK_SPEED * TICK,
		"kept most speed: {exit_step}"
	);
}

#[test]
fn wall_blocks_and_slides() {
	// The slide half: walking diagonally into the wall keeps the
	// tangential component moving while the normal one is refused —
	// losing it would glue a player to every wall they brush.
	{
		let world = |_x: i32, y: i32, z: i32| -> Option<u16> {
			some_cell(if y < 10 || (z >= 12 && y < 13) {
				BlockType::Dirt
			} else {
				BlockType::Air
			})
		};
		let mut p = new_player();
		for _ in 0..600 {
			p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
		}
		let x0 = p.pos.x;
		for _ in 0..180 {
			p.tick(
				TICK,
				Vec3::Z,
				PlayerIntent {
					move_axis: Vec2::new(1.0, 1.0),
					jump: false,
				},
				&world,
			);
		}
		assert!(
			p.pos.z < 12.0,
			"diagonal stopped at the wall, z = {}",
			p.pos.z
		);
		// The free diagonal covers this much along x; the wall must not
		// eat more than half of it.
		let diag_x = WALK_SPEED * 180.0 * TICK / std::f32::consts::SQRT_2;
		assert!(
			(p.pos.x - x0).abs() > diag_x * 0.5,
			"slide lost: moved {} of {diag_x}",
			p.pos.x - x0
		);
	}
	// A solid wall ahead, taller than the capsule, over flat ground.
	let world = |_x: i32, y: i32, z: i32| -> Option<u16> {
		some_cell(if y < 10 || (z >= 12 && y < 13) {
			BlockType::Dirt
		} else {
			BlockType::Air
		})
	};
	let mut p = new_player();
	for _ in 0..600 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
	}
	// Walk forward (+z) into the wall for several seconds.
	for _ in 0..180 {
		p.tick(
			TICK,
			Vec3::Z,
			PlayerIntent {
				move_axis: Vec2::new(0.0, 1.0),
				jump: false,
			},
			&world,
		);
	}
	assert!(p.pos.z < 12.0, "stopped before wall, z = {}", p.pos.z);
}

#[test]
fn state_roundtrip_preserves_trajectory() {
	// Two players, identical inputs; one has its state extracted and
	// restored mid-air. Both must land bit-identically — reconciliation
	// rewind/replay depends on state()/set_state() being lossless.
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.3, 1.0),
		jump: false,
	};
	let mut a = new_player();
	let mut b = new_player();
	for i in 0..300 {
		let intent = if i == 50 {
			PlayerIntent {
				move_axis: forward.move_axis,
				jump: true,
			}
		} else {
			forward
		};
		a.tick(TICK, Vec3::Z, intent, &flat_world);
		b.tick(TICK, Vec3::Z, intent, &flat_world);
		if i == 80 {
			let s = b.state();
			b = new_player();
			b.set_state(s);
		}
	}
	assert_eq!(a.state(), b.state());
}

#[test]
fn ceiling_stops_jump() {
	// A flat ceiling two blocks over the floor. The crown must stop at
	// the ceiling plane (the point collider sailed straight through).
	let world = |_x: i32, y: i32, _z: i32| -> Option<u16> {
		some_cell(if y < 10 || y == 12 {
			BlockType::Dirt
		} else {
			BlockType::Air
		})
	};
	let mut p = Player::new(Vec3::new(0.5, GROUND_EYE, 0.5));
	p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world); // settle
	p.tick(
		TICK,
		Vec3::Z,
		PlayerIntent {
			move_axis: Vec2::ZERO,
			jump: true,
		},
		&world,
	);
	let mut peak = p.pos.y;
	for _ in 0..120 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
		peak = peak.max(p.pos.y);
	}
	assert!(peak <= 12.0 - EYE_TO_CROWN + 1e-3, "head stopped: {peak}");
	assert!(peak > GROUND_EYE, "still jumped: {peak}");
	assert!(
		(p.pos.y - GROUND_EYE).abs() < 0.001,
		"returned: {}",
		p.pos.y
	);
}

#[test]
fn one_block_hole_blocked() {
	// A wall ahead with one cell open at ground level: a one-block
	// hole the capsule cannot fit through (the point collider walked
	// straight in).
	let world = |_x: i32, y: i32, z: i32| -> Option<u16> {
		some_cell(if y < 10 || (z >= 12 && (11..14).contains(&y)) {
			BlockType::Dirt
		} else {
			BlockType::Air
		})
	};
	let mut p = new_player();
	for _ in 0..600 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
	}
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..180 {
		p.tick(TICK, Vec3::Z, forward, &world);
	}
	assert!(p.pos.z < 11.75, "blocked at the hole, z = {}", p.pos.z);
}

#[test]
fn two_block_corridor_passes() {
	// A wall ahead opened to two cells of height: a corridor the
	// capsule walks through.
	let world = |_x: i32, y: i32, z: i32| -> Option<u16> {
		some_cell(if y < 10 || (z >= 12 && y >= 12) {
			BlockType::Dirt
		} else {
			BlockType::Air
		})
	};
	let mut p = new_player();
	for _ in 0..600 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
	}
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..240 {
		p.tick(TICK, Vec3::Z, forward, &world);
	}
	assert!(p.pos.z > 13.0, "walked through, z = {}", p.pos.z);
}

#[test]
fn jump_mounts_one_block_step() {
	// A one-block step ahead under open sky: hold forward + jump and
	// end up standing on it.
	let world = |_x: i32, y: i32, z: i32| -> Option<u16> {
		some_cell(if y < 10 || (z >= 12 && y == 10) {
			BlockType::Dirt
		} else {
			BlockType::Air
		})
	};
	let mut p = new_player();
	for _ in 0..600 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
	}
	let hold = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: true,
	};
	for _ in 0..600 {
		p.tick(TICK, Vec3::Z, hold, &world);
	}
	// Release jump and settle: hopping stops, feet find the step top.
	for _ in 0..120 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
	}
	assert!(p.pos.z > 12.0, "advanced onto the step, z = {}", p.pos.z);
	assert!(
		(p.pos.y - (GROUND_EYE + 1.0)).abs() < 0.001,
		"standing on it, y = {}",
		p.pos.y
	);
}

#[test]
fn jump_clears_step_under_three_high_ceiling() {
	// Same step, but under a flat ceiling three blocks over the floor.
	// Head clearance while mounting is the marginal case the capsule
	// must still clear.
	let world = |_x: i32, y: i32, z: i32| -> Option<u16> {
		some_cell(if y < 10 || (z >= 12 && y == 10) || y == 13 {
			BlockType::Dirt
		} else {
			BlockType::Air
		})
	};
	let mut p = Player::new(Vec3::new(0.5, GROUND_EYE, 0.5));
	let hold = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: true,
	};
	for _ in 0..600 {
		p.tick(TICK, Vec3::Z, hold, &world);
	}
	// Release jump and settle: hopping stops, feet find the step top.
	for _ in 0..120 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
	}
	assert!(p.pos.z > 12.0, "advanced onto the step, z = {}", p.pos.z);
	assert!(
		(p.pos.y - (GROUND_EYE + 1.0)).abs() < 0.001,
		"standing on it, y = {}",
		p.pos.y
	);
}

#[test]
fn falls_through_unloaded_chunks() {
	let none = |_: i32, _: i32, _: i32| -> Option<u16> { None };
	let mut p = new_player();
	for _ in 0..120 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &none);
	}
	assert!(p.pos.y < TEST_SPAWN.y - 10.0, "fell through unloaded world");
}

// ---- swimming --------------------------------------------------------

/// Solid ground under a deep column of water sources, air above. The
/// visible surface sits `WATER_SURFACE_DROP` below the top water
/// cell's lattice plane.
fn water_world(x: i32, y: i32, z: i32) -> Option<u16> {
	let _ = (x, z);
	some_cell(if y < 10 {
		BlockType::Dirt
	} else if y < 20 {
		BlockType::Water
	} else {
		BlockType::Air
	})
}

/// `water_world` plus a solid shelf ahead whose top surface sits level
/// with the water cells' tops.
fn shelf_world(x: i32, y: i32, z: i32) -> Option<u16> {
	if z >= 12 && y < 20 {
		return some_cell(BlockType::Dirt);
	}
	water_world(x, y, z)
}

const SWIM: PlayerIntent = PlayerIntent {
	move_axis: Vec2::new(0.0, 1.0),
	jump: true,
};

#[test]
fn sinks_slowly_in_water() {
	let mut p = Player::new(Vec3::new(0.5, 15.0, 0.5));
	let mut min_vy = 0.0f32;
	for _ in 0..60 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &water_world);
		min_vy = min_vy.min(p.state().vel.y);
	}
	assert!(min_vy >= WATER_SINK_SPEED - 1e-3, "sink capped: {min_vy}");
	let dropped = 15.0 - p.pos.y;
	// Ramp to the sink target then coast: the drop can approach but
	// never outrun the sink cap over the window, and it must be a
	// fraction of what dry gravity takes in the same second.
	assert!(dropped > 0.0, "never sank");
	assert!(
		dropped <= -WATER_SINK_SPEED * 60.0 * TICK + 1e-3,
		"outran the sink cap: {dropped}"
	);
	let mut dry = Player::new(Vec3::new(0.5, 15.0, 0.5));
	for _ in 0..60 {
		dry.tick(TICK, Vec3::Z, PlayerIntent::default(), &|_, _, _| {
			some_cell(BlockType::Air)
		});
	}
	let dry_drop = 15.0 - dry.pos.y;
	assert!(
		dropped < dry_drop / 2.0,
		"water hardly slowed the fall: {dropped} vs {dry_drop} dry"
	);
}

#[test]
fn swim_up_rises() {
	let mut p = Player::new(Vec3::new(0.5, 12.0, 0.5));
	let hold = PlayerIntent {
		move_axis: Vec2::ZERO,
		jump: true,
	};
	for _ in 0..120 {
		p.tick(TICK, Vec3::Z, hold, &water_world);
		assert!(
			p.state().vel.y <= SWIM_UP_SPEED + 1e-3,
			"rise capped: {}",
			p.state().vel.y
		);
	}
	// Swimming up from depth: most of the climb to the surface, minus
	// the ramp-up and the surface bob band.
	assert!(p.pos.y > 16.0, "rose to {}", p.pos.y);
}

#[test]
fn swims_where_pointed() {
	// Forward input follows the LOOK direction: pointing down dives,
	// pointing up climbs — no Space involved.
	let down = Vec3::new(0.0, -0.995, 0.1).normalize();
	let up = Vec3::new(0.0, 0.995, 0.1).normalize();
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	let mut diver = Player::new(Vec3::new(0.5, 16.0, 0.5));
	for _ in 0..60 {
		diver.tick(TICK, down, forward, &water_world);
	}
	// A near-vertical dive approaches -SWIM_SPEED — clearly past what
	// the passive sink covers over the same second.
	assert!(diver.pos.y < 16.0 - 1.8, "dove to {}", diver.pos.y);

	let mut climber = Player::new(Vec3::new(0.5, 13.0, 0.5));
	for _ in 0..60 {
		climber.tick(TICK, up, forward, &water_world);
	}
	assert!(climber.pos.y > 14.6, "climbed to {}", climber.pos.y);
}

#[test]
fn jump_out_mounts_adjacent_block() {
	// Swimming against the shelf wall, holding forward + jump: the
	// breach fires in the near-surface band and the ordinary landing
	// resolution puts the eye at shelf top + EYE_TO_FEET.
	let mut p = Player::new(Vec3::new(0.5, 18.5, 11.4));
	for _ in 0..600 {
		p.tick(TICK, Vec3::Z, SWIM, &shelf_world);
		if (p.pos.y - 21.5).abs() < 0.001 && p.pos.z > 12.0 {
			return; // mounted the shelf
		}
	}
	panic!("never mounted the shelf: pos {:?}", p.pos);
}

#[test]
fn terminal_fall_damped_on_entry() {
	// Free-fall from high above the pool: enters near terminal
	// velocity, and the water drag must kill the plunge within a
	// few blocks (the gentle sink afterwards is allowed to continue
	// to the floor — only the fast phase is bounded).
	let mut p = Player::new(Vec3::new(0.5, 40.0, 0.5));
	for _ in 0..600 {
		p.tick(TICK, Vec3::Z, PlayerIntent::default(), &water_world);
		if p.pos.y < 20.0 && p.state().vel.y >= WATER_SINK_SPEED - 1e-3 {
			// Recovered to the sink band — the plunge is over.
			assert!(p.pos.y > 16.0, "plunge ran too deep: {}", p.pos.y);
			return;
		}
	}
	panic!("entry plunge never damped");
}

// ---- sub-block shapes ------------------------------------------------

use crate::chunk::make_cell;
use crate::shapes::{Facing, door_state, slab_state, stair_state};

/// A built four-step +x flight: each stair rests on a stone riser,
/// with a flush platform beyond. This is what a staircase a player
/// actually builds looks like; `flight_world` is the floating variant.
fn built_flight(x: i32, y: i32, z: i32) -> Option<u16> {
	let _ = z;
	if y < 10 {
		return some_cell(BlockType::Dirt);
	}
	let step = x - 12; // stair at y = 10 + step, riser fill below it
	if (0..=3).contains(&step) {
		if y == 10 + step {
			return Some(make_cell(BlockType::WoodStairs, stair_state(Facing::PosX)));
		}
		if y < 10 + step {
			return some_cell(BlockType::Stone);
		}
	}
	if x >= 16 && y <= 13 {
		return some_cell(BlockType::Stone);
	}
	some_cell(BlockType::Air)
}

/// The per-tick rise a walk covers on a 45° ramp — the mount budget,
/// spent continuously.
const RAMP_DY: f32 = super::MOUNT_RATE * TICK;

/// Walking a flight is ONE continuous climb, not a series of hops: the feet move at the rate the
/// ramp under them carries the walk, every tick, in both directions — never faster, and never
/// backwards. Ascent failed both halves before SIM v10: brushing the next step's cell popped the
/// feet a full RADIUS early, many times the honest per-tick rise, and the downhill glue then
/// clawed them back onto the step below. Held for both flight styles.
#[test]
fn stair_flight_climbs_at_a_constant_rate() {
	for (name, world) in [
		(
			"built",
			&built_flight as &dyn Fn(i32, i32, i32) -> Option<u16>,
		),
		("floating", &flight_world),
	] {
		// flight_world ascends +z to a platform; built_flight ascends
		// +x to a taller one. Same walk in each one's own axis: up the
		// flight, then back down it.
		let along_x = name == "built";
		let dir = if along_x { Vec3::X } else { Vec3::Z };
		let (start, top) = if along_x {
			(Vec3::new(10.5, GROUND_EYE, 0.5), 14.0)
		} else {
			(Vec3::new(0.5, GROUND_EYE, 8.5), 13.0)
		};
		let mut p = Player::new(start);
		let mut prev = p.pos.y - EYE_TO_FEET;
		// Enough ticks to cross the flight and settle, each way.
		for (leg, axis, ticks) in [("up", 1.0f32, 150), ("down", -1.0, 260)] {
			let walk = PlayerIntent {
				move_axis: Vec2::new(0.0, axis),
				jump: false,
			};
			for t in 0..ticks {
				p.tick(TICK, dir, walk, world);
				let feet = p.pos.y - EYE_TO_FEET;
				let dy = feet - prev;
				prev = feet;
				assert!(
					dy.abs() <= RAMP_DY + 1e-4,
					"{name} {leg}: jumped {dy} at tick {t}, pos {:?}",
					p.pos
				);
				let monotone = if axis > 0.0 { dy >= -1e-4 } else { dy <= 1e-4 };
				assert!(
					monotone,
					"{name} {leg}: reversed {dy} at tick {t}, {:?}",
					p.pos
				);
				// Every state the resolver produces on the flight must
				// read as clear, or the server's adoption envelope
				// rejects the honest claim and the walk rubber-bands.
				assert!(
					!overlaps_solid(p.pos, world),
					"{name} {leg}: inside geometry at tick {t}, pos {:?}",
					p.pos
				);
			}
			let want = if axis > 0.0 { top } else { 10.0 };
			assert!(
				(prev - want).abs() < 1e-3,
				"{name} {leg}: ended at feet {prev}, wanted {want}"
			);
		}
	}
}

/// Flat ground with a three-stair flight ascending +z and a solid
/// platform level with its top beyond.
fn flight_world(_x: i32, y: i32, z: i32) -> Option<u16> {
	if y < 10 {
		return some_cell(BlockType::Dirt);
	}
	match (y, z) {
		(10, 12) | (11, 13) | (12, 14) => {
			Some(make_cell(BlockType::WoodStairs, stair_state(Facing::PosZ)))
		}
		(10..=12, 15..) => some_cell(BlockType::Stone),
		_ => some_cell(BlockType::Air),
	}
}

/// One +x stair with a flush stone landing behind it, the landing's
/// top level with the stair's — the ramp run-out scenario.
fn landing_world(x: i32, y: i32, z: i32) -> Option<u16> {
	let _ = z;
	if y < 10 {
		some_cell(BlockType::Dirt)
	} else if (x, y) == (12, 10) {
		Some(make_cell(BlockType::WoodStairs, stair_state(Facing::PosX)))
	} else if x >= 13 && y == 10 {
		some_cell(BlockType::Stone)
	} else {
		some_cell(BlockType::Air)
	}
}

#[test]
fn stair_mounts_flush_landing_smoothly() {
	// Without the run-out rule this walk ground against the landing's
	// top corner for a visible stall (the wall push cancels the walk
	// while the corner catches the bottom sphere). With it, the mount
	// costs no meaningful time and the climb never dips.
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..90 {
		let prev = p.pos.y;
		p.tick(TICK, Vec3::X, forward, &landing_world);
		assert!(p.pos.y >= prev - 1e-3, "no dip mounting: {:?}", p.pos);
	}
	assert!(p.pos.x > 14.0, "mounted and moved on, x = {}", p.pos.x);
	assert!(
		(p.pos.y - (GROUND_EYE + 1.0)).abs() < 0.001,
		"standing on the landing, y = {}",
		p.pos.y
	);
}

#[test]
fn stair_runout_blocked_by_taller_wall() {
	// A wall behind the stair, tall enough to leave no capsule
	// headroom over the first cube's top, so the run-out exemption
	// must not apply — the wall blocks at the top of the ramp like
	// any wall.
	let world = |x: i32, y: i32, z: i32| -> Option<u16> {
		if x >= 13 && y == 11 {
			return some_cell(BlockType::Stone);
		}
		landing_world(x, y, z)
	};
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..240 {
		p.tick(TICK, Vec3::X, forward, &world);
		assert!(p.pos.y < 12.6, "never climbed the wall: {:?}", p.pos);
	}
	assert!(p.pos.x < 12.75, "blocked at the wall face, x = {}", p.pos.x);
}

#[test]
fn stair_side_wall_never_hoists() {
	// A raised strip BESIDE the ramp, its top flush with the stair's
	// and running the length of the approach: pressing into it
	// diagonally while climbing slides along it, never hoists onto it
	// — the run-out exemption is ahead-of-ascent only, so side faces
	// keep blocking.
	let world = |x: i32, y: i32, z: i32| -> Option<u16> {
		if y < 10 {
			some_cell(BlockType::Dirt)
		} else if y == 10 && z >= 1 {
			some_cell(BlockType::Stone)
		} else if (x, y, z) == (12, 10, 0) {
			Some(make_cell(BlockType::WoodStairs, stair_state(Facing::PosX)))
		} else {
			some_cell(BlockType::Air)
		}
	};
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let press = PlayerIntent {
		move_axis: Vec2::new(0.5, 1.0).normalize(),
		jump: false,
	};
	for _ in 0..240 {
		p.tick(TICK, Vec3::X, press, &world);
		let feet = p.pos.y - EYE_TO_FEET;
		// While the cylinder section overlaps the strip, its face
		// blocks flush a RADIUS out from it; above that only the
		// bottom sphere can legitimately round over the corner, by a
		// bounded amount.
		if feet < 10.69 {
			assert!(p.pos.z < 0.71, "face kept blocking: {:?}", p.pos);
		}
		assert!(p.pos.z < 0.78, "never hoisted over the edge: {:?}", p.pos);
		assert!(
			!overlaps_solid(p.pos, &world),
			"never inside the strip: {:?}",
			p.pos
		);
		// Past the ramp top the strip is the same height as the feet —
		// walking onto it is legal, so the property ends here.
		if p.pos.x > 12.9 {
			break;
		}
	}
	assert!(p.pos.x > 12.9, "climbed the ramp while sliding");
}

#[test]
fn overlaps_solid_runout_matches_resolver() {
	// Mid-mount: feet near the top of the ramp, the bottom sphere
	// grazing the landing's top corner. The resolver exempts the
	// landing cube, so the probe must not call this solid — else the
	// adoption envelope rejects every honest mount claim.
	let mid_mount = Vec3::new(12.8, 10.8 + EYE_TO_FEET, 0.5);
	assert!(!overlaps_solid(mid_mount, &landing_world));
	// The same position against the taller wall is a state the
	// resolver never produces — solid.
	let walled = |x: i32, y: i32, z: i32| -> Option<u16> {
		if x >= 13 && y == 11 {
			return some_cell(BlockType::Stone);
		}
		landing_world(x, y, z)
	};
	assert!(overlaps_solid(mid_mount, &walled));
}

/// Flat ground with one +x-ascending stair standing on it, running the
/// full z.
fn one_stair_world(x: i32, y: i32, z: i32) -> Option<u16> {
	let _ = z;
	if y < 10 {
		some_cell(BlockType::Dirt)
	} else if (x, y) == (12, 10) {
		Some(make_cell(BlockType::WoodStairs, stair_state(Facing::PosX)))
	} else {
		some_cell(BlockType::Air)
	}
}

#[test]
fn stair_back_face_blocks_like_a_wall() {
	// Walking −x into the stair's tall back (+x side): a wall.
	let mut p = Player::new(Vec3::new(14.5, GROUND_EYE, 0.5));
	let toward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..180 {
		p.tick(TICK, Vec3::NEG_X, toward, &one_stair_world);
	}
	assert!(p.pos.x > 13.25, "blocked at the back face, x = {}", p.pos.x);
	assert!(
		(p.pos.y - GROUND_EYE).abs() < 0.001,
		"never climbed: {}",
		p.pos.y
	);
}

#[test]
fn stair_low_side_ramps_up() {
	// Walking +x from the low side: the ramp lifts the player over
	// the cell with no jump, then they drop off the far side.
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	let mut peak = 0.0f32;
	for _ in 0..240 {
		p.tick(TICK, Vec3::X, forward, &one_stair_world);
		peak = peak.max(p.pos.y);
	}
	assert!(peak > 12.3, "ramped up, peak eye {peak}");
	assert!(p.pos.x > 13.5, "carried on past the stair, x = {}", p.pos.x);
}

#[test]
fn stair_under_low_ceiling_wedges() {
	// Stair with a flat ceiling two blocks over its base: the gap
	// pinches below capsule height partway up — the player wedges
	// (bounded, no jitter escape, never through).
	let world = |x: i32, y: i32, z: i32| -> Option<u16> {
		if y == 12 && x >= 12 {
			return some_cell(BlockType::Stone);
		}
		one_stair_world(x, y, z)
	};
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..240 {
		p.tick(TICK, Vec3::X, forward, &world);
		assert!(p.pos.y < 12.75, "never squeezed past the gap: {}", p.pos.y);
	}
	assert!(
		p.pos.x < 13.0,
		"wedged before the far side, x = {}",
		p.pos.x
	);
}

// ---- auto-step (v9) --------------------------------------------------

/// Flat ground with a slab staircase ascending +x, every riser exactly
/// AUTO_STEP-sized: a one-third slab, then a two-thirds slab, then
/// full stone, then a one-third slab standing on top of that.
fn slab_stack_world(x: i32, y: i32, z: i32) -> Option<u16> {
	let _ = z;
	if y < 10 {
		return some_cell(BlockType::Dirt);
	}
	match (x, y) {
		(12, 10) => Some(make_cell(BlockType::WoodSlab, slab_state(false, false))),
		(13, 10) => Some(make_cell(BlockType::WoodSlab, slab_state(true, false))),
		(14, 10) | (15, 10) => some_cell(BlockType::Stone),
		(15, 11) => Some(make_cell(BlockType::WoodSlab, slab_state(false, false))),
		_ => some_cell(BlockType::Air),
	}
}

/// A step up is the same motion as a stair: the feet rise at the ramp rate, no faster, and the
/// mount lands exactly as the axis crosses onto the block. Before SIM v10 each rise was a one-tick
/// teleport of the whole `AUTO_STEP_MAX` — the jarring pop this pins against. Every intermediate
/// state must also pass `overlaps_solid`: a rate-limited mount spends its ticks with the capsule
/// inside the side of the box it climbs, and the server's envelope probes exactly these.
#[test]
fn slab_steps_mount_at_the_ramp_rate() {
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	let mut prev = p.pos.y - EYE_TO_FEET;
	let mut rises = 0;
	for t in 0..300 {
		p.tick(TICK, Vec3::X, forward, &slab_stack_world);
		let feet = p.pos.y - EYE_TO_FEET;
		let dy = feet - prev;
		prev = feet;
		if dy > 1e-4 {
			rises += 1;
		}
		assert!(
			dy <= MOUNT_RATE * TICK + 1e-4,
			"popped {dy} at tick {t}, pos {:?}",
			p.pos
		);
		assert!(dy >= -1e-3, "dipped {dy} at tick {t}, pos {:?}", p.pos);
		assert!(
			!overlaps_solid(p.pos, &slab_stack_world),
			"probe agrees with the resolver at tick {t}: {:?}",
			p.pos
		);
		if p.pos.x > 15.4 {
			break;
		}
	}
	assert!(p.pos.x > 15.4, "climbed the whole stack, x = {}", p.pos.x);
	assert!(
		(prev - (11.0 + 1.0 / 3.0)).abs() < 0.001,
		"topped out on the raised 1/3, feet = {prev}"
	);
	// Four one-third rises, none of them taken in a single tick.
	assert!(
		rises >= 16,
		"spread across ticks, only {rises} rising ticks"
	);
}

/// The mount budget must not turn a LANDING into a slow sink: arriving downward faster than a
/// mount climbs keeps the flat-bottomed snap. Dropped onto the very edge of a slab — axis off the
/// box, so the mount gates would otherwise all pass — the feet must never end a tick below its
/// surface, at any drop height. The sweep matters: how far the first contacting tick overshoots
/// the top depends on the phase of the fall, and only the deep overshoots exceed one tick.
#[test]
fn fast_fall_onto_a_slab_edge_lands_flat() {
	let world = |x: i32, y: i32, z: i32| -> Option<u16> {
		let _ = z;
		if y < 10 {
			some_cell(BlockType::Dirt)
		} else if (x, y) == (12, 10) {
			Some(make_cell(BlockType::WoodSlab, slab_state(false, false)))
		} else {
			some_cell(BlockType::Air)
		}
	};
	const TOP: f32 = 10.0 + 1.0 / 3.0;
	// Resting with the axis off the edge, the bottom sphere sits on
	// the top CORNER, so the feet settle a hair under the surface —
	// RADIUS − √(RADIUS² − d²) for the axis's overhang d. That is the
	// honest rest height; a mount-capped landing sinks an order of
	// magnitude deeper.
	const CORNER_REST: f32 = 0.01;
	for i in 0..40 {
		// Axis just off the slab's -x edge; vary the drop phase.
		let h = 14.5 + i as f32 * 0.05;
		let mut p = Player::new(Vec3::new(11.95, h, 0.5));
		let mut landed = false;
		for t in 0..200 {
			p.tick(TICK, Vec3::X, PlayerIntent::default(), &world);
			let feet = p.pos.y - EYE_TO_FEET;
			landed |= p.state().vel.y == 0.0;
			if landed {
				assert!(
					feet >= TOP - CORNER_REST,
					"drop {h}: sank to {feet} at tick {t} instead of landing on {TOP}"
				);
			}
			assert!(
				!overlaps_solid(p.pos, &world),
				"drop {h}: solid at tick {t}"
			);
		}
		assert!(landed, "drop {h}: never landed");
	}
}

#[test]
fn walks_onto_third_slab_without_jumping() {
	let world = |x: i32, y: i32, z: i32| -> Option<u16> {
		if (x, y) == (13, 10) {
			return some_cell(BlockType::Air);
		}
		slab_stack_world(x, y, z)
	};
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..90 {
		let prev = p.pos.y;
		p.tick(TICK, Vec3::X, forward, &world);
		assert!(p.pos.y >= prev - 1e-3, "no dip stepping up: {:?}", p.pos);
		if p.pos.x > 12.5 {
			break;
		}
	}
	assert!(p.pos.x > 12.5, "walked onto the slab, x = {}", p.pos.x);
	let feet = p.pos.y - EYE_TO_FEET;
	assert!(
		(feet - (10.0 + 1.0 / 3.0)).abs() < 0.001,
		"standing on the 1/3 top, feet = {feet}"
	);
}

#[test]
fn two_thirds_rise_walls() {
	// Ground straight into a two-thirds slab: a rise past
	// `AUTO_STEP_MAX` must block like any wall — no step, no creep.
	let world = |x: i32, y: i32, z: i32| -> Option<u16> {
		let _ = z;
		if y < 10 {
			some_cell(BlockType::Dirt)
		} else if (x, y) == (12, 10) {
			Some(make_cell(BlockType::WoodSlab, slab_state(true, false)))
		} else {
			some_cell(BlockType::Air)
		}
	};
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..240 {
		p.tick(TICK, Vec3::X, forward, &world);
	}
	assert!(p.pos.x < 11.75, "blocked at the face, x = {}", p.pos.x);
	let feet = p.pos.y - EYE_TO_FEET;
	assert!((feet - 10.0).abs() < 0.001, "never lifted, feet = {feet}");
}

#[test]
fn step_blocked_under_low_ceiling() {
	// A one-third slab under a ceiling that leaves less than the
	// capsule's HEIGHT over its top: the step must not jam the crown
	// — the slab blocks like a wall.
	let world = |x: i32, y: i32, z: i32| -> Option<u16> {
		let _ = z;
		if y < 10 {
			some_cell(BlockType::Dirt)
		} else if (x, y) == (12, 10) {
			Some(make_cell(BlockType::WoodSlab, slab_state(false, false)))
		} else if (x, y) == (12, 12) {
			some_cell(BlockType::Stone)
		} else {
			some_cell(BlockType::Air)
		}
	};
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..240 {
		p.tick(TICK, Vec3::X, forward, &world);
	}
	assert!(
		p.pos.x < 11.75,
		"blocked under the ceiling, x = {}",
		p.pos.x
	);
	let feet = p.pos.y - EYE_TO_FEET;
	assert!((feet - 10.0).abs() < 0.001, "never stepped, feet = {feet}");
}

#[test]
fn step_never_mounts_a_door_leaf() {
	// Standing on a two-thirds slab beside a closed door: the leaf's top is
	// exactly one step above the feet, but the mandatory upper half
	// sits directly on it — the headroom probe is the ONLY thing
	// keeping door tops unwalkable from slab height. If this fails
	// after a shape change, revisit `step_contact`'s doc.
	let world = |x: i32, y: i32, z: i32| -> Option<u16> {
		let _ = z;
		if y < 10 {
			some_cell(BlockType::Dirt)
		} else if (x, y) == (11, 10) {
			Some(make_cell(BlockType::WoodSlab, slab_state(true, false)))
		} else if x == 12 && (y == 10 || y == 11) {
			Some(make_cell(
				BlockType::LumberDoor,
				door_state(Facing::PosX, false, y == 11),
			))
		} else {
			some_cell(BlockType::Air)
		}
	};
	// Start standing on the two-thirds slab, walking +x.
	let mut p = Player::new(Vec3::new(11.5, 10.0 + 2.0 / 3.0 + EYE_TO_FEET, 0.5));
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..240 {
		p.tick(TICK, Vec3::X, forward, &world);
	}
	assert!(p.pos.x < 12.0, "blocked at the leaf, x = {}", p.pos.x);
	let feet = p.pos.y - EYE_TO_FEET;
	assert!(
		(feet - (10.0 + 2.0 / 3.0)).abs() < 0.001,
		"never mounted the door, feet = {feet}"
	);
}

#[test]
fn jump_mounts_two_thirds_slab() {
	// A two-thirds rise needs a jump — and the descent phase of the jump
	// (out.y ≤ 0 with the feet inside the step window) completes the
	// mount via the step rule.
	let world = |x: i32, y: i32, z: i32| -> Option<u16> {
		let _ = z;
		if y < 10 {
			some_cell(BlockType::Dirt)
		} else if x >= 12 && y == 10 {
			Some(make_cell(BlockType::WoodSlab, slab_state(true, false)))
		} else {
			some_cell(BlockType::Air)
		}
	};
	let mut p = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let hold = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: true,
	};
	for _ in 0..600 {
		p.tick(TICK, Vec3::X, hold, &world);
	}
	for _ in 0..120 {
		p.tick(TICK, Vec3::X, PlayerIntent::default(), &world);
	}
	assert!(p.pos.x > 12.0, "advanced onto the slab, x = {}", p.pos.x);
	let feet = p.pos.y - EYE_TO_FEET;
	assert!(
		(feet - (10.0 + 2.0 / 3.0)).abs() < 0.001,
		"standing on it, feet = {feet}"
	);
}

/// Ground plus a door pair standing on it ahead of the walker, hinged
/// geometry irrelevant to the test — `open` toggles collision.
fn door_world(open: bool) -> impl Fn(i32, i32, i32) -> Option<u16> {
	move |x: i32, y: i32, z: i32| -> Option<u16> {
		let _ = z;
		if y < 10 {
			some_cell(BlockType::Dirt)
		} else if x == 12 && (y == 10 || y == 11) {
			Some(make_cell(
				BlockType::LumberDoor,
				door_state(Facing::PosX, open, y == 11),
			))
		} else {
			some_cell(BlockType::Air)
		}
	}
}

#[test]
fn closed_door_blocks_open_door_passes() {
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	let mut blocked = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let closed = door_world(false);
	for _ in 0..240 {
		blocked.tick(TICK, Vec3::X, forward, &closed);
	}
	assert!(
		blocked.pos.x < 12.0,
		"closed leaf is a wall, x = {}",
		blocked.pos.x
	);

	let mut walker = Player::new(Vec3::new(10.5, GROUND_EYE, 0.5));
	let opened = door_world(true);
	for _ in 0..240 {
		walker.tick(TICK, Vec3::X, forward, &opened);
	}
	assert!(
		walker.pos.x > 13.5,
		"open door is a doorway, x = {}",
		walker.pos.x
	);
}

#[test]
fn overlaps_solid_is_shape_aware() {
	// Standing ON a tread: feet inside the stair CELL but not its
	// geometry — never "inside solid" (the anti-rubber-band rule).
	let mid = Vec3::new(12.5, 10.5 + EYE_TO_FEET, 0.5); // halfway up the ramp
	assert!(!overlaps_solid(mid, &one_stair_world));
	// Sunk below the ramp surface: solid.
	let buried = mid - Vec3::Y * 0.2;
	assert!(overlaps_solid(buried, &one_stair_world));
	// Cube world sanity: resting on ground clear, dipped solid.
	assert!(!overlaps_solid(
		Vec3::new(0.5, GROUND_EYE, 0.5),
		&flight_world
	));
	assert!(overlaps_solid(Vec3::new(0.5, 11.3, 0.5), &flight_world));
	// Door: the closed leaf is set back from the cell face it points
	// at, so the cell CENTER's capsule now grazes the leaf's front
	// face; clearing it needs a RADIUS beyond that face. Open = no
	// collision geometry at all (the handle knob is interaction-only
	// and never counts).
	let center = Vec3::new(12.5, GROUND_EYE, 0.5);
	let clear = Vec3::new(12.6, GROUND_EYE, 0.5);
	let against = Vec3::new(12.25, GROUND_EYE, 0.5);
	assert!(overlaps_solid(center, &door_world(false)));
	assert!(!overlaps_solid(clear, &door_world(false)));
	assert!(overlaps_solid(against, &door_world(false)));
	assert!(!overlaps_solid(against, &door_world(true)));
}

#[test]
fn placement_blocked_is_strict() {
	let eye = Vec3::new(0.5, GROUND_EYE, 0.5);
	let feet_cell = (0, 10, 0);
	// A stair INTO the player's feet cell is refused — no ramp
	// leniency for placement.
	let stair = make_cell(BlockType::WoodStairs, stair_state(Facing::PosX));
	assert!(placement_blocked(eye, feet_cell, stair));
	assert!(!placement_blocked(eye, (3, 10, 0), stair), "3 blocks away");
	// A closed door leaf blocks only where its thin slab actually
	// reaches the capsule — set back from the cell face, but close
	// enough that the cell center's capsule grazes it; the open state
	// never blocks anywhere.
	let closed = make_cell(
		BlockType::LumberDoor,
		door_state(Facing::PosX, false, false),
	);
	let open = make_cell(BlockType::LumberDoor, door_state(Facing::PosX, true, false));
	let near_leaf = Vec3::new(0.25, GROUND_EYE, 0.5);
	assert!(placement_blocked(near_leaf, feet_cell, closed));
	assert!(placement_blocked(eye, feet_cell, closed), "center grazes");
	assert!(
		!placement_blocked(Vec3::new(0.6, GROUND_EYE, 0.5), feet_cell, closed),
		"front face + RADIUS clears the leaf"
	);
	assert!(!placement_blocked(near_leaf, feet_cell, open));
	// Non-colliding cells never block.
	assert!(!placement_blocked(eye, feet_cell, BlockType::Water as u16));
	// Slab grows under a standing player must reject at EVERY height:
	// a box whose top is above the feet overlaps the capsule (the
	// closest approach is vertical), so growing the cell someone
	// stands in is InsidePlayer — step off first.
	let slab13 = make_cell(BlockType::WoodSlab, slab_state(false, false));
	let slab23 = make_cell(BlockType::WoodSlab, slab_state(true, false));
	assert!(placement_blocked(eye, feet_cell, slab13), "fresh 1/3");
	assert!(placement_blocked(eye, feet_cell, slab23), "grow to 2/3");
	assert!(
		placement_blocked(eye, feet_cell, BlockType::Wood as u16),
		"grow to full base block"
	);
	// Feet exactly ON the grown top clears — the standing-on-top
	// contract (exact contact never reads as overlap).
	assert!(!placement_blocked(
		eye + Vec3::new(0.0, 1.0 / 3.0, 0.0),
		feet_cell,
		slab13
	));
}

#[test]
fn wading_stays_dry_physics() {
	// One block of water on solid ground: submersion stays under
	// SWIM_THRESHOLD — walking speed must be full WALK_SPEED, not
	// SWIM_SPEED.
	let shallow = |x: i32, y: i32, z: i32| -> Option<u16> {
		let _ = (x, z);
		some_cell(if y < 10 {
			BlockType::Dirt
		} else if y < 11 {
			BlockType::Water
		} else {
			BlockType::Air
		})
	};
	let mut p = Player::new(Vec3::new(0.5, GROUND_EYE, 0.5)); // feet on ground
	let forward = PlayerIntent {
		move_axis: Vec2::new(0.0, 1.0),
		jump: false,
	};
	for _ in 0..60 {
		p.tick(TICK, Vec3::Z, forward, &shallow); // accelerate
	}
	let z0 = p.pos.z;
	for _ in 0..60 {
		p.tick(TICK, Vec3::Z, forward, &shallow);
	}
	let waded = p.pos.z - z0;
	assert!((waded - WALK_SPEED).abs() < 0.1, "waded {waded} in 1s");
}
