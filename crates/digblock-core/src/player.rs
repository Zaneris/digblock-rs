//! Player physics — a line-for-line port of the Java Player.move():
//! point collider (no AABB), eye at the camera position, ground probe at
//! −1.5, collision probe at −1.25 with 0.25 skin, per-axis slide, floor
//! snap to +0.5, water fully non-solid, unloaded chunks non-solid (the
//! Java null-block behavior). Run at a fixed 60 Hz tick: the ×0.5 ground
//! friction and ÷100 air control are per-tick constants tuned at 60 fps.

use crate::block::BlockType;
use glam::{Vec2, Vec3};

pub const GRAVITY: f32 = 9.8;
pub const TERM_VELOCITY: f32 = -20.0;
pub const WALK_SPEED: f32 = 3.0;
pub const JUMP_IMPULSE: f32 = 5.0;
/// Fixed simulation tick (Java was per-frame at ~60fps).
pub const TICK: f32 = 1.0 / 60.0;

/// Eye spawn position: the nearest land to the origin. Walks expanding
/// square rings (8-block stride) until a column clears the beach line, so
/// spawning tracks the terrain instead of hardcoding a height — and never
/// drops the player into the ocean.
pub fn spawn(seed: f64) -> Vec3 {
	use crate::worldgen::{carved, terrain_height, SEA_LEVEL};
	const STEP: i32 = 8;
	for ring in 0..=64 {
		let d = ring * STEP;
		for z in (-d..=d).step_by(STEP as usize) {
			for x in (-d..=d).step_by(STEP as usize) {
				if x.abs().max(z.abs()) != d {
					continue; // interior already visited on earlier rings
				}
				let h = terrain_height(x, z, seed);
				// Land, and the surface itself isn't a cave mouth.
				if h > SEA_LEVEL + 1 && !carved(x, h, z, seed) {
					return Vec3::new(x as f32 + 0.5, h as f32 + 2.5, z as f32 + 0.5);
				}
			}
		}
	}
	// Pathological seed: no land within 512 blocks — tread water.
	Vec3::new(0.5, SEA_LEVEL as f32 + 3.5, 0.5)
}

/// One tick's worth of input.
#[derive(Default, Clone, Copy)]
pub struct PlayerIntent {
	/// x = strafe, y = forward; clamped to unit length like Java move.nor().
	pub move_axis: Vec2,
	pub jump: bool,
}

pub struct Player {
	/// Eye position (the Java camera position).
	pub pos: Vec3,
	out: Vec3,
	falling: bool,
}

fn cell(v: Vec3) -> (i32, i32, i32) {
	(v.x.floor() as i32, v.y.floor() as i32, v.z.floor() as i32)
}

fn solid(b: Option<BlockType>) -> bool {
	// Java: block != null && != AIR && != WATER. Unloaded (None) is not
	// solid — the player falls through unloaded terrain, as in the Java.
	matches!(b, Some(bt) if bt.props().collides)
}

impl Player {
	pub fn new(spawn: Vec3) -> Self {
		Self {
			pos: spawn,
			out: Vec3::ZERO,
			falling: true,
		}
	}

	fn ground_check(&self, dt: f32, block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>) -> bool {
		if self.out.y >= 0.0 {
			return false;
		}
		let probe = self.pos + self.out * dt + Vec3::new(0.0, -1.5, 0.0);
		let (x, y, z) = cell(probe);
		solid(block_at(x, y, z))
	}

	fn collision(
		&self,
		dt: f32,
		block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>,
	) -> Option<(i32, i32, i32)> {
		let probe = self.pos
			+ self.out * dt
			+ self.out.normalize_or_zero() * 0.25
			+ Vec3::new(0.0, -1.25, 0.0);
		let c = cell(probe);
		solid(block_at(c.0, c.1, c.2)).then_some(c)
	}

