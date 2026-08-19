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
/// The rendered surface of a resting source sits this far below the cell
/// top (water.wgsl sinks a corner by its drop code's fraction of a
/// block); physics flips
/// where water LOOKS like it ends. Mirrored by the engine's eye_in_water.
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
/// Max rise a walking player mounts automatically (SIM v9): one slab
/// third. The ε absorbs twelfths-lattice float skew; the next real rise
/// is the two-thirds slab, comfortably outside. Deliberately equal to one
/// stair tread — stairs are excluded structurally (only the Ramp branch
/// lifts on a stair; its box-fallback still walls), so the tread
/// coincidence can't climb a stair's back face. Must stay under the
/// adoption envelope's per-tick displacement clamp or honest step claims
/// reject.
const AUTO_STEP_MAX: f32 = 4.0 / 12.0 + 1e-3;
/// How fast a mount may carry the feet upward, blocks/s. This is exactly
/// the rate a stair's 45° diagonal lifts a walk (rise = run, so the ramp
/// climbs at `WALK_SPEED`), which is the point: a step and a flight of
/// stairs are the same motion, and a step of one slab third now takes
/// exactly the ticks the equivalent ramp would. A ramp needs no clamp — its
/// geometry never asks for more — so this only ever binds on the step,
/// whose surface is a cliff rather than a slope.
///
/// The sim doing this is what makes the camera smooth: `TickLerp` draws
/// between tick states, so a mount spread over ticks needs no
/// presentation special case, exactly like stairs.
const MOUNT_RATE: f32 = WALK_SPEED;

