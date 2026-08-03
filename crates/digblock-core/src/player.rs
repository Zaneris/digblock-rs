//! Player physics: a vertical capsule collider (RADIUS wide, HEIGHT
//! tall, eye EYE_TO_CROWN below the crown top) against the voxel grid.
//! Each tick integrates velocity, then resolves penetration iteratively,
//! deepest contact first. Ground contacts (steep upward normal while
//! descending) land flat on the block plane — the eye rests at exactly
//! top + EYE_TO_FEET anywhere on a block top, no edge slide — while
//! walls slide tangentially and ceilings stop the jump. Water and
//! unloaded chunks are non-solid to COLLISION (the Java null-block
//! behavior); submersion instead switches the integration to the
//! buoyant swim branch (slow sink, held-jump swim-up, surface breach).
//! Run at a fixed 60 Hz tick: the ÷60 air control is a per-tick
//! constant tuned at 60 fps.
//! Diverges from the Java: the point probes grew into a real capsule,
//! ground movement accelerates toward the input target at WALK_ACCEL
//! instead of snapping to full speed, and momentum carries over edges
//! and landings.

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

// --- Swimming. Any change here is a sim-behavior change: re-pin BOTH
// golden trajectories and bump SIM_VERSION. ---------------------------
/// The rendered surface of a resting source sits this far below the
/// cell top (water.wgsl DROPS); physics flips where water LOOKS like
/// it ends. Mirrored by the engine's eye_in_water.
pub const WATER_SURFACE_DROP: f32 = 0.111;
/// Passive sink speed in water when idle, blocks/s (the velocity target
/// with no input; vs TERM_VELOCITY in air).
pub const WATER_SINK_SPEED: f32 = -1.5;
/// Exponential entry drag, 1/s, applied only below WATER_SINK_SPEED:
/// kills a terminal-velocity plunge within ~2 blocks of the surface.
pub const WATER_DRAG: f32 = 10.0;
/// Held-Space rise-speed target, blocks/s.
pub const SWIM_UP_SPEED: f32 = 3.0;
/// Swim speed along the pointed direction, blocks/s (vs WALK_SPEED).
pub const SWIM_SPEED: f32 = 2.5;
/// The one swim ramp: acceleration toward the current velocity target
/// (swim direction, Space-up, or the idle sink), blocks/s².
pub const SWIM_ACCEL: f32 = 8.0;
/// Swimming physics engages above this submersion; below it is wading
/// and the dry rules apply unchanged. One block of water on solid
/// ground wets 0.89/1.8 ≈ 0.49 of the capsule — waist-deep wading (and
/// ankle-deep flow edges, which count as full cells) stays walkable.
const SWIM_THRESHOLD: f32 = 0.5;
/// Jump held at or below this submersion (while moving) fires a full
/// jump impulse — the way out of the pool. Must be ABOVE the swim
/// threshold or the window is unreachable; the band [threshold, this]
/// is what a surfacing swimmer rises through.
const BREACH_SUBMERSION: f32 = 0.62;

// Capsule collider. HEIGHT and RADIUS are the tunables; everything else
// derives from them. Any change here is a sim-behavior change: re-pin
// the golden trajectory, bump SIM_VERSION, and resize the rendered body
// in players.wgsl to match.
/// Total capsule height, feet bottom to crown top.
pub const HEIGHT: f32 = 1.8;
/// Capsule radius.
pub const RADIUS: f32 = 0.3;
/// The eye sits this far below the crown top.
pub const EYE_TO_CROWN: f32 = 0.3;
/// Feet bottom below the eye.
pub const EYE_TO_FEET: f32 = HEIGHT - EYE_TO_CROWN;
/// Axis segment of the two sphere centers, relative to the eye.
const AXIS_LO: f32 = -(EYE_TO_FEET - RADIUS);
const AXIS_HI: f32 = EYE_TO_CROWN - RADIUS;
const _: () = assert!(2.0 * RADIUS <= HEIGHT);

