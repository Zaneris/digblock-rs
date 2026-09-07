//! Player physics: a vertical capsule collider against the voxel grid. Each
//! tick integrates velocity, then resolves penetration iteratively, deepest
//! contact first. Ground contacts (steep upward normal while descending) land
//! flat on the block plane — the eye rests at exactly top + EYE_TO_FEET
//! anywhere on a block top, no edge slide — while walls slide tangentially and
//! ceilings stop the jump.
//!
//! Water and unloaded chunks are non-solid to COLLISION; submersion instead
//! switches the integration to the buoyant swim branch. Run at a fixed tick
//! (`TICK`): the air-control divisor is a per-tick constant tuned at that rate.

use crate::block::BlockType;
use glam::{Vec2, Vec3};

mod collision;
use crate::shapes::Facing;
use crate::worldgen::Seed;
use collision::*;
pub use collision::{ascend_clear, occupied_cells, overlaps_solid, placement_blocked};

pub const GRAVITY: f32 = 9.8;
pub const TERM_VELOCITY: f32 = -20.0;
/// Max ground speed, blocks/s.
pub const WALK_SPEED: f32 = 4.0;
/// Ground acceleration toward the input target, blocks/s²: reaches
/// WALK_SPEED from a standstill in WALK_SPEED / WALK_ACCEL seconds.
pub const WALK_ACCEL: f32 = 10.0;
/// Ground brake, blocks/s²: how fast speed the input no longer asks for
/// sheds (a release, a reversal) and how fast the velocity swings toward
/// a new direction (a WASD or mouse turn). A full stop and a quarter
/// turn both take about WALK_SPEED / WALK_BRAKE seconds.
pub const WALK_BRAKE: f32 = 40.0;
pub const JUMP_IMPULSE: f32 = 5.0;
/// Fraction of the horizontal run kept at takeoff. The jump adds its
/// own 5 blocks/s straight up, so a full carry launches the eye along
/// a visibly faster path than the walk; the Java halved it, which cost
/// every landing a re-acceleration. Between the two.
pub const JUMP_CARRY: f32 = 0.7;
/// Fixed simulation tick (Java was per-frame at ~60fps).
pub const TICK: f32 = 1.0 / 60.0;

// --- Swimming. Any change here is a sim-behavior change: re-pin BOTH
// golden trajectories and bump SIM_VERSION. ---------------------------
/// The rendered surface of a resting source sits this far below the cell top (water.wgsl sinks a
/// corner by its drop code's fraction of a block); physics flips where water LOOKS like it ends.
/// Mirrored by the engine's eye_in_water.
pub const WATER_SURFACE_DROP: f32 = 0.111;
/// Passive sink speed in water when idle, blocks/s (the velocity target
/// with no input; vs TERM_VELOCITY in air).
pub const WATER_SINK_SPEED: f32 = -1.5;
/// Exponential entry drag, 1/s, applied only below WATER_SINK_SPEED:
/// brings a terminal-velocity plunge down to the idle sink speed within
/// the first stretch under the surface.
pub const WATER_DRAG: f32 = 10.0;
/// Held-Space rise-speed target, blocks/s.
pub const SWIM_UP_SPEED: f32 = 3.0;
/// Swim speed along the pointed direction, blocks/s (vs WALK_SPEED).
pub const SWIM_SPEED: f32 = 2.5;
/// The one swim ramp: acceleration toward the current velocity target
/// (swim direction, Space-up, or the idle sink), blocks/s².
pub const SWIM_ACCEL: f32 = 8.0;
/// Swimming physics engages above this submersion; below it is wading
/// and the dry rules apply unchanged. One block of water on solid ground
/// wets just under half the capsule — waist-deep wading (and ankle-deep
/// flow edges, which count as full cells) stays walkable.
const SWIM_THRESHOLD: f32 = 0.5;
/// Jump held at or below this submersion (while moving) fires a full
/// jump impulse — the way out of the pool. Must be ABOVE the swim
/// threshold or the window is unreachable; the band [threshold, this]
/// is what a surfacing swimmer rises through.
const BREACH_SUBMERSION: f32 = 0.62;
/// The float line: this far above the swim threshold, an upward LOOK stops lifting the swimmer.
/// Aimed up at the surface, the swim target used to carry the capsule through the threshold, the
/// dry branch dropped it straight back, and the two traded the tick at 10-20 Hz: a centimetre
/// judder that a phone's 30 Hz screen turns into a slideshow. Leaving the water is the jump's
/// job (SWIM_UP_SPEED, the breach); the look only steers within it.
const FLOAT_BAND: f32 = 0.05;
/// How fast the water lifts a swimmer whose crown is above the surface toward the float line,
/// blocks/s: gentler than the swim-up, since nobody asked for it.
const FLOAT_RISE_SPEED: f32 = 1.0;

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
/// Stairs deliberately bypass it (the explicit ramp branch) — a 45° ramp
/// normal is exactly 1/√2, a float knife-edge above this threshold.
const GROUND_NORMAL_Y: f32 = 0.7;
const RESOLVE_ITERS: usize = 4;
/// Penetrations shallower than this wait for the next tick.
const SKIN: f32 = 1e-4;

