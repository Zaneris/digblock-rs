//! Capsule-vs-cell geometry, shared by the client, the server's
//! InsidePlayer scan and the adoption envelope's probe. Standing ON a
//! shaped block is not "inside solid" -- a step and a stair are the
//! same motion, and a landing is never rate-limited.

use super::*;

/// Cells covered by the closed interval [lo, hi], shrunk by CELL_EPS so
/// a bound resting exactly on a block plane stays out of the far cell.
pub(super) fn span(lo: f32, hi: f32) -> std::ops::RangeInclusive<i32> {
	((lo + CELL_EPS).floor() as i32)..=((hi - CELL_EPS).floor() as i32)
}

/// Closest points between the (vertical) capsule axis at eye `pos` and
/// the AABB [lo, hi]: (point on axis, point on box). Closed form — the
/// box clamps the axis xz horizontally; vertically, overlapping
/// intervals meet at the overlap (the normal comes out horizontal),
/// otherwise the nearer segment end pairs with the nearer face. Unit
/// cells pass `lo = c as f32, hi = lo + 1.0` — bit-identical to the old
/// cell-only form.
pub(super) fn closest_axis_box(pos: Vec3, lo: Vec3, hi: Vec3) -> (Vec3, Vec3) {
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
pub(super) fn cell_box(c: (i32, i32, i32)) -> (Vec3, Vec3) {
	let lo = Vec3::new(c.0 as f32, c.1 as f32, c.2 as f32);
	(lo, Vec3::new(lo.x + 1.0, lo.y + 1.0, lo.z + 1.0))
}

pub(super) struct Contact {
	/// Unit push direction, box toward capsule.
	pub(super) n: Vec3,
	pub(super) depth: f32,
	/// Supporting surface height for the ground branch (a box's top
	/// plane, or the exact ramp height under the axis for stairs).
	pub(super) top: f32,
	/// Written by [`step_contact`]: this is a mount in progress, so the
	/// lift it applies draws down the tick's mount budget.
	pub(super) step: bool,
}

/// Penetration contact of the capsule at eye `pos` with the AABB
/// [lo, hi], if any.
pub(super) fn box_contact(pos: Vec3, lo: Vec3, hi: Vec3) -> Option<Contact> {
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
pub(super) enum CellShape {
	Empty,
	Cube,
	Boxes(&'static [crate::shapes::SubBox]),
	Ramp {
		facing: crate::shapes::Facing,
		boxes: &'static [crate::shapes::SubBox],
	},
}

pub(super) fn cell_shape(cell: Option<u16>) -> CellShape {
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
pub(super) fn ramp_top(pos: Vec3, c: (i32, i32, i32), f: crate::shapes::Facing) -> Option<f32> {
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
pub(super) fn ramp_support(
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
pub(super) fn runout_exempt(
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
pub(super) fn step_mount(
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
pub(super) fn xz_outside(pos: Vec3, lo: Vec3, hi: Vec3) -> bool {
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
pub(super) fn step_contact(
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
pub(super) fn xz_rect_dist2(pos: Vec3, x: i32, z: i32) -> f32 {
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
pub(super) fn block_of(cell: u16) -> BlockType {
	BlockType::from_id(crate::chunk::cell_id(cell))
}

/// Cell-granular solidity (unloaded None = non-solid, the Java
/// null-block). Production paths moved to the shape-aware `cell_shape`/
/// `overlaps_solid`; tests keep this for cube-world assertions.
#[cfg(test)]
pub(super) fn solid(cell: Option<u16>) -> bool {
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