	/// One fixed tick. `dir` is the current look direction (the Java
	/// camera direction).
	pub fn tick(
		&mut self,
		dt: f32,
		dir: Vec3,
		intent: PlayerIntent,
		block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>,
	) {
		let dt = dt.min(0.16);
		if !self.falling {
			self.out.x *= 0.5;
			self.out.z *= 0.5;
		}
		self.falling = true;
		self.out.y = (self.out.y - GRAVITY * dt).max(TERM_VELOCITY);

		let mut mv = intent.move_axis;
		if mv.length() > 1.0 {
			mv = mv.normalize();
		}
		let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
		let right = dir.cross(Vec3::Y).normalize_or_zero(); // Java dir × up

		if self.ground_check(dt, block_at) {
			self.pos.y = self.pos.y.floor() + 0.5;
			self.out = flat * (mv.y * WALK_SPEED) + right * (mv.x * WALK_SPEED);
			if intent.jump {
				self.out *= 0.5;
				self.out.y += JUMP_IMPULSE;
			} else {
				self.falling = false;
			}
		} else {
			self.out += flat * (mv.y / 100.0) + right * (mv.x / 100.0);
		}

		let cur = cell(self.pos + Vec3::new(0.0, -1.25, 0.0));
		if let Some(hit) = self.collision(dt, block_at) {
			if cur.0 != hit.0 && cur.2 != hit.2 {
				// Diagonal: try zeroing x, then z, then both (Java order).
				let x = self.out.x;
				self.out.x = 0.0;
				if self.collision(dt, block_at).is_some() {
					self.out.x = x;
					self.out.z = 0.0;
					if self.collision(dt, block_at).is_some() {
						self.out.x = 0.0;
					}
				}
			} else if cur.0 != hit.0 {
				self.out.x = 0.0;
			} else if cur.2 != hit.2 {
				self.out.z = 0.0;
			}
		}
		self.pos += self.out * dt;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Test spawn: in the air above the flat_world ground.
	const TEST_SPAWN: Vec3 = Vec3::new(0.5, 35.0, 0.5);

	fn new_player() -> Player {
		Player::new(TEST_SPAWN)
	}

	/// Flat ground at y < 10: solid below, air above.
	fn flat_world(x: i32, y: i32, z: i32) -> Option<BlockType> {
		let _ = (x, z);
		Some(if y < 10 {
			BlockType::Dirt
		} else {
			BlockType::Air
		})
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
		// Ground top surface at y=10; eye snaps to floor(y)+0.5.
		assert!((p.pos.y - 11.5).abs() < 0.001, "eye at {}", p.pos.y);
	}

	#[test]
	fn walks_at_java_speed() {
		let mut p = new_player();
		tick_n(&mut p, 600, PlayerIntent::default()); // land
		let z0 = p.pos.z;
		tick_n(
			&mut p,
			60,
			PlayerIntent {
				move_axis: Vec2::new(0.0, 1.0),
				jump: false,
			},
		);
		let walked = p.pos.z - z0;
		assert!((walked - WALK_SPEED).abs() < 0.1, "walked {walked} in 1s");
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
	fn wall_blocks_and_slides() {
		// Wall of solid at z >= 12 for y in 9..13; ground below y=10.
		let world = |_x: i32, y: i32, z: i32| -> Option<BlockType> {
			Some(if y < 10 || (z >= 12 && y < 13) {
				BlockType::Dirt
			} else {
				BlockType::Air
			})
		};
		let mut p = new_player();
		for _ in 0..600 {
			p.tick(TICK, Vec3::Z, PlayerIntent::default(), &world);
		}
		// Walk forward (+z) into the wall for 3 seconds.
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
	fn falls_through_unloaded_chunks() {
		let none = |_: i32, _: i32, _: i32| -> Option<BlockType> { None };
		let mut p = new_player();
		for _ in 0..120 {
			p.tick(TICK, Vec3::Z, PlayerIntent::default(), &none);
		}
		assert!(p.pos.y < TEST_SPAWN.y - 10.0, "fell through unloaded world");
	}
}
