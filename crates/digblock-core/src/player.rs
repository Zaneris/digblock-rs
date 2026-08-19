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
mod tests {
	use super::*;

	/// Test spawn: in the air above the flat_world ground.
	const TEST_SPAWN: Vec3 = Vec3::new(0.5, 35.0, 0.5);

	fn new_player() -> Player {
		Player::new(TEST_SPAWN)
	}

	/// Flat ground: solid below the surface line, air above.
	fn flat_world(x: i32, y: i32, z: i32) -> Option<u16> {
		let _ = (x, z);
		some_cell(if y < 10 {
			BlockType::Dirt
		} else {
			BlockType::Air
		})
	}

	/// The anchor each seed resolves to. A drift here means players of an
	/// existing world wake up somewhere else, so these move only for a
	/// deliberate rule change — never as a side effect.
	///
	/// Last moved when the disc test became a real levelness check: the
	/// old rule permitted neighbours at `h + 1` and said nothing about
	/// neighbours below `h`, so spawns could sit boxed in by rock at head
	/// height or on the lip of a drop. Requiring a level disc relocated
	/// most seeds (`8675309` happened to already satisfy it).
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
			assert_eq!(spawn(seed), want, "seed {seed}");
		}
	}

	/// `ascend_clear` steps over blocks built into the spawn capsule,
	/// leaves a clear spawn alone, treats unloaded as clear, and gives up
	/// (returns the input) on a column solid to the ceiling.
	#[test]
	fn ascend_clear_steps_over_built_obstructions() {
		let eye = Vec3::new(0.5, 11.5, 0.5); // standing on flat_world ground
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
			(p.pos.y - 12.5).abs() < 0.001,
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
		// Ramp to the sink target then coast — a gentle sink, nothing
		// like the drop dry gravity gives over the same second.
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

	/// The per-tick rise a walk covers on a 45° ramp.
	const RAMP_DY: f32 = 4.0 * TICK;

	/// Walking a flight is ONE continuous climb, not a series of hops: the
	/// feet move at the rate the ramp under them carries the walk, every
	/// tick, in both directions — never faster, and never backwards.
	/// Ascent failed both halves before SIM v10: merely brushing the next
	/// step's cell popped the feet a full RADIUS early — many times the
	/// honest per-tick rise — and the downhill glue then clawed them back
	/// onto the step below. Held for both flight styles — risers under the
	/// treads, and the floating diagonal.
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
				(Vec3::new(10.5, 11.5, 0.5), 14.0)
			} else {
				(Vec3::new(0.5, 11.5, 8.5), 13.0)
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
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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
			(p.pos.y - 12.5).abs() < 0.001,
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
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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
	fn walks_up_a_stair_flight_without_jumping() {
		let mut p = Player::new(Vec3::new(0.5, 11.5, 8.5));
		let forward = PlayerIntent {
			move_axis: Vec2::new(0.0, 1.0),
			jump: false,
		};
		for _ in 0..360 {
			p.tick(TICK, Vec3::Z, forward, &flight_world);
		}
		assert!(p.pos.z > 16.0, "crossed the flight, z = {}", p.pos.z);
		assert!(
			(p.pos.y - 14.5).abs() < 0.001,
			"standing on the platform, y = {}",
			p.pos.y
		);
	}

	#[test]
	fn descends_a_stair_flight_smoothly() {
		// Start on the platform, walk −z down the flight: never a free
		// fall — each tick's descent stays inside the downhill-glue band
		// while over the stairs, and the run ends back on the ground.
		let mut p = Player::new(Vec3::new(0.5, 14.5, 17.5));
		let back = PlayerIntent {
			move_axis: Vec2::new(0.0, -1.0),
			jump: false,
		};
		let mut prev_y = p.pos.y;
		for _ in 0..420 {
			p.tick(TICK, Vec3::Z, back, &flight_world);
			let dy = p.pos.y - prev_y;
			prev_y = p.pos.y;
			if (11.5..14.5).contains(&p.pos.y) && p.pos.z > 11.5 {
				assert!(dy > -0.15, "smooth descent, dy = {dy} at {:?}", p.pos);
			}
		}
		assert!(p.pos.z < 11.0, "left the flight, z = {}", p.pos.z);
		assert!((p.pos.y - 11.5).abs() < 0.001, "on the ground: {}", p.pos.y);
	}

	#[test]
	fn stair_back_face_blocks_like_a_wall() {
		// Walking −x into the stair's tall back (+x side): a wall.
		let mut p = Player::new(Vec3::new(14.5, 11.5, 0.5));
		let toward = PlayerIntent {
			move_axis: Vec2::new(0.0, 1.0),
			jump: false,
		};
		for _ in 0..180 {
			p.tick(TICK, Vec3::NEG_X, toward, &one_stair_world);
		}
		assert!(p.pos.x > 13.25, "blocked at the back face, x = {}", p.pos.x);
		assert!((p.pos.y - 11.5).abs() < 0.001, "never climbed: {}", p.pos.y);
	}

	#[test]
	fn stair_low_side_ramps_up() {
		// Walking +x from the low side: the ramp lifts the player over
		// the cell with no jump, then they drop off the far side.
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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

	/// A step up is the same motion as a stair: the feet rise at the
	/// ramp rate, no faster, and the mount lands exactly as the axis
	/// crosses onto the block. Before SIM v10 each rise was a one-tick
	/// teleport of the whole `AUTO_STEP_MAX` — the jarring pop this pins
	/// against. Every intermediate state must also pass `overlaps_solid`:
	/// a rate-limited mount spends its ticks with the capsule inside the
	/// side of the box it climbs, and the server's envelope probes
	/// exactly these.
	#[test]
	fn slab_steps_mount_at_the_ramp_rate() {
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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

	/// The mount budget must not turn a LANDING into a slow sink: arriving
	/// downward faster than a mount climbs keeps the flat-bottomed snap.
	/// Dropped onto the very edge of a slab, the feet land on top of it
	/// and never end a tick below its surface.
	/// The mount budget must not turn a LANDING into a slow sink: arriving
	/// downward faster than a mount climbs keeps the flat-bottomed snap.
	/// Dropped onto the very edge of a slab — axis off the box, so the
	/// mount gates would otherwise all pass — the feet must never end a
	/// tick below its surface, at any drop height. The sweep matters: how
	/// far the first contacting tick overshoots the top depends on the
	/// phase of the fall, and only the deep overshoots exceed one tick of
	/// mount budget.
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
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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
	fn steps_up_slab_stack_and_probe_agrees() {
		// One third → two thirds → full cube → cube plus a third: four
		// AUTO_STEP rises in one contiguous walk. Every post-tick
		// position must read as
		// not-solid — the adoption envelope's probe runs on exactly these.
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
		let forward = PlayerIntent {
			move_axis: Vec2::new(0.0, 1.0),
			jump: false,
		};
		for _ in 0..300 {
			let prev = p.pos.y;
			p.tick(TICK, Vec3::X, forward, &slab_stack_world);
			assert!(p.pos.y >= prev - 1e-3, "no dip on the stack: {:?}", p.pos);
			assert!(
				!overlaps_solid(p.pos, &slab_stack_world),
				"probe agrees with the resolver: {:?}",
				p.pos
			);
			if p.pos.x > 15.4 {
				break;
			}
		}
		assert!(p.pos.x > 15.4, "climbed the whole stack, x = {}", p.pos.x);
		let feet = p.pos.y - EYE_TO_FEET;
		assert!(
			(feet - (11.0 + 1.0 / 3.0)).abs() < 0.001,
			"topped out on the raised 1/3, feet = {feet}"
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
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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
		let mut p = Player::new(Vec3::new(10.5, 11.5, 0.5));
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
		let mut blocked = Player::new(Vec3::new(10.5, 11.5, 0.5));
		let closed = door_world(false);
		for _ in 0..240 {
			blocked.tick(TICK, Vec3::X, forward, &closed);
		}
		assert!(
			blocked.pos.x < 12.0,
			"closed leaf is a wall, x = {}",
			blocked.pos.x
		);

		let mut walker = Player::new(Vec3::new(10.5, 11.5, 0.5));
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
		assert!(!overlaps_solid(Vec3::new(0.5, 11.5, 0.5), &flight_world));
		assert!(overlaps_solid(Vec3::new(0.5, 11.3, 0.5), &flight_world));
		// Door: the closed leaf is set back from the cell face it points
		// at, so the cell CENTER's capsule now grazes the leaf's front
		// face; clearing it needs a RADIUS beyond that face. Open = no
		// collision geometry at all (the handle knob is interaction-only
		// and never counts).
		let center = Vec3::new(12.5, 11.5, 0.5);
		let clear = Vec3::new(12.6, 11.5, 0.5);
		let against = Vec3::new(12.25, 11.5, 0.5);
		assert!(overlaps_solid(center, &door_world(false)));
		assert!(!overlaps_solid(clear, &door_world(false)));
		assert!(overlaps_solid(against, &door_world(false)));
		assert!(!overlaps_solid(against, &door_world(true)));
	}

	#[test]
	fn placement_blocked_is_strict() {
		let eye = Vec3::new(0.5, 11.5, 0.5);
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
		let near_leaf = Vec3::new(0.25, 11.5, 0.5);
		assert!(placement_blocked(near_leaf, feet_cell, closed));
		assert!(placement_blocked(eye, feet_cell, closed), "center grazes");
		assert!(
			!placement_blocked(Vec3::new(0.6, 11.5, 0.5), feet_cell, closed),
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
			some_cell(if y < 10 {
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
			([0.5f32, 11.5, 7.969425], [0.0f32, 0.0, -4.0]),
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
			p.tick(TICK, Vec3::Z, intent, &shelf);
		}
		let s = p.state();
		// The last tick catches the breach arc over the shelf lip — falling
		// in, sinking, swim-up, and the breach all feed these bits.
		assert_eq!(
			(s.pos.to_array(), s.vel.to_array()),
			([0.5f32, 22.090445, 11.94389], [0.0f32, 3.856666, 0.5000001]),
			"swim behavior changed — update these values AND bump digblock_core::SIM_VERSION"
		);
	}
}
