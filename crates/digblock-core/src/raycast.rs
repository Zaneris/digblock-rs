//! Voxel picking: Amanatides–Woo DDA over the block grid. Pure — takes the
//! same `cell_at` closure as player physics, so it is headless-testable
//! and reusable for any future entity targeting.

use crate::block::BlockType;
use crate::chunk::cell_id;
use glam::Vec3;

/// The player's reach for digging and placing, in blocks.
pub const REACH: f32 = 5.0;

#[derive(Clone, Copy, Debug)]
pub struct RayHit {
	/// The stopping cell (colliding, or matching the caller's `stops`
	/// predicate in `raycast_with`).
	pub cell: (i32, i32, i32),
	/// Unit normal of the face entered through; the placement cell is
	/// `cell + normal`. All zero when the origin was inside a stopping
	/// block (dig is fine, placement must be rejected).
	pub normal: (i32, i32, i32),
	pub block: BlockType,
	/// The stopping cell's full value (id + state nibble).
	pub cell_value: u16,
}

/// Walk the grid from `origin` along `dir` up to `max_dist`, returning the first colliding block.
/// Unloaded (None) cells pass through, matching physics; water and air are transparent to the ray.
///
/// Shaped cells narrow-phase against their INTERACTION boxes, never their collision ones — an open
/// door has no collision yet must stay pickable and diggable, here and on the re-raycasting server.
pub fn raycast(
	origin: Vec3,
	dir: Vec3,
	max_dist: f32,
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
) -> Option<RayHit> {
	raycast_with(origin, dir, max_dist, cell_at, &|c| {
		BlockType::from_id(cell_id(c)).props().solid
	})
}

/// Water-tool picking: stops at colliding blocks AND fluid sources, so a
/// source is targetable while flow/fall sheets stay transparent. Used
/// identically by the client pick and the server's re-validation.
pub fn raycast_fluid(
	origin: Vec3,
	dir: Vec3,
	max_dist: f32,
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
) -> Option<RayHit> {
	raycast_with(origin, dir, max_dist, cell_at, &|c| {
		let b = BlockType::from_id(cell_id(c));
		b.props().solid || b.is_fluid_source()
	})
}

/// The general walk: `stops` decides which loaded cells end the ray
/// (shaped cells then narrow-phase against their interaction boxes).
pub fn raycast_with(
	origin: Vec3,
	dir: Vec3,
	max_dist: f32,
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
	stops: &dyn Fn(u16) -> bool,
) -> Option<RayHit> {
	walk(
		origin,
		dir,
		max_dist,
		cell_at,
		stops,
		&crate::shapes::interaction_boxes,
	)
}

/// Deterministic ray-vs-AABB slab test: `Some((t, normal))` for the
/// nearest crossing at t ≥ 0 (normal all-zero when the origin is inside
/// the box). Pure IEEE f32 — both sides of the wire run these exact ops.
fn ray_box(origin: Vec3, dir: Vec3, lo: Vec3, hi: Vec3) -> Option<(f32, (i32, i32, i32))> {
	let (o, d) = (origin.to_array(), dir.to_array());
	let (l, h) = (lo.to_array(), hi.to_array());
	let mut t_near = f32::NEG_INFINITY;
	let mut t_far = f32::INFINITY;
	let mut axis = 3usize; // past the last axis: no bounding slab (inside all)
	for a in 0..3 {
		if d[a] == 0.0 {
			if o[a] < l[a] || o[a] > h[a] {
				return None;
			}
			continue;
		}
		let inv = 1.0 / d[a];
		let (t0, t1) = ((l[a] - o[a]) * inv, (h[a] - o[a]) * inv);
		let (t0, t1) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
		if t0 > t_near {
			t_near = t0;
			axis = a;
		}
		t_far = t_far.min(t1);
		if t_near > t_far {
			return None;
		}
	}
	if t_far < 0.0 {
		return None;
	}
	if t_near < 0.0 || axis == 3 {
		return Some((0.0, (0, 0, 0))); // origin inside the box
	}
	let mut n = [0i32; 3];
	n[axis] = if d[axis] > 0.0 { -1 } else { 1 };
	Some((t_near, (n[0], n[1], n[2])))
}