// --- Stairs. Any change here is a sim-behavior change: re-pin the stair
// golden and bump SIM_VERSION. ----------------------------------------
/// Max upward snap onto a stair's diagonal ramp per tick. Larger lifts
/// mean the tall/back side — those resolve against the stair's sub-boxes
/// like ordinary walls.
const RAMP_SNAP_MAX: f32 = 0.5;
/// Downhill glue: feet this far ABOVE the ramp still snap down, so
/// descending a flight never oscillates into free-fall (a walking descent
/// of a 45° ramp drops `WALK_SPEED · TICK` per tick). Ramp-only — cube edges
/// keep the walk-off-cliff momentum behavior bit-exact.
const RAMP_SNAP_DOWN: f32 = 0.1;
/// Probe skin for the shape-aware occupancy tests: covers resolver skin
/// plus claim float noise, so an adopted claim resting exactly on a
/// tread never reads as "inside geometry".
const SKIN_PROBE: f32 = 2e-3;

// --- Auto-step. Any change here is a sim-behavior change: bump
// SIM_VERSION. ---------------------------------------------------------
/// Max rise a walking player mounts automatically (SIM v9): one slab third, plus an ε for
/// twelfths-lattice float skew. Deliberately equal to one stair tread — stairs are excluded
/// structurally — and must stay under the adoption envelope's per-tick clamp or step claims reject.
const AUTO_STEP_MAX: f32 = 4.0 / 12.0 + 1e-3;
/// How fast a mount may carry the feet upward, blocks/s. Exactly the rate a stair's 45° diagonal
/// lifts a walk (rise = run, so the ramp climbs at `WALK_SPEED`), which is the point: a step and a
/// flight of stairs are the same motion. A ramp never asks for more, so this only binds on the
/// step. The sim doing this is what makes the camera smooth — `TickLerp` draws between tick states,
/// so a mount spread over ticks needs no presentation special case.
const MOUNT_RATE: f32 = WALK_SPEED;