/// Eye spawn position from the seed alone — see [`crate::spawn::anchor`],
/// which owns the walk. Kept here as the name the rest of the engine has
/// always called; use [`crate::spawn::resolve`] when player edits matter.
pub fn spawn(seed: f64) -> Vec3 {
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

/// Cells covered by the closed interval [lo, hi], shrunk by CELL_EPS so
/// a bound resting exactly on a block plane stays out of the far cell.
fn span(lo: f32, hi: f32) -> std::ops::RangeInclusive<i32> {
	((lo + CELL_EPS).floor() as i32)..=((hi - CELL_EPS).floor() as i32)
}

/// Closest points between the (vertical) capsule axis at eye `pos` and
/// the AABB [lo, hi]: (point on axis, point on box). Closed form — the
/// box clamps the axis xz horizontally; vertically, overlapping
/// intervals meet at the overlap (the normal comes out horizontal),
/// otherwise the nearer segment end pairs with the nearer face. Unit
/// cells pass `lo = c as f32, hi = lo + 1.0` — bit-identical to the old
/// cell-only form.
fn closest_axis_box(pos: Vec3, lo: Vec3, hi: Vec3) -> (Vec3, Vec3) {
	let qx = pos.x.clamp(lo.x, hi.x);
	let qz = pos.z.clamp(lo.z, hi.z);
	let (a_lo, a_hi) = (pos.y + AXIS_LO, pos.y + AXIS_HI);
	let (b_lo, b_hi) = (lo.y, hi.y);
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

/// The unit box of cell `c`.
#[inline]
fn cell_box(c: (i32, i32, i32)) -> (Vec3, Vec3) {
	let lo = Vec3::new(c.0 as f32, c.1 as f32, c.2 as f32);
	(lo, Vec3::new(lo.x + 1.0, lo.y + 1.0, lo.z + 1.0))
}

struct Contact {
	/// Unit push direction, box toward capsule.
	n: Vec3,
	depth: f32,
	/// Supporting surface height for the ground branch (a box's top
	/// plane, or the exact ramp height under the axis for stairs).
	top: f32,
	/// Written by [`step_contact`]: this is a mount in progress, so the
	/// lift it applies draws down the tick's mount budget.
	step: bool,
}

/// Penetration contact of the capsule at eye `pos` with the AABB
/// [lo, hi], if any.
fn box_contact(pos: Vec3, lo: Vec3, hi: Vec3) -> Option<Contact> {
	let (p, q) = closest_axis_box(pos, lo, hi);
	let d2 = p.distance_squared(q);
	if d2 >= RADIUS * RADIUS {
		return None;
	}
	let d = d2.sqrt();
	if d > 1e-6 {
		return Some(Contact {
			n: (p - q) / d,
			depth: RADIUS - d,
			top: hi.y,
			step: false,
		});
	}
	// Degenerate: the axis runs inside the box (a terminal-velocity
	// landing can bury it). Push out through the nearest face, in a
	// fixed order so ties resolve identically everywhere.
	let faces = [
		(Vec3::Y, hi.y - p.y),
		(Vec3::NEG_Y, p.y - lo.y),
		(Vec3::X, hi.x - p.x),
		(Vec3::NEG_X, p.x - lo.x),
		(Vec3::Z, hi.z - p.z),
		(Vec3::NEG_Z, p.z - lo.z),
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
		top: hi.y,
		step: false,
	})
}

/// What a cell presents to COLLISION: nothing, the unit cube, a list of
/// sub-boxes (door leaves), or a stair ramp with its fallback boxes.
enum CellShape {
	Empty,
	Cube,
	Boxes(&'static [crate::shapes::SubBox]),
	Ramp {
		facing: crate::shapes::Facing,
		boxes: &'static [crate::shapes::SubBox],
	},
}

fn cell_shape(cell: Option<u16>) -> CellShape {
	let Some(c) = cell else {
		return CellShape::Empty;
	};
	let b = block_of(c);
	if !b.props().collides {
		return CellShape::Empty;
	}
	if b.props().shape.is_none() {
		return CellShape::Cube;
	}
	let v = crate::shapes::shape_of_cell(c).expect("shape tag ⇒ registered shape");
	match v.ramp {
		Some(facing) => CellShape::Ramp {
			facing,
			boxes: &v.collision,
		},
		// An open door has no collision boxes at all.
		None if v.collision.is_empty() => CellShape::Empty,
		None => CellShape::Boxes(&v.collision),
	}
}

/// The stair ramp's support height under the capsule AXIS: the diagonal
/// h(u) = u from the low front edge to the high back edge of cell `c` ascending
/// toward `f` — the one C0-continuous surface joining both neighbours, so
/// walking a flight never steps or pops.
///
/// `None` BELOW the low front edge: the diagonal hasn't begun, so the cell
/// supports nothing there, and the axis alone decides. Clamping u up to 0
/// instead reported the cell FLOOR as support the moment the footprint merely
/// brushed the cell — on a contiguous flight that is the next step's floor, so
/// the feet popped up a step early and the downhill glue yanked them back.
///
/// ABOVE the high back edge u stays clamped: that top back edge is a real
/// horizontal surface flush with the next cell's floor, and holding it is what
/// carries a mount onto a flush landing without a step.
fn ramp_top(pos: Vec3, c: (i32, i32, i32), f: crate::shapes::Facing) -> Option<f32> {
	use crate::shapes::Facing;
	let u = match f {
		Facing::PosX => pos.x - c.0 as f32,
		Facing::NegX => (c.0 + 1) as f32 - pos.x,
		Facing::PosZ => pos.z - c.2 as f32,
		Facing::NegZ => (c.2 + 1) as f32 - pos.z,
	};
	(u >= 0.0).then(|| c.1 as f32 + u.min(1.0))
}

/// The facing of a ramp currently supporting the capsule — a stair
/// under the footprint whose diagonal is within the snap band of the
/// feet (max top wins when several qualify). Position-only, like the
/// occupancy probes that share it.
fn ramp_support(
	pos: Vec3,
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
) -> Option<crate::shapes::Facing> {
	let feet = pos.y - EYE_TO_FEET;
	let y0 = (feet - GROUND_EPS).floor() as i32;
	let r2 = RADIUS * RADIUS;
	let mut best: Option<(f32, crate::shapes::Facing)> = None;
	for x in span(pos.x - RADIUS, pos.x + RADIUS) {
		for z in span(pos.z - RADIUS, pos.z + RADIUS) {
			if xz_rect_dist2(pos, x, z) >= r2 {
				continue;
			}
			for y in [y0, y0 + 1] {
				if let CellShape::Ramp { facing, .. } = cell_shape(cell_at(x, y, z))
					&& let Some(top) = ramp_top(pos, (x, y, z), facing)
				{
					let lift = top - feet;
					if (-RAMP_SNAP_DOWN..=RAMP_SNAP_MAX).contains(&lift)
						&& best.is_none_or(|(t, _)| top > t)
					{
						best = Some((top, facing));
					}
				}
			}
		}
	}
	best.map(|(_, f)| f)
}

/// Ramp run-out: while a stair's diagonal supports the capsule (`support`),
/// the cube the ramp tops out against must not block — without this the cube's
/// top corner catches the capsule's bottom sphere and mounting a flush landing
/// degrades into a seconds-long grind. Exempt when the cube's top is within the
/// snap band above the feet, the cube sits ahead along the ascent (side walls
/// still block), and there is capsule headroom above its top. Cubes only;
/// shaped neighbours negotiate through their own branches. Shared by the
/// resolver and `overlaps_solid`, so mid-mount claims stay adoptable.
///
/// The next step of the same flight resting ON the cube is not a ceiling: on a
/// filled-in staircase every riser is a cube carrying the next stair. Only a
/// same-facing ramp seated directly on the cube's top qualifies — a stair
/// facing back down the flight really is a wall in the way.
fn runout_exempt(
	pos: Vec3,
	support: Option<crate::shapes::Facing>,
	cell: (i32, i32, i32),
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
) -> bool {
	use crate::shapes::Facing;
	let Some(f) = support else {
		return false;
	};
	let (x, y, z) = cell;
	let top = (y + 1) as f32;
	let lift = top - (pos.y - EYE_TO_FEET);
	if lift <= SKIN || lift > RAMP_SNAP_MAX {
		return false;
	}
	let ahead = match f {
		Facing::PosX => x as f32 + 0.5 > pos.x,
		Facing::NegX => x as f32 + 0.5 < pos.x,
		Facing::PosZ => z as f32 + 0.5 > pos.z,
		Facing::NegZ => z as f32 + 0.5 < pos.z,
	};
	ahead
		&& !(1..=2).any(|k| {
			let over = cell_shape(cell_at(x, y + k, z));
			// The flight continuing over the riser, not a ceiling.
			let next_tread =
				k == 1 && matches!(over, CellShape::Ramp { facing, .. } if facing == f);
			!matches!(over, CellShape::Empty) && !next_tread && ((y + k) as f32 - top) < HEIGHT
		})
}

/// Is the capsule mid-mount onto the box [lo, hi] in `cell` — a rise the
/// auto-step carries rather than a wall it blocks against? Position-only, so
/// the resolver, `grounded` and `overlaps_solid` all read the same answer.
///
/// Three gates. The rise must be within [`AUTO_STEP_MAX`]. There must be
/// capsule headroom above the landing (the ramp `pinched` probe) — doors stay
/// step-proof through this alone, so if a future single-cell shape carries a
/// cell-top box, reconsider deliberately. And the capsule AXIS must still be
/// outside the box's footprint: a mount is an approach from the side, and once
/// the axis is over the box, feet below its top is being buried in it rather
/// than climbing it — which is also what keeps a claim sunk into the floor from
/// passing `overlaps_solid`.
fn step_mount(
	pos: Vec3,
	cell: (i32, i32, i32),
	(lo, hi): (Vec3, Vec3),
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
) -> bool {
	let lift = hi.y - (pos.y - EYE_TO_FEET);
	if lift <= SKIN || lift > AUTO_STEP_MAX || !xz_outside(pos, lo, hi) {
		return false;
	}
	let (x, y, z) = cell;
	!(1..=2).any(|k| {
		!matches!(cell_shape(cell_at(x, y + k, z)), CellShape::Empty)
			&& (y + k) as f32 - hi.y < HEIGHT
	})
}

/// Does the capsule axis lie outside the box's xz footprint?
fn xz_outside(pos: Vec3, lo: Vec3, hi: Vec3) -> bool {
	pos.x < lo.x || pos.x > hi.x || pos.z < lo.z || pos.z > hi.z
}

/// Auto-step: convert a would-be wall contact into a mount — the same pure
/// vertical ground contact the stair ramp emits, so the walk keeps its full
/// horizontal speed and the feet ride up. Cube/Boxes cells only; the Ramp arm
/// negotiates its own lift.
///
/// The lift is capped at the tick's remaining `budget` ([`MOUNT_RATE`]), which
/// turns a step from a one-tick teleport into the same glide a stair gives. The
/// capsule is briefly inside the box's side, which is why `step_mount` is
/// mirrored in `overlaps_solid`; a spent budget yields a zero-depth contact
/// rather than the wall. It always terminates — the feet only ever rise.
///
/// **The vertical-velocity window is what separates a step from a landing.**
/// Rising is never a mount, and neither is arriving downward faster than a
/// mount climbs — that is a fall onto the box's edge, and it keeps the
/// flat-bottomed snap so a landing never sinks into what it lands on.
///
/// This must claim CORNER contacts that already read as ground: partway up the
/// normal on the box's top edge rounds past `GROUND_NORMAL_Y`, and the generic
/// ground branch would snap the rest of the rise in one tick — the pop the
/// budget exists to spread.
fn step_contact(
	pos: Vec3,
	out_y: f32,
	c: Contact,
	cell: (i32, i32, i32),
	bx: (Vec3, Vec3),
	budget: f32,
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
) -> Contact {
	if !(-MOUNT_RATE..=0.0).contains(&out_y) || !step_mount(pos, cell, bx, cell_at) {
		return c;
	}
	let feet = pos.y - EYE_TO_FEET;
	let top = bx.1.y.min(feet + budget.max(0.0));
	Contact {
		n: Vec3::Y,
		depth: top - feet,
		top,
		step: true,
	}
}

/// Squared xz distance from the axis at `pos` to cell (x, z)'s unit
/// rect (0 inside).
#[inline]
fn xz_rect_dist2(pos: Vec3, x: i32, z: i32) -> f32 {
	let dx = (x as f32 - pos.x).max(pos.x - (x as f32 + 1.0)).max(0.0);
	let dz = (z as f32 - pos.z).max(pos.z - (z as f32 + 1.0)).max(0.0);
	dx * dx + dz * dz
}

/// Every cell the player capsule at eye `pos` overlaps (an exact
/// capsule-vs-box test over the bounding cells).
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
				let (lo, hi) = cell_box((x, y, z));
				let (p, q) = closest_axis_box(pos, lo, hi);
				(p.distance_squared(q) < r2).then_some((x, y, z))
			})
		})
	})
}

