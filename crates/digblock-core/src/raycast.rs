//! Voxel picking: Amanatides–Woo DDA over the block grid. Pure — takes the
//! same `block_at` closure as player physics, so it is headless-testable
//! and reusable for any future entity targeting.

use crate::block::BlockType;
use glam::Vec3;

/// The player's reach for digging and placing, in blocks.
pub const REACH: f32 = 5.0;

pub struct RayHit {
	/// The colliding cell the ray stopped in.
	pub cell: (i32, i32, i32),
	/// Unit normal of the face entered through; the placement cell is
	/// `cell + normal`. All zero when the origin was inside a colliding
	/// block (dig is fine, placement must be rejected).
	pub normal: (i32, i32, i32),
	pub block: BlockType,
}

fn hits(b: Option<BlockType>) -> Option<BlockType> {
	// Unloaded (None) passes through, matching physics; water and air
	// don't collide and are transparent to the ray.
	b.filter(|bt| bt.props().collides)
}

/// Walk the grid from `origin` along `dir` up to `max_dist`, returning the
/// first colliding block.
pub fn raycast(
	origin: Vec3,
	dir: Vec3,
	max_dist: f32,
	block_at: &dyn Fn(i32, i32, i32) -> Option<BlockType>,
) -> Option<RayHit> {
	let dir = dir.normalize_or_zero();
	if dir == Vec3::ZERO {
		return None;
	}
	let mut cell = (
		origin.x.floor() as i32,
		origin.y.floor() as i32,
		origin.z.floor() as i32,
	);
	if let Some(block) = hits(block_at(cell.0, cell.1, cell.2)) {
		return Some(RayHit {
			cell,
			normal: (0, 0, 0),
			block,
		});
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
		if let Some(block) = hits(block_at(cell.0, cell.1, cell.2)) {
			return Some(RayHit {
				cell,
				normal,
				block,
			});
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType as B;

	/// Flat ground: solid dirt below y=10, air above.
	fn flat(_x: i32, y: i32, _z: i32) -> Option<BlockType> {
		Some(if y < 10 { B::Dirt } else { B::Air })
	}

	#[test]
	fn straight_down_hits_ground() {
		let hit = raycast(Vec3::new(3.5, 12.5, 3.5), Vec3::NEG_Y, REACH, &flat).unwrap();
		assert_eq!(hit.cell, (3, 9, 3));
		assert_eq!(hit.normal, (0, 1, 0), "entered through the top face");
		assert_eq!(hit.block, B::Dirt);
	}

	#[test]
	fn axis_aligned_hits_wall_face() {
		let wall = |x: i32, _y: i32, _z: i32| Some(if x >= 5 { B::Dirt } else { B::Air });
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
			..=4 => Some(B::Dirt),
			5..=7 => None,
			_ => Some(B::Water),
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
}