/// Eye spawn position from the seed alone — see [`crate::spawn::anchor`],
/// which owns the walk. Kept here as the name the rest of the engine has
/// always called; use [`crate::spawn::resolve`] when player edits matter.
pub fn spawn(seed: Seed) -> Vec3 {
	crate::spawn::anchor(seed)
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

	/// Standing support probe: any supporting surface within reach of the feet whose xz footprint
	/// is within RADIUS of the axis. Cubes scan the single layer just below the feet (the classic
	/// rule, bit-identical); shaped cells also scan one layer up, since a stair's own cell holds
	/// the feet mid-ramp. Public because the Causeway asks it: a plank is owed exactly when the
	/// feet have no support, and both ends must ask the ONE probe rather than each defining it.
	pub fn grounded(&self, cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>) -> bool {
		if self.out.y > 0.0 {
			return false;
		}
		let feet = self.pos.y - EYE_TO_FEET;
		let y0 = (feet - GROUND_EPS).floor() as i32;
		let r2 = RADIUS * RADIUS;
		for x in span(self.pos.x - RADIUS, self.pos.x + RADIUS) {
			for z in span(self.pos.z - RADIUS, self.pos.z + RADIUS) {
				if xz_rect_dist2(self.pos, x, z) >= r2 {
					continue;
				}
				for y in [y0, y0 + 1] {
					match cell_shape(cell_at(x, y, z)) {
						CellShape::Empty => {}
						CellShape::Cube => {
							// Layer y0 only — the classic rule.
							let (lo, hi) = cell_box((x, y, z));
							if y == y0 || step_mount(self.pos, (x, y, z), (lo, hi), cell_at) {
								return true;
							}
						}
						CellShape::Boxes(bs) => {
							for b in bs {
								let (lo, hi) = b.aabb((x, y, z));
								// Mid-mount the foot is already on the step,
								// so ground rules apply — otherwise a mount
								// spends its ticks under air control.
								if step_mount(self.pos, (x, y, z), (lo, hi), cell_at) {
									return true;
								}
								// The box top must sit at the feet, and its
								// own footprint must reach the axis.
								if (hi.y - feet).abs() <= GROUND_EPS {
									let dx = (lo.x - self.pos.x).max(self.pos.x - hi.x).max(0.0);
									let dz = (lo.z - self.pos.z).max(self.pos.z - hi.z).max(0.0);
									if dx * dx + dz * dz < r2 {
										return true;
									}
								}
							}
						}
						CellShape::Ramp { facing, .. } => {
							if let Some(top) = ramp_top(self.pos, (x, y, z), facing)
								&& (top - feet).abs() <= RAMP_SNAP_DOWN
							{
								return true;
							}
						}
					}
				}
			}
		}
		false
	}

	/// Iterative capsule-vs-grid penetration resolution: fix the deepest contact each pass. Ground
	/// contacts resolve vertically onto the supporting plane — a flat-bottomed stance, so block
	/// edges don't shed the player; everything else pushes along the contact normal, walls sliding
	/// and ceilings stopping the jump. Stairs within RAMP_SNAP_MAX emit a GROUND contact at the
	/// ramp height (`dry` gates the downhill glue); an auto-step is the same, spending MOUNT_RATE.
	fn resolve(&mut self, dt: f32, cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>, dry: bool) {
		let feet = |pos: Vec3| pos.y - EYE_TO_FEET;
		let mut step_budget = MOUNT_RATE * dt;
		for _ in 0..RESOLVE_ITERS {
			let support = ramp_support(self.pos, cell_at);
			let mut best: Option<Contact> = None;
			let push = |best: &mut Option<Contact>, c: Contact| {
				if best.as_ref().is_none_or(|b| c.depth > b.depth) {
					*best = Some(c);
				}
			};
			for y in span(self.pos.y - EYE_TO_FEET, self.pos.y + EYE_TO_CROWN) {
				for x in span(self.pos.x - RADIUS, self.pos.x + RADIUS) {
					for z in span(self.pos.z - RADIUS, self.pos.z + RADIUS) {
						match cell_shape(cell_at(x, y, z)) {
							CellShape::Empty => {}
							CellShape::Cube => {
								if runout_exempt(self.pos, support, (x, y, z), cell_at) {
									continue;
								}
								let (lo, hi) = cell_box((x, y, z));
								if let Some(c) = box_contact(self.pos, lo, hi) {
									let c = step_contact(
										self.pos,
										self.out.y,
										c,
										(x, y, z),
										(lo, hi),
										step_budget,
										cell_at,
									);
									push(&mut best, c);
								}
							}
							CellShape::Boxes(bs) => {
								for b in bs {
									let (lo, hi) = b.aabb((x, y, z));
									if let Some(c) = box_contact(self.pos, lo, hi) {
										let c = step_contact(
											self.pos,
											self.out.y,
											c,
											(x, y, z),
											(lo, hi),
											step_budget,
											cell_at,
										);
										push(&mut best, c);
									}
								}
							}
							CellShape::Ramp { facing, boxes } => {
								let support = ramp_top(self.pos, (x, y, z), facing);
								// Below the ramp's low edge nothing is
								// underfoot, but the cell's geometry must
								// still block from below — gate on the
								// cell floor, which is where the diagonal
								// starts.
								let top = support.unwrap_or(y as f32);
								let lift = top - feet(self.pos);
								// Pinched: solid cells above leave less than capsule height over
								// the ramp here — the snap would jam the crown into the ceiling,
								// so the stair reverts to its boxes and blocks like a wall.
								let pinched = (1..=2).any(|k| {
									!matches!(cell_shape(cell_at(x, y + k, z)), CellShape::Empty)
										&& (y + k) as f32 - top < HEIGHT
								});
								if lift > RAMP_SNAP_MAX || (pinched && lift > SKIN) {
									for b in boxes {
										let (lo, hi) = b.aabb((x, y, z));
										if let Some(c) = box_contact(self.pos, lo, hi) {
											push(&mut best, c);
										}
									}
								} else if support.is_some()
									&& self.out.y <= 0.0 && lift > SKIN
									&& xz_rect_dist2(self.pos, x, z) < RADIUS * RADIUS
								{
									push(
										&mut best,
										Contact {
											n: Vec3::Y,
											depth: lift,
											top,
											// The ramp's own slope never
											// asks for more than the mount
											// budget, so it never spends it.
											step: false,
										},
									);
								}
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
				if c.step {
					step_budget -= c.top - feet(self.pos);
				}
				self.pos.y = c.top + EYE_TO_FEET;
				self.out.y = 0.0;
			} else {
				self.pos += c.n * c.depth;
				let vn = self.out.dot(c.n);
				if vn < 0.0 {
					self.out -= c.n * vn;
				}
			}
		}
		// Downhill glue: descending a ramp, the feet drift a few
		// centimeters above it each tick (no penetration → no contact).
		// Snap back down within RAMP_SNAP_DOWN so descent hugs the
		// surface instead of stuttering into micro-falls.
		//
		// It glues to the HIGHEST ramp surface under the footprint, and
		// only when that is genuinely below the feet. Taking the highest
		// merely within the band instead let the step just walked off —
		// whose surface clamps to its own top back edge, the FLOOR of the
		// step now underfoot — drag the feet back down for a tick every
		// time an ascent crossed a step boundary.
		if dry && self.out.y <= 0.0 {
			let f = feet(self.pos);
			let fy = f.floor() as i32;
			let mut best_top: Option<f32> = None;
			for y in [fy, fy - 1] {
				for x in span(self.pos.x - RADIUS, self.pos.x + RADIUS) {
					for z in span(self.pos.z - RADIUS, self.pos.z + RADIUS) {
						let CellShape::Ramp { facing, .. } = cell_shape(cell_at(x, y, z)) else {
							continue;
						};
						if xz_rect_dist2(self.pos, x, z) >= RADIUS * RADIUS {
							continue;
						}
						let Some(top) = ramp_top(self.pos, (x, y, z), facing) else {
							continue;
						};
						if top - f >= -RAMP_SNAP_DOWN && best_top.is_none_or(|b| top > b) {
							best_top = Some(top);
						}
					}
				}
			}
			if let Some(top) = best_top.filter(|t| *t < f) {
				self.pos.y = top + EYE_TO_FEET;
				self.out.y = 0.0;
			}
		}
	}

	/// Fraction of the capsule (feet..crown) below the visible water surface, 0..1. Single-column
	/// probe at the capsule axis; every water-family cell counts as full height — flow amounts are
	/// transient fluid-sim state that can differ across the wire, and physics must not flip-flop
	/// with them. The topmost water cell tops out at the rendered surface, WATER_SURFACE_DROP below
	/// the lattice plane.
	fn submersion(&self, cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>) -> f32 {
		let (x, z) = (self.pos.x.floor() as i32, self.pos.z.floor() as i32);
		let feet = self.pos.y - EYE_TO_FEET;
		let crown = self.pos.y + EYE_TO_CROWN;
		let mut wet = 0.0f32;
		for cy in (feet.floor() as i32)..=(crown.floor() as i32) {
			if !cell_at(x, cy, z).is_some_and(|c| block_of(c).is_water()) {
				continue;
			}
			let capped = cell_at(x, cy + 1, z).is_some_and(|c| block_of(c).is_water());
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
	/// camera direction) and `from` the one the previous tick ran with:
	/// on the ground the run re-aims by the turn between them, so a
	/// mouse flick never waits on the steering swing. In the air the
	/// look leaves velocity alone.
	pub fn tick(
		&mut self,
		dt: f32,
		from: Vec3,
		dir: Vec3,
		intent: PlayerIntent,
		cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
	) {
		let dt = dt.min(0.16);

		let mut mv = intent.move_axis;
		if mv.length() > 1.0 {
			mv = mv.normalize();
		}
		let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
		let right = dir.cross(Vec3::Y).normalize_or_zero(); // Java dir × up

		let sub = self.submersion(cell_at);
		if sub < SWIM_THRESHOLD {
			// ---- dry / wading: the classic rules, bit-identical ----
			self.out.y = (self.out.y - GRAVITY * dt).max(TERM_VELOCITY);
			if self.grounded(cell_at) {
				// Ground movement chases the input target (wish × the
				// speed cap) split by the current HEADING: speed along it
				// grows at WALK_ACCEL up to what the target wants there and
				// sheds at WALK_BRAKE down to it (never past a stop), and
				// the target's sideways part swings the velocity at
				// WALK_BRAKE too. So a release, a reversal and a WASD turn
				// all settle in one short brake, a mouse turn mid-walk
				// carries its speed into the new heading, and a wall slide
				// is untouched because it IS the target along the wall.
				// Stairs mount by position snap (see resolve), so the climb
				// never shows in velocity: the cap shrinks with the rise
				// along the wish so travel ALONG a 45° flight is WALK_SPEED.
				let wish = flat * mv.y + right * mv.x;
				let wish_dir = wish.normalize_or_zero();
				let rise = match ramp_support(self.pos, cell_at) {
					Some(Facing::PosX | Facing::NegX) => wish_dir.x.abs(),
					Some(Facing::PosZ | Facing::NegZ) => wish_dir.z.abs(),
					None => 0.0,
				};
				let target = wish * (WALK_SPEED / (1.0 + rise * rise).sqrt());
				let mut cur = Vec3::new(self.out.x, 0.0, self.out.z);
				// Turn the run with the look: rotate by the yaw from
				// the previous look to this one (a unit complex product
				// in the xz plane), so a flick carries full speed into
				// the new heading and the swing below has nothing to do.
				let from = Vec3::new(from.x, 0.0, from.z).normalize_or_zero();
				if from != Vec3::ZERO && flat != Vec3::ZERO {
					let re = from.x * flat.x + from.z * flat.z;
					let im = from.x * flat.z - from.z * flat.x;
					cur = Vec3::new(cur.x * re - cur.z * im, 0.0, cur.x * im + cur.z * re);
				}
				let heading = cur.normalize_or_zero();
				let want = target.dot(heading);
				let speed = cur.length();
				let speed = if want < speed {
					(speed - WALK_BRAKE * dt).max(want.max(0.0))
				} else {
					(speed + WALK_ACCEL * dt).min(want)
				};
				self.out = if speed <= 0.0 {
					// Stopped (or never moving): set off toward the target.
					target.clamp_length_max(WALK_ACCEL * dt)
				} else {
					// The swing never exceeds the speed itself, so steering
					// rotates a crawl instead of doubling it.
					let swing = (WALK_BRAKE * dt).min(speed);
					let side = target - heading * want;
					heading * speed + side.clamp_length_max(swing)
				};
				// A jump keeps most of its run (JUMP_CARRY) and rides it
				// into the air.
				if intent.jump {
					self.out *= JUMP_CARRY;
					self.out.y = JUMP_IMPULSE;
				}
			} else {
				// Air control steers the flight but may not build speed past a
				// walk. On land a landing brakes whatever the air added; off a
				// breach there is no landing, and a held jump re-breached at
				// every splash with everything the last hop had gained, which
				// skimmed the sea at 19 blocks/s after twenty seconds.
				let before = Vec2::new(self.out.x, self.out.z);
				let drift = Vec2::new(flat.x, flat.z) * (mv.y / 60.0)
					+ Vec2::new(right.x, right.z) * (mv.x / 60.0);
				let after = (before + drift).clamp_length_max(before.length().max(WALK_SPEED));
				self.out.x = after.x;
				self.out.z = after.y;
			}
		} else {
			// ---- swimming: you swim where you point ----
			// Entry damping: exponential drag only below the fastest
			// INTENTIONAL descent (a straight-down dive at SWIM_SPEED),
			// so a terminal-velocity plunge dies just under the surface
			// while deliberate dives never fight the drag. The max(0.0)
			// keeps the dt clamp at the top of the tick from flipping
			// signs.
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
			// Buoyancy, once any part of you is out of the water: idle or
			// swimming level you drift up to the float line and hold there
			// with your head out, and only a downward aim takes you under
			// again. Fully submerged the slow sink stands, so a dive stays a
			// dive. The look cannot swim you OUT of the water either: at the
			// float line an upward aim holds level (FLOAT_BAND), and leaving
			// is the jump's job.
			if !intent.jump && sub < 1.0 {
				let diving = mv != Vec2::ZERO && target.y < 0.0;
				if !diving {
					target.y = target.y.max(FLOAT_RISE_SPEED);
				}
				if sub <= SWIM_THRESHOLD + FLOAT_BAND {
					target.y = target.y.min(0.0);
				}
			}
			self.out += (target - self.out).clamp_length_max(SWIM_ACCEL * dt);
			// Breach: jump held near the surface while moving fires a
			// full jump — its apex clears an adjacent one-block ledge,
			// mounted through the ordinary landing resolution. Gated on
			// movement so a stationary jump-hold treads instead of
			// pogoing; horizontal carry is kept (no ground-jump damp) so
			// the leap actually reaches the ledge.
			if intent.jump && sub <= BREACH_SUBMERSION && mv != Vec2::ZERO {
				self.out.y = JUMP_IMPULSE;
			}
		}

		debug_assert!((self.out * dt).length() < 1.0, "tunneling guard");
		self.pos += self.out * dt;
		self.resolve(dt, cell_at, sub < SWIM_THRESHOLD);
	}
}

#[cfg(test)]
fn some_cell(b: BlockType) -> Option<u16> {
	Some(b as u16)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod golden;