/// Shrink for cell mapping and occupancy: resolution leaves exact
/// contact (distance == RADIUS), which must not read back as overlap.
const CELL_EPS: f32 = 1e-4;
/// Ground-probe depth below the feet.
const GROUND_EPS: f32 = 1e-3;
/// Contacts steeper than this (while descending) count as ground and
/// resolve flat onto the block plane. Radius-independent: an edge
/// support contact has n.y = r/√(o²+r²) > 1/√2 for any overhang o < r.
const GROUND_NORMAL_Y: f32 = 0.7;
const RESOLVE_ITERS: usize = 4;
/// Penetrations shallower than this wait for the next tick.
const SKIN: f32 = 1e-4;

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
				// A spawnable surface kind (excludes water columns), high
				// enough to be dry land, and not a cave mouth.
				if s.surface.props().spawnable && h > SEA_LEVEL + 1 && !carved(x, h, z, seed) {
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

/// Cells covered by the closed interval [lo, hi], shrunk by CELL_EPS so
/// a bound resting exactly on a block plane stays out of the far cell.
fn span(lo: f32, hi: f32) -> std::ops::RangeInclusive<i32> {
	((lo + CELL_EPS).floor() as i32)..=((hi - CELL_EPS).floor() as i32)
}

/// Closest points between the (vertical) capsule axis at eye `pos` and
/// cell `c`'s unit box: (point on axis, point on box). Closed form —
/// the box clamps the axis xz horizontally; vertically, overlapping
/// intervals meet at the overlap (the normal comes out horizontal),
/// otherwise the nearer segment end pairs with the nearer face.
fn closest_axis_box(pos: Vec3, c: (i32, i32, i32)) -> (Vec3, Vec3) {
	let qx = pos.x.clamp(c.0 as f32, c.0 as f32 + 1.0);
	let qz = pos.z.clamp(c.2 as f32, c.2 as f32 + 1.0);
	let (a_lo, a_hi) = (pos.y + AXIS_LO, pos.y + AXIS_HI);
	let (b_lo, b_hi) = (c.1 as f32, c.1 as f32 + 1.0);
	let (py, qy) = if a_lo > b_hi {
		(a_lo, b_hi)
	} else if a_hi < b_lo {
		(a_hi, b_lo)
	} else {
		let y = a_lo.max(b_lo);
		(y, y)
	};
	(Vec3::new(pos.x, py, pos.z), Vec3::new(qx, qy, qz))
}

struct Contact {
	/// Unit push direction, box toward capsule.
	n: Vec3,
	depth: f32,
	cell_y: i32,
}

/// Penetration contact of the capsule at eye `pos` with cell `c`, if any.
fn cell_contact(pos: Vec3, c: (i32, i32, i32)) -> Option<Contact> {
	let (p, q) = closest_axis_box(pos, c);
	let d2 = p.distance_squared(q);
	if d2 >= RADIUS * RADIUS {
		return None;
	}
	let d = d2.sqrt();
	if d > 1e-6 {
		return Some(Contact {
			n: (p - q) / d,
			depth: RADIUS - d,
			cell_y: c.1,
		});
	}
	// Degenerate: the axis runs inside the box (a terminal-velocity
	// landing can bury it ~0.03). Push out through the nearest face, in
	// a fixed order so ties resolve identically everywhere.
	let faces = [
		(Vec3::Y, c.1 as f32 + 1.0 - p.y),
		(Vec3::NEG_Y, p.y - c.1 as f32),
		(Vec3::X, c.0 as f32 + 1.0 - p.x),
		(Vec3::NEG_X, p.x - c.0 as f32),
		(Vec3::Z, c.2 as f32 + 1.0 - p.z),
		(Vec3::NEG_Z, p.z - c.2 as f32),
	];
	let mut best = faces[0];
	for f in &faces[1..] {
		if f.1 < best.1 {
			best = *f;
		}
	}
	Some(Contact {
		n: best.0,
		depth: best.1 + RADIUS,
		cell_y: c.1,
	})
}

/// Every cell the player capsule at eye `pos` overlaps (exact
/// capsule-vs-box test over the bounding cells, 2–12 results).
/// Placement rejection (client), the server's InsidePlayer scan, and
/// claim probing must all agree on exactly these — the geometry lives
/// here and nowhere else. The CELL_EPS shrink means resting contact
/// (feet on a floor, shoulder flush to a wall) does not occupy the
/// touching cell.
pub fn occupied_cells(pos: Vec3) -> impl Iterator<Item = (i32, i32, i32)> {
	let r2 = (RADIUS - CELL_EPS) * (RADIUS - CELL_EPS);
	span(pos.x - RADIUS, pos.x + RADIUS).flat_map(move |x| {
		span(pos.y - EYE_TO_FEET, pos.y + EYE_TO_CROWN).flat_map(move |y| {
			span(pos.z - RADIUS, pos.z + RADIUS).filter_map(move |z| {
				let (p, q) = closest_axis_box(pos, (x, y, z));
				(p.distance_squared(q) < r2).then_some((x, y, z))
			})
		})
	})
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

	/// Standing support probe: any solid cell in the layer just below the
	/// feet whose top rect is within RADIUS of the axis in xz. The disk
	/// matches the resolver's GROUND_NORMAL_Y cutoff, so probe and
	/// resolver agree on where standing ends and falling begins.
	fn grounded(&self, block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>) -> bool {
		if self.out.y > 0.0 {
			return false;
		}
		let y = (self.pos.y - EYE_TO_FEET - GROUND_EPS).floor() as i32;
		let r2 = RADIUS * RADIUS;
		for x in span(self.pos.x - RADIUS, self.pos.x + RADIUS) {
			for z in span(self.pos.z - RADIUS, self.pos.z + RADIUS) {
				if !solid(block_at(x, y, z)) {
					continue;
				}
				let dx = (x as f32 - self.pos.x)
					.max(self.pos.x - (x as f32 + 1.0))
					.max(0.0);
				let dz = (z as f32 - self.pos.z)
					.max(self.pos.z - (z as f32 + 1.0))
					.max(0.0);
				if dx * dx + dz * dz < r2 {
					return true;
				}
			}
		}
		false
	}

	/// Iterative capsule-vs-grid penetration resolution: fix the deepest
	/// contact each pass. Ground contacts (steep upward normal while
	/// descending) resolve vertically onto the block plane — a
	/// flat-bottomed stance, so block edges don't shed the player.
	/// Everything else pushes along the contact normal with the
	/// into-contact velocity removed: walls slide, ceilings stop the jump.
	fn resolve(&mut self, block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>) {
		for _ in 0..RESOLVE_ITERS {
			let mut best: Option<Contact> = None;
			for y in span(self.pos.y - EYE_TO_FEET, self.pos.y + EYE_TO_CROWN) {
				for x in span(self.pos.x - RADIUS, self.pos.x + RADIUS) {
					for z in span(self.pos.z - RADIUS, self.pos.z + RADIUS) {
						if !solid(block_at(x, y, z)) {
							continue;
						}
						if let Some(c) = cell_contact(self.pos, (x, y, z)) {
							if best.as_ref().is_none_or(|b| c.depth > b.depth) {
								best = Some(c);
							}
						}
					}
				}
			}
			let Some(c) = best else { break };
			if c.depth < SKIN {
				break;
			}
			if c.n.y > GROUND_NORMAL_Y && self.out.y <= 0.0 {
				self.pos.y = (c.cell_y + 1) as f32 + EYE_TO_FEET;
				self.out.y = 0.0;
			} else {
				self.pos += c.n * c.depth;
				let vn = self.out.dot(c.n);
				if vn < 0.0 {
					self.out -= c.n * vn;
				}
			}
		}
	}

	/// Fraction of the capsule (feet..crown) below the visible water
	/// surface, 0..1. Single-column probe at the capsule axis; every
	/// water-family cell counts as full height (flow amounts are
	/// transient fluid-sim state that can differ across the wire — the
	/// physics must not flip-flop with them); the topmost water cell
	/// tops out at the rendered surface, WATER_SURFACE_DROP below the
	/// lattice plane.
	fn submersion(&self, block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>) -> f32 {
		let (x, z) = (self.pos.x.floor() as i32, self.pos.z.floor() as i32);
		let feet = self.pos.y - EYE_TO_FEET;
		let crown = self.pos.y + EYE_TO_CROWN;
		let mut wet = 0.0f32;
		for cy in (feet.floor() as i32)..=(crown.floor() as i32) {
			if !block_at(x, cy, z).is_some_and(|b| b.is_water()) {
				continue;
			}
			let capped = block_at(x, cy + 1, z).is_some_and(|b| b.is_water());
			let top = cy as f32
				+ if capped {
					1.0
				} else {
					1.0 - WATER_SURFACE_DROP
				};
			wet += (crown.min(top) - feet.max(cy as f32)).max(0.0);
		}
		(wet / HEIGHT).min(1.0)
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

		let mut mv = intent.move_axis;
		if mv.length() > 1.0 {
			mv = mv.normalize();
		}
		let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
		let right = dir.cross(Vec3::Y).normalize_or_zero(); // Java dir × up

		let sub = self.submersion(block_at);
		if sub < SWIM_THRESHOLD {
			// ---- dry / wading: the classic rules, bit-identical ----
			self.out.y = (self.out.y - GRAVITY * dt).max(TERM_VELOCITY);
			if self.grounded(block_at) {
				// Ground movement is plain kinematics: accelerate toward
				// the input target (direction × WALK_SPEED) at
				// WALK_ACCEL, with velocity as the only state —
				// landings, edge exits, turns and re-presses all fall
				// out of momentum naturally (the Java snapped straight
				// to full speed). Releasing input still stops instantly.
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
		} else {
			// ---- swimming: you swim where you point ----
			// Entry damping: exponential drag only below the fastest
			// INTENTIONAL descent (a straight-down dive at SWIM_SPEED),
			// so a terminal-velocity plunge dies within ~2 blocks while
			// deliberate dives never fight the drag. The max(0.0) keeps
			// the dt.min(0.16) guard from flipping signs.
			if self.out.y < -SWIM_SPEED {
				self.out.y *= (1.0 - WATER_DRAG * dt).max(0.0);
			}
			// One velocity target, one ramp (SWIM_ACCEL) — Mario water
			// rules: forward input swims along the LOOK direction (look
			// down to dive, up to climb), strafes stay level, releasing
			// everything eases into the slow sink, and Space always
			// targets straight up.
			let mut target = (dir * mv.y + right * mv.x) * SWIM_SPEED;
			if intent.jump {
				target.y = SWIM_UP_SPEED;
			} else if mv == Vec2::ZERO {
				target.y = WATER_SINK_SPEED;
			}
			self.out += (target - self.out).clamp_length_max(SWIM_ACCEL * dt);
			// Breach: jump held near the surface while moving fires a
			// full jump — apex ≈ 1.27 blocks mounts an adjacent 1-high
			// block through the ordinary landing resolution. Gated on
			// movement so a stationary jump-hold treads instead of
			// pogoing; horizontal carry is kept (no ground-jump damp) so
			// the leap actually reaches the ledge.
			if intent.jump && sub <= BREACH_SUBMERSION && mv != Vec2::ZERO {
				self.out.y = JUMP_IMPULSE;
			}
		}

		debug_assert!((self.out * dt).length() < 1.0, "tunneling guard");
		self.pos += self.out * dt;
		self.resolve(block_at);
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
	fn ceiling_stops_jump() {
		// Flat ceiling at y=12 over the y<10 floor: a 2-block room. The
		// crown must stop at the ceiling plane (the point collider sailed
		// straight through).
		let world = |_x: i32, y: i32, _z: i32| -> Option<BlockType> {
			Some(if y < 10 || y == 12 {
				BlockType::Dirt
			} else {
				BlockType::Air
			})
		};
		let mut p = Player::new(Vec3::new(0.5, 11.5, 0.5));
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
		assert!(peak > 11.5, "still jumped: {peak}");
		assert!((p.pos.y - 11.5).abs() < 0.001, "returned: {}", p.pos.y);
	}

	#[test]
	fn one_block_hole_blocked() {
		// Wall at z >= 12 with only cell y=10 open: a 1-block hole the
		// 1.8-tall capsule cannot fit through (the point collider walked
		// straight in).
		let world = |_x: i32, y: i32, z: i32| -> Option<BlockType> {
			Some(if y < 10 || (z >= 12 && (11..14).contains(&y)) {
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
		// Wall at z >= 12 with cells y=10..12 open: a 2-block corridor
		// the 1.8-tall capsule walks through.
		let world = |_x: i32, y: i32, z: i32| -> Option<BlockType> {
			Some(if y < 10 || (z >= 12 && y >= 12) {
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
		// One-block step at z >= 12, open sky: hold forward + jump and
		// end up standing on it.
		let world = |_x: i32, y: i32, z: i32| -> Option<BlockType> {
			Some(if y < 10 || (z >= 12 && y == 10) {
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
			(p.pos.y - 12.5).abs() < 0.001,
			"standing on it, y = {}",
			p.pos.y
		);
	}

	#[test]
	fn jump_clears_step_under_three_high_ceiling() {
		// Same step, but with a flat ceiling at y=13 (a 3-block room over
		// the floor). Head clearance while mounting is 1.2 blocks — the
		// marginal case the capsule must still clear.
		let world = |_x: i32, y: i32, z: i32| -> Option<BlockType> {
			Some(if y < 10 || (z >= 12 && y == 10) || y == 13 {
				BlockType::Dirt
			} else {
				BlockType::Air
			})
		};
		let mut p = Player::new(Vec3::new(0.5, 11.5, 0.5));
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
			(p.pos.y - 12.5).abs() < 0.001,
			"standing on it, y = {}",
			p.pos.y
		);
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

	// ---- swimming --------------------------------------------------------

	/// Solid below y=10, water sources 10..20 (visible surface at
	/// 19.889), air above.
	fn water_world(x: i32, y: i32, z: i32) -> Option<BlockType> {
		let _ = (x, z);
		Some(if y < 10 {
			BlockType::Dirt
		} else if y < 20 {
			BlockType::Water
		} else {
			BlockType::Air
		})
	}

	/// `water_world` plus a solid shelf at z >= 12 whose top surface
	/// (y = 20) sits level with the water cell tops.
	fn shelf_world(x: i32, y: i32, z: i32) -> Option<BlockType> {
		if z >= 12 && y < 20 {
			return Some(BlockType::Dirt);
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
		// Ramp to the sink target then coast: ~1.36 blocks in the first
		// second — a gentle sink, nothing like dry gravity (~4.9).
		assert!((1.1..=1.5).contains(&dropped), "dropped {dropped}");
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
		// ~2 s of swim-up from depth: most of 6 blocks, minus the ramp
		// and the surface bob band.
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
		// A near-vertical dive approaches -SWIM_SPEED — clearly past the
		// passive sink's ~1.36 blocks over the same second.
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

	#[test]
	fn wading_stays_dry_physics() {
		// One block of water on solid ground: sub ≈ 0.49 < threshold —
		// walking speed must be full WALK_SPEED, not SWIM_SPEED.
		let shallow = |x: i32, y: i32, z: i32| -> Option<BlockType> {
			let _ = (x, z);
			Some(if y < 10 {
				BlockType::Dirt
			} else if y < 11 {
				BlockType::Water
			} else {
				BlockType::Air
			})
		};
		let mut p = Player::new(Vec3::new(0.5, 11.5, 0.5)); // feet on ground
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
			([-2.2490497f32, 11.5, 5.5237446], [-4.0f32, 0.0, 0.0]),
			"Player::tick behavior changed — update these values AND bump digblock_core::SIM_VERSION"
		);
	}

	/// The water counterpart: pins the swim branch (entry damping, sink,
	/// swim-up, breach onto a shelf) to the bit. Same contract as above.
	#[test]
	fn golden_water_trajectory_pins_sim_version() {
		let shelf = |x: i32, y: i32, z: i32| -> Option<BlockType> {
			let _ = x;
			Some(if y < 10 || (z >= 12 && y < 20) {
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
			p.tick(TICK, Vec3::Z, intent, &shelf);
		}
		let s = p.state();
		// Tick 240 catches the breach arc over the shelf lip — falling
		// in, sinking, swim-up, and the breach all feed these bits.
		assert_eq!(
			(s.pos.to_array(), s.vel.to_array()),
			([0.5f32, 22.090445, 11.94389], [0.0f32, 3.856666, 0.5000001]),
			"swim behavior changed — update these values AND bump digblock_core::SIM_VERSION"
		);
	}
}