/// Narrow phase for a stopping cell: full cubes hit at the DDA's entry
/// face; shaped cells test the ray against `boxes` and MISS (None) when
/// it threads the open region.
fn cell_hit(
	origin: Vec3,
	dir: Vec3,
	max_dist: f32,
	cell: (i32, i32, i32),
	v: u16,
	entry_normal: (i32, i32, i32),
	boxes_of: &dyn Fn(u16) -> &'static [crate::shapes::SubBox],
) -> Option<RayHit> {
	let hit = |normal: (i32, i32, i32)| RayHit {
		cell,
		normal,
		block: BlockType::from_id(cell_id(v)),
		cell_value: v,
	};
	if crate::shapes::is_full_cube(v) {
		return Some(hit(entry_normal));
	}
	let mut best: Option<(f32, (i32, i32, i32))> = None;
	for b in boxes_of(v) {
		let (lo, hi) = b.aabb(cell);
		if let Some((t, n)) = ray_box(origin, dir, lo, hi)
			&& t <= max_dist
			&& best.is_none_or(|(bt, _)| t < bt)
		{
			best = Some((t, n));
		}
	}
	best.map(|(_, n)| hit(n))
}

fn walk(
	origin: Vec3,
	dir: Vec3,
	max_dist: f32,
	cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
	stops: &dyn Fn(u16) -> bool,
	boxes_of: &dyn Fn(u16) -> &'static [crate::shapes::SubBox],
) -> Option<RayHit> {
	let hits = |c: Option<u16>| c.filter(|&cell| stops(cell));
	let dir = dir.normalize_or_zero();
	if dir == Vec3::ZERO {
		return None;
	}
	let mut cell = (
		origin.x.floor() as i32,
		origin.y.floor() as i32,
		origin.z.floor() as i32,
	);
	if let Some(v) = hits(cell_at(cell.0, cell.1, cell.2)) {
		// Origin-cell stop: a full cube reports the zero normal (dig ok,
		// placement rejected); a shaped cell may still miss and let the
		// walk continue.
		if let Some(h) = cell_hit(origin, dir, max_dist, cell, v, (0, 0, 0), boxes_of) {
			return Some(h);
		}
	}
	// Per axis: cell step, t to the first boundary crossing, t per cell.
	let axis = |o: f32, d: f32| -> (i32, f32, f32) {
		if d > 0.0 {
			(1, (o.floor() + 1.0 - o) / d, 1.0 / d)
		} else if d < 0.0 {
			(-1, (o - o.floor()) / -d, -1.0 / d)
		} else {
			(0, f32::INFINITY, f32::INFINITY)
		}
	};
	let (step_x, mut tx, dtx) = axis(origin.x, dir.x);
	let (step_y, mut ty, dty) = axis(origin.y, dir.y);
	let (step_z, mut tz, dtz) = axis(origin.z, dir.z);
	loop {
		let (t, normal) = if tx <= ty && tx <= tz {
			cell.0 += step_x;
			tx += dtx;
			(tx - dtx, (-step_x, 0, 0))
		} else if ty <= tz {
			cell.1 += step_y;
			ty += dty;
			(ty - dty, (0, -step_y, 0))
		} else {
			cell.2 += step_z;
			tz += dtz;
			(tz - dtz, (0, 0, -step_z))
		};
		if t > max_dist {
			return None;
		}
		if let Some(v) = hits(cell_at(cell.0, cell.1, cell.2))
			&& let Some(h) = cell_hit(origin, dir, max_dist, cell, v, normal, boxes_of)
		{
			return Some(h);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType as B;

	fn some_cell(b: B) -> Option<u16> {
		Some(b as u16)
	}

	/// Flat ground: solid dirt below the surface line, air above.
	fn flat(_x: i32, y: i32, _z: i32) -> Option<u16> {
		some_cell(if y < 10 { B::Dirt } else { B::Air })
	}

	#[test]
	fn straight_down_hits_ground() {
		let hit = raycast(Vec3::new(3.5, 12.5, 3.5), Vec3::NEG_Y, REACH, &flat).unwrap();
		assert_eq!(hit.cell, (3, 9, 3));
		assert_eq!(hit.normal, (0, 1, 0), "entered through the top face");
		assert_eq!(hit.block, B::Dirt);
		assert_eq!(hit.cell_value, B::Dirt as u16);
	}

	#[test]
	fn axis_aligned_hits_wall_face() {
		let wall = |x: i32, _y: i32, _z: i32| some_cell(if x >= 5 { B::Dirt } else { B::Air });
		let hit = raycast(Vec3::new(2.5, 0.5, 0.5), Vec3::X, REACH, &wall).unwrap();
		assert_eq!(hit.cell, (5, 0, 0));
		assert_eq!(hit.normal, (-1, 0, 0));
	}

	#[test]
	fn diagonal_hits_ground() {
		let dir = Vec3::new(1.0, -1.0, 0.7).normalize();
		let hit = raycast(Vec3::new(0.5, 12.0, 0.5), dir, REACH, &flat).unwrap();
		assert_eq!(hit.cell.1, 9, "stopped in the surface layer");
		assert_ne!(hit.normal, (0, 0, 0));
	}

	#[test]
	fn out_of_reach_misses() {
		assert!(raycast(Vec3::new(0.5, 20.0, 0.5), Vec3::NEG_Y, 5.0, &flat).is_none());
	}

	#[test]
	fn water_and_unloaded_pass_through() {
		// Water column over dirt, with an unloaded gap in between.
		let world = |_x: i32, y: i32, _z: i32| match y {
			..=4 => some_cell(B::Dirt),
			5..=7 => None,
			_ => some_cell(B::Water),
		};
		let hit = raycast(Vec3::new(0.5, 12.5, 0.5), Vec3::NEG_Y, 20.0, &world).unwrap();
		assert_eq!(hit.cell, (0, 4, 0), "passed water and unloaded cells");
	}

	#[test]
	fn origin_inside_solid_has_zero_normal() {
		let hit = raycast(Vec3::new(0.5, 5.5, 0.5), Vec3::NEG_Y, REACH, &flat).unwrap();
		assert_eq!(hit.cell, (0, 5, 0));
		assert_eq!(hit.normal, (0, 0, 0));
	}

	#[test]
	fn zero_direction_is_none() {
		assert!(raycast(Vec3::new(0.5, 20.0, 0.5), Vec3::ZERO, REACH, &flat).is_none());
	}

	// ---- shaped narrow phase --------------------------------------------

	use crate::chunk::make_cell;
	use crate::shapes::{Facing, door_state, stair_state};

	/// A +x-ascending stair with a solid wall a few cells behind it, over
	/// air (no floor — rays only).
	fn stair_wall(x: i32, y: i32, z: i32) -> Option<u16> {
		if (x, y, z) == (5, 0, 0) {
			Some(make_cell(B::WoodStairs, stair_state(Facing::PosX)))
		} else if x == 8 {
			some_cell(B::Dirt)
		} else {
			some_cell(B::Air)
		}
	}

	#[test]
	fn stair_narrow_phase_hits_and_misses() {
		// Straight +x at tread height: the ray meets the mid step's riser
		// front — hit with a −x normal.
		let hit = raycast(Vec3::new(3.5, 0.5, 0.5), Vec3::X, REACH, &stair_wall).unwrap();
		assert_eq!(hit.cell, (5, 0, 0));
		assert_eq!(hit.normal, (-1, 0, 0), "riser face");
		assert_eq!(hit.block, B::WoodStairs);
		// Entering over the mid step's tread but under nothing: still
		// inside the cell's air region until the top step's riser.
		let hit2 = raycast(Vec3::new(3.5, 0.95, 0.5), Vec3::X, REACH, &stair_wall).unwrap();
		assert_eq!(hit2.cell, (5, 0, 0));
		assert_eq!(hit2.normal, (-1, 0, 0));
		// Threading the open region ABOVE the whole stair cell: the ray
		// misses the stair and lands on the wall behind.
		let past = raycast(Vec3::new(3.5, 1.5, 0.5), Vec3::X, 10.0, &stair_wall).unwrap();
		assert_eq!(past.cell, (8, 1, 0), "grazed past the stair");
		// From above onto a tread: +y normal on the stair cell.
		let down = raycast(Vec3::new(5.2, 3.0, 0.5), Vec3::NEG_Y, 10.0, &stair_wall).unwrap();
		assert_eq!(down.cell, (5, 0, 0));
		assert_eq!(down.normal, (0, 1, 0), "tread top");
	}

	#[test]
	fn origin_inside_stair_mass_is_zero_normal() {
		let hit = raycast(Vec3::new(5.9, 0.2, 0.5), Vec3::X, REACH, &stair_wall).unwrap();
		assert_eq!(hit.cell, (5, 0, 0));
		assert_eq!(hit.normal, (0, 0, 0), "started inside the base slab");
	}

	/// A door pair (leaf flush to the −x face when closed) with a solid
	/// wall a few cells behind it.
	fn door_world(open: bool) -> impl Fn(i32, i32, i32) -> Option<u16> {
		move |x: i32, y: i32, z: i32| {
			if x == 6 && (y == 0 || y == 1) && z == 0 {
				Some(make_cell(
					B::LumberDoor,
					door_state(Facing::PosX, open, y == 1),
				))
			} else if x == 9 {
				some_cell(B::Dirt)
			} else {
				some_cell(B::Air)
			}
		}
	}

	#[test]
	fn an_open_door_is_still_pickable_on_its_swung_leaf() {
		let origin = Vec3::new(4.5, 0.5, 0.5);
		// Closed: the leaf spans the cell and stops the ray.
		let closed = door_world(false);
		let hit = raycast(origin, Vec3::X, 10.0, &closed).unwrap();
		assert_eq!((hit.cell, hit.normal), ((6, 0, 0), (-1, 0, 0)));
		// Open: the doorway itself is clear — the ray threads it to the
		// wall behind, which is what makes an open door walk-through and
		// see-through.
		let open = door_world(true);
		let through = raycast(origin, Vec3::X, 10.0, &open).unwrap();
		assert_eq!(through.cell, (9, 0, 0), "the doorway is clear");
		// But the leaf itself is still there, and still a target: aim at
		// where it swung to — flat against the jamb — and the pick lands
		// on the door. Physics has no box there at all — this is exactly
		// the case that used to be unpickable.
		let toward_leaf = Vec3::new(4.5, 0.5, 0.15);
		let leaf = raycast(toward_leaf, Vec3::X, 10.0, &open).unwrap();
		assert_eq!(leaf.cell, (6, 0, 0), "open leaf is aimable");
		assert_eq!(leaf.block, B::LumberDoor);
		assert!(
			crate::shapes::collision_boxes(leaf.cell_value).is_empty(),
			"and it got there with no collision box to hit"
		);
	}

	#[test]
	fn fluid_ray_stops_at_sources_only() {
		// Falling sheet over a flow layer over a source over dirt.
		let world = |_x: i32, y: i32, _z: i32| {
			some_cell(match y {
				..=4 => B::Dirt,
				5 => B::Water,
				6 => B::WaterFlow3,
				7..=9 => B::WaterFall,
				_ => B::Air,
			})
		};
		let hit = raycast_fluid(Vec3::new(0.5, 11.5, 0.5), Vec3::NEG_Y, 20.0, &world).unwrap();
		assert_eq!(hit.cell, (0, 5, 0), "fall and flow are transparent");
		assert_eq!(hit.block, B::Water);
		assert_eq!(hit.normal, (0, 1, 0));

		// The plain ray still passes the whole water column.
		let hit = raycast(Vec3::new(0.5, 11.5, 0.5), Vec3::NEG_Y, 20.0, &world).unwrap();
		assert_eq!(hit.cell, (0, 4, 0));
	}
}
