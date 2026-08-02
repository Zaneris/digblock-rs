//! Player physics — ported from the Java Player.move(): point collider
//! (no AABB), eye at the camera position, ground probe at −1.5,
//! collision probe at −1.25 with 0.25 skin, per-axis slide, floor snap
//! to +0.5, water fully non-solid, unloaded chunks non-solid (the Java
//! null-block behavior). Run at a fixed 60 Hz tick: the ÷60 air control
//! is a per-tick constant tuned at 60 fps.
//! Diverges from the Java: ground movement accelerates toward the input
//! target at WALK_ACCEL instead of snapping to full speed, and momentum
//! carries over edges and landings.

use crate::block::BlockType;
use glam::{Vec2, Vec3};

pub const GRAVITY: f32 = 9.8;
pub const TERM_VELOCITY: f32 = -20.0;
/// Max ground speed, blocks/s.
pub const WALK_SPEED: f32 = 4.0;
/// Ground acceleration toward the input target, blocks/s²: reaches
/// WALK_SPEED from a standstill in WALK_SPEED / WALK_ACCEL seconds.
pub const WALK_ACCEL: f32 = 10.0;
pub const JUMP_IMPULSE: f32 = 5.0;
/// Fixed simulation tick (Java was per-frame at ~60fps).
pub const TICK: f32 = 1.0 / 60.0;

/// Eye spawn position: the nearest land to the origin. Walks expanding
/// square rings (8-block stride) until a column clears the beach line, so
/// spawning tracks the terrain instead of hardcoding a height — and never
/// drops the player into the ocean.
pub fn spawn(seed: f64) -> Vec3 {
	use crate::worldgen::{carved, column_shape, SEA_LEVEL};
	const STEP: i32 = 8;
	for ring in 0..=64 {
		let d = ring * STEP;
		for z in (-d..=d).step_by(STEP as usize) {
			for x in (-d..=d).step_by(STEP as usize) {
				if x.abs().max(z.abs()) != d {
					continue; // interior already visited on earlier rings
				}
				let s = column_shape(x, z, seed);
				let h = s.height;
				// Land (not a river channel), and the surface itself
				// isn't a cave mouth.
				if h > SEA_LEVEL + 1 && s.water <= h && !carved(x, h, z, seed) {
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

/// Everything `Player::tick` reads and writes — the unit of state that
/// snapshots, prediction, and reconciliation move around.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PlayerState {
	pub pos: Vec3,
	pub vel: Vec3,
}

#[derive(Clone)]
pub struct Player {
	/// Eye position (the Java camera position).
	pub pos: Vec3,
	out: Vec3,
}

fn cell(v: Vec3) -> (i32, i32, i32) {
	(v.x.floor() as i32, v.y.floor() as i32, v.z.floor() as i32)
}

/// The two cells a player at eye position `pos` occupies: the eye cell
/// and the −1.25 body probe (the point collider's). Placement rejection
/// (client), the server's InsidePlayer scan, and claim probing must all
/// agree on exactly these — the constant lives here and nowhere else.
pub fn occupied_cells(pos: Vec3) -> [(i32, i32, i32); 2] {
	[cell(pos), cell(pos - Vec3::new(0.0, 1.25, 0.0))]
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
		}
	}

	pub fn state(&self) -> PlayerState {
		PlayerState {
			pos: self.pos,
			vel: self.out,
		}
	}

	pub fn set_state(&mut self, s: PlayerState) {
		self.pos = s.pos;
		self.out = s.vel;
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
		self.out.y = (self.out.y - GRAVITY * dt).max(TERM_VELOCITY);

		let mut mv = intent.move_axis;
		if mv.length() > 1.0 {
			mv = mv.normalize();
		}
		let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
		let right = dir.cross(Vec3::Y).normalize_or_zero(); // Java dir × up

		if self.ground_check(dt, block_at) {
			self.pos.y = self.pos.y.floor() + 0.5;
			// Ground movement is plain kinematics: accelerate toward the
			// input target (direction × WALK_SPEED) at WALK_ACCEL, with
			// velocity as the only state — landings, edge exits, turns
			// and re-presses all fall out of momentum naturally (the
			// Java snapped straight to full speed). Releasing input
			// still stops instantly.
			if mv == Vec2::ZERO {
				self.out = Vec3::ZERO;
			} else {
				let target = (flat * mv.y + right * mv.x) * WALK_SPEED;
				let cur = Vec3::new(self.out.x, 0.0, self.out.z);
				self.out = cur + (target - cur).clamp_length_max(WALK_ACCEL * dt);
			}
			if intent.jump {
				self.out *= 0.5;
				self.out.y += JUMP_IMPULSE;
			}
		} else {
			self.out += flat * (mv.y / 60.0) + right * (mv.x / 60.0);
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
			if (p.pos.y - 11.5).abs() < 0.001 {
				break; // landed
			}
		}
		// Landing speed is just the air-control drift accumulated over
		// the ~1 s flight — well under max walk speed.
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
		// Cliff: ground only at z < 12; walk forward off the edge at
		// full speed and check the first airborne step keeps it (the
		// Java ×0.5 damping lost half).
		let world = |_x: i32, y: i32, z: i32| -> Option<BlockType> {
			Some(if y < 10 && z < 12 {
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
			let grounded_before = (p.pos.y - 11.5).abs() < 0.001;
			p.tick(TICK, Vec3::Z, forward, &world);
			let step = p.pos.z - prev_z;
			prev_z = p.pos.z;
			if grounded_before && p.pos.y < 11.5 - 0.001 {
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
	fn falls_through_unloaded_chunks() {
		let none = |_: i32, _: i32, _: i32| -> Option<BlockType> { None };
		let mut p = new_player();
		for _ in 0..120 {
			p.tick(TICK, Vec3::Z, PlayerIntent::default(), &none);
		}
		assert!(p.pos.y < TEST_SPAWN.y - 10.0, "fell through unloaded world");
	}
}

#[cfg(test)]
mod golden {
	use super::*;

	/// Pins `Player::tick` behavior to the bit. If this fails, physics
	/// changed: update the expected values AND bump `crate::SIM_VERSION` —
	/// client prediction and server adoption replay this exact code, so a
	/// behavior change without a version bump desyncs every session.
	#[test]
	fn golden_trajectory_pins_sim_version() {
		let flat = |_x: i32, y: i32, _z: i32| {
			Some(if y < 10 {
				crate::block::BlockType::Stone
			} else {
				crate::block::BlockType::Air
			})
		};
		let mut p = Player::new(Vec3::new(0.5, 12.5, 0.5));
		let diag = Vec3::new(1.0, 0.0, 1.0).normalize();
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
			p.tick(TICK, dir, intent, &flat);
		}
		let s = p.state();
		assert_eq!(
			(s.pos.to_array(), s.vel.to_array()),
			([-2.3094306f32, 11.5, 5.529869], [-4.0f32, 0.0, 0.0]),
			"Player::tick behavior changed — update these values AND bump digblock_core::SIM_VERSION"
		);
	}
}