/// Block type half of a cell (state nibble ignored).
#[inline]
fn block_of(cell: u16) -> BlockType {
	BlockType::from_id(crate::chunk::cell_id(cell))
}

/// Cell-granular solidity (unloaded None = non-solid, the Java
/// null-block). Production paths moved to the shape-aware `cell_shape`/
/// `overlaps_solid`; tests keep this for cube-world assertions.
#[cfg(test)]
fn solid(cell: Option<u16>) -> bool {
	matches!(cell, Some(c) if block_of(c).props().collides)
}

/// Raise `pos` one block at a time until the capsule overlaps no
/// colliding cell — the landing for spawn teleports and joins when
/// players have built at the spawn point (the spawn walk rejects columns
/// whose capsule cells hold terrain or decoration, so only placed blocks
/// can occupy it).
/// Unloaded (None) cells count as clear, matching the physics. A column
/// solid to the world ceiling (not honestly buildable) returns `pos`
/// unchanged.
pub fn ascend_clear(pos: Vec3, cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>) -> Vec3 {
	let mut p = pos;
	while p.y < crate::chunk::WORLD_VBLOCK as f32 {
		if !overlaps_solid(p, cell_at) {
			return p;
		}
		p.y += 1.0;
	}
	pos
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

	/// Standing support probe: any supporting surface within reach of the
	/// feet whose xz footprint is within RADIUS of the axis. Cubes scan
	/// the single layer just below the feet (the classic rule, bit-
	/// identical); shaped cells also scan one layer up — a stair's own
	/// cell contains the feet mid-ramp, and its bottom edge sits exactly
	/// at the layer boundary.
	fn grounded(&self, cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>) -> bool {
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

	/// Iterative capsule-vs-grid penetration resolution: fix the deepest
	/// contact each pass. Ground contacts (steep upward normal while
	/// descending) resolve vertically onto the supporting plane — a
	/// flat-bottomed stance, so block edges don't shed the player. Everything
	/// else pushes along the contact normal with the into-contact velocity
	/// removed: walls slide, ceilings stop the jump.
	///
	/// Stairs are the exception: within RAMP_SNAP_MAX of the diagonal they
	/// emit a pure GROUND contact at the ramp height, never a horizontal push,
	/// so walking a flight is smooth both ways. Deeper than the snap band they
	/// fall back to their sub-boxes and block like walls. `dry` gates the
	/// downhill glue so swimming over submerged stairs doesn't stick.
	///
	/// An auto-step is the same kind of contact and gets the same kind of
	/// motion: the tick's [`MOUNT_RATE`] budget is spent across iterations.
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
								// Pinched: solid cells above leave less
								// than capsule height over the ramp here —
								// the snap would jam the crown into the
								// ceiling, so the stair reverts to its
								// boxes and blocks like a wall (no
								// squeezing through sub-capsule gaps).
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

	/// Fraction of the capsule (feet..crown) below the visible water
	/// surface, 0..1. Single-column probe at the capsule axis; every
	/// water-family cell counts as full height (flow amounts are
	/// transient fluid-sim state that can differ across the wire — the
	/// physics must not flip-flop with them); the topmost water cell
	/// tops out at the rendered surface, WATER_SURFACE_DROP below the
	/// lattice plane.
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
	/// camera direction).
	pub fn tick(
		&mut self,
		dt: f32,
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

/// Shape-aware "is this position inside solid geometry" — the claim probe
/// (server adoption envelope), spawn ascent, and any future teleport check.
/// Mirrors the resolver's classification so a player the resolver is happy
/// with is NEVER "inside solid": standing on a stair tread puts the feet
/// inside the stair's unit CELL, but not inside its geometry.
///
/// Two mount windows are deliberately not solid, both mirrored from the
/// resolver: the ramp run-out, and an auto-step in progress ([`step_mount`]),
/// which spends a few ticks with the capsule inside the side of the box it is
/// climbing. Both are bounded by the same gates the resolver uses, so the
/// allowance a dishonest claim buys is clipping into the top third of
/// something it could legally be standing on a few ticks later.
pub fn overlaps_solid(pos: Vec3, cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>) -> bool {
	let r2 = (RADIUS - CELL_EPS) * (RADIUS - CELL_EPS);
	let feet = pos.y - EYE_TO_FEET;
	let support = ramp_support(pos, cell_at);
	for y in span(pos.y - EYE_TO_FEET, pos.y + EYE_TO_CROWN) {
		for x in span(pos.x - RADIUS, pos.x + RADIUS) {
			for z in span(pos.z - RADIUS, pos.z + RADIUS) {
				let overlap_box = |lo: Vec3, hi: Vec3| -> bool {
					let (p, q) = closest_axis_box(pos, lo, hi);
					p.distance_squared(q) < r2
				};
				match cell_shape(cell_at(x, y, z)) {
					CellShape::Empty => {}
					CellShape::Cube => {
						// Ramp run-out: mid-mount the bottom sphere grazes
						// the landing cube's corner — the resolver exempts
						// it, so the probe must too.
						let (lo, hi) = cell_box((x, y, z));
						if runout_exempt(pos, support, (x, y, z), cell_at)
							|| step_mount(pos, (x, y, z), (lo, hi), cell_at)
						{
							continue;
						}
						if overlap_box(lo, hi) {
							return true;
						}
					}
					CellShape::Boxes(bs) => {
						for b in bs {
							let (lo, hi) = b.aabb((x, y, z));
							if step_mount(pos, (x, y, z), (lo, hi), cell_at) {
								continue;
							}
							if overlap_box(lo, hi) {
								return true;
							}
						}
					}
					CellShape::Ramp { facing, boxes } => {
						// Mirrors the resolver's gate exactly, `None` and
						// all: below the low front edge the cell carries
						// nothing, so feet under its floor are on the step
						// behind — not buried in this one.
						let support = ramp_top(pos, (x, y, z), facing);
						let lift = support.unwrap_or(y as f32) - feet;
						if lift > RAMP_SNAP_MAX {
							for b in boxes {
								let (lo, hi) = b.aabb((x, y, z));
								if overlap_box(lo, hi) {
									return true;
								}
							}
						} else if support.is_some()
							&& lift > SKIN_PROBE && xz_rect_dist2(pos, x, z) < r2
						{
							// Feet below the ramp band (beyond probe skin):
							// buried in the ramp, not resting on it.
							return true;
						}
					}
				}
			}
		}
	}
	false
}

/// Strict capsule-vs-candidate test for PLACEMENT rejection: would the
/// cell value `placed` at `cell` intersect the player at `pos`? No ramp
/// leniency — placing a stair under your own feet is refused exactly
/// like a full block. The client's local gate and the server's
/// InsidePlayer scan share this, so they can never disagree.
pub fn placement_blocked(pos: Vec3, cell: (i32, i32, i32), placed: u16) -> bool {
	let b = block_of(placed);
	if !b.props().collides {
		return false;
	}
	let r2 = (RADIUS - CELL_EPS) * (RADIUS - CELL_EPS);
	let overlap_box = |lo: Vec3, hi: Vec3| -> bool {
		let (p, q) = closest_axis_box(pos, lo, hi);
		p.distance_squared(q) < r2
	};
	if b.props().shape.is_none() {
		let (lo, hi) = cell_box(cell);
		return overlap_box(lo, hi);
	}
	let v = crate::shapes::shape_of_cell(placed).expect("shape tag ⇒ registered shape");
	v.collision.iter().any(|sb| {
		let (lo, hi) = sb.aabb(cell);
		overlap_box(lo, hi)
	})
}

/// Test worlds speak `BlockType`; the seam speaks cells.
#[cfg(test)]
fn some_cell(b: BlockType) -> Option<u16> {
	Some(b as u16)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod golden;
