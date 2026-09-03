use super::*;

/// Pins `Player::tick` behavior to the bit. If this fails, physics
/// changed: update the expected values AND bump `crate::SIM_VERSION` —
/// client prediction and server adoption replay this exact code, so a
/// behavior change without a version bump desyncs every session.
#[test]
fn golden_trajectory_pins_sim_version() {
	let flat = |_x: i32, y: i32, _z: i32| {
		some_cell(if y < 10 {
			crate::block::BlockType::Stone
		} else {
			crate::block::BlockType::Air
		})
	};
	let mut p = Player::new(Vec3::new(0.5, 12.5, 0.5));
	let diag = Vec3::new(1.0, 0.0, 1.0).normalize();
	let mut from = Vec3::Z;
	for t in 0..180u32 {
		let (intent, dir) = match t {
			0..=59 => (
				PlayerIntent {
					move_axis: Vec2::new(0.0, 1.0),
					jump: false,
				},
				Vec3::Z,
			),
			60 => (
				PlayerIntent {
					move_axis: Vec2::new(0.0, 1.0),
					jump: true,
				},
				diag,
			),
			61..=119 => (
				PlayerIntent {
					move_axis: Vec2::new(1.0, 1.0),
					jump: false,
				},
				diag,
			),
			_ => (
				PlayerIntent {
					move_axis: Vec2::new(0.0, 1.0),
					jump: false,
				},
				Vec3::NEG_X,
			),
		};
		p.tick(TICK, from, dir, intent, &flat);
		from = dir;
	}
	let s = p.state();
	assert_eq!(
		(s.pos.to_array(), s.vel.to_array()),
		([-0.69925743f32, 11.5, 4.964402], [-4.0f32, 0.0, 0.0]),
		"Player::tick behavior changed — update these values AND bump digblock_core::SIM_VERSION"
	);
}

/// The stairs counterpart: pins the ramp branch (ascending support,
/// downhill glue, back-face boxes) to the bit. Same contract as
/// above; added with SIM v7.
#[test]
fn golden_stair_trajectory_pins_sim_version() {
	use crate::chunk::make_cell;
	use crate::shapes::{Facing, stair_state};
	// Flat ground with a three-stair flight ascending +z and a
	// platform beyond; walk up, jump at the top, walk back down.
	let world = |_x: i32, y: i32, z: i32| -> Option<u16> {
		if y < 10 {
			return Some(crate::block::BlockType::Dirt as u16);
		}
		Some(match (y, z) {
			(10, 12) | (11, 13) | (12, 14) => make_cell(
				crate::block::BlockType::WoodStairs,
				stair_state(Facing::PosZ),
			),
			(10..=12, 15..) => crate::block::BlockType::Stone as u16,
			_ => crate::block::BlockType::Air as u16,
		})
	};
	let mut p = Player::new(Vec3::new(0.5, 11.5, 8.5));
	for t in 0..420u32 {
		let (mv, jump) = match t {
			0..=149 => (Vec2::new(0.0, 1.0), false),
			150 => (Vec2::new(0.0, 1.0), true),
			151..=179 => (Vec2::new(0.0, 1.0), false),
			_ => (Vec2::new(0.0, -1.0), false),
		};
		p.tick(
			TICK,
			Vec3::Z,
			Vec3::Z,
			PlayerIntent {
				move_axis: mv,
				jump,
			},
			&world,
		);
	}
	let s = p.state();
	// The last tick lands back on the ground after the round trip —
	// ascent support, the landing-mount run-out, platform jump, and
	// glued descent all feed these bits.
	assert_eq!(
		(s.pos.to_array(), s.vel.to_array()),
		([0.5f32, 11.500007, 8.279938], [0.0f32, 0.0, -4.0]),
		"stair behavior changed — update these values AND bump digblock_core::SIM_VERSION"
	);
}

/// The water counterpart: pins the swim branch (entry damping, sink,
/// swim-up, breach onto a shelf) to the bit. Same contract as above.
#[test]
fn golden_water_trajectory_pins_sim_version() {
	let shelf = |x: i32, y: i32, z: i32| -> Option<u16> {
		let _ = x;
		some_cell(if y < 10 || (z >= 12 && y < 20) {
			crate::block::BlockType::Dirt
		} else if y < 20 {
			crate::block::BlockType::Water
		} else {
			crate::block::BlockType::Air
		})
	};
	// Fall in from above, sink, then swim forward+up and breach out
	// onto the shelf.
	let mut p = Player::new(Vec3::new(0.5, 26.0, 10.5));
	for t in 0..240u32 {
		let intent = if t < 120 {
			PlayerIntent::default()
		} else {
			PlayerIntent {
				move_axis: Vec2::new(0.0, 1.0),
				jump: true,
			}
		};
		p.tick(TICK, Vec3::Z, Vec3::Z, intent, &shelf);
	}
	let s = p.state();
	// The last tick catches the breach arc over the shelf lip — falling
	// in, sinking, swim-up, and the breach all feed these bits.
	assert_eq!(
		(s.pos.to_array(), s.vel.to_array()),
		(
			[0.5f32, 22.090445, 11.964334],
			[0.0f32, 3.856666, 0.6533333]
		),
		"swim behavior changed — update these values AND bump digblock_core::SIM_VERSION"
	);
}
