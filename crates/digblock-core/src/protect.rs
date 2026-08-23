//! Cells no one may modify.
//!
//! Two sources, one predicate. The world floor is permanent and applies
//! to every world; a [`Region`] is a protected arrival area — the spawn
//! lock, and a waystone pad. Everything that can write a cell — the
//! server's edit validation, its fluid sim, and the client's local
//! prediction — asks [`immutable`] first, so a protected cell cannot be
//! dug, placed over, interacted with, or flowed into by water sourced
//! outside it.

/// The bottom-most layer of the world. Digging it would leave a hole through the floor of a finite
/// world with nothing underneath. The same plane as [`crate::block::BEDROCK_Y`] BY CONSTRUCTION:
/// the floor is asked about in two vocabularies — how long a cell takes to dig, and whether it may
/// be written ([`immutable`]) — and both have to draw the line at the same y, or a dig refuses
/// where a place lands. Aliasing rather than repeating the literal is why there is one number.
pub const WORLD_FLOOR: i32 = crate::block::BEDROCK_Y;

/// An inclusive box of unmodifiable cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxBounds {
	pub x0: i32,
	pub x1: i32,
	pub y0: i32,
	pub y1: i32,
	pub z0: i32,
	pub z1: i32,
}

/// A ball of unmodifiable cells. Membership is one integer compare on
/// cell coordinates, so the two ends of the wire can never dispute a
/// boundary cell the way a rounded float distance could.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sphere {
	pub cx: i32,
	pub cy: i32,
	pub cz: i32,
	pub r: i32,
}

/// One protected region. A box is the historical shape; arrival regions
/// (the spawn lock, waystone pads) are spheres, whose lower half is what
/// stops a pad being hollowed out from underneath.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
	Box(BoxBounds),
	Sphere(Sphere),
}

impl BoxBounds {
	pub fn contains(&self, x: i32, y: i32, z: i32) -> bool {
		(self.x0..=self.x1).contains(&x)
			&& (self.y0..=self.y1).contains(&y)
			&& (self.z0..=self.z1).contains(&z)
	}
}

impl Sphere {
	pub fn contains(&self, x: i32, y: i32, z: i32) -> bool {
		let (dx, dy, dz) = (x - self.cx, y - self.cy, z - self.cz);
		dx * dx + dy * dy + dz * dz <= self.r * self.r
	}
}

impl Region {
	pub fn contains(&self, x: i32, y: i32, z: i32) -> bool {
		match self {
			Region::Box(b) => b.contains(x, y, z),
			Region::Sphere(s) => s.contains(x, y, z),
		}
	}

	/// The axis-aligned bounds, inclusive — exact for a box, the
	/// enclosing cube for a sphere. This is what a flat wire field
	/// reports; membership questions go through [`Self::contains`].
	pub fn bounds(&self) -> [i32; 6] {
		match self {
			Region::Box(b) => [b.x0, b.x1, b.y0, b.y1, b.z0, b.z1],
			Region::Sphere(s) => [
				s.cx - s.r,
				s.cx + s.r,
				s.cy - s.r,
				s.cy + s.r,
				s.cz - s.r,
				s.cz + s.r,
			],
		}
	}

	/// Every cell in the region, for scans that need to inspect or clear
	/// it up front. A sphere walks its enclosing box and keeps what
	/// [`Self::contains`] keeps, so the two can never disagree.
	pub fn cells(&self) -> impl Iterator<Item = (i32, i32, i32)> + '_ {
		let [x0, x1, y0, y1, z0, z1] = self.bounds();
		(y0..=y1)
			.flat_map(move |y| (z0..=z1).flat_map(move |z| (x0..=x1).map(move |x| (x, y, z))))
			.filter(move |&(x, y, z)| self.contains(x, y, z))
	}
}

/// Whether this cell may never be modified. `regions` is the set of
/// protected arrival areas — empty for a multiplayer client, which is
/// deliberately never told them and learns refusals from the server.
#[inline]
pub fn immutable(x: i32, y: i32, z: i32, regions: &[Region]) -> bool {
	y <= WORLD_FLOOR || regions.iter().any(|r| r.contains(x, y, z))
}

// The refusals a player READS live in the client's `voice`, with every
// other sentence in the game: this module owns the rule, not the wording.

#[cfg(test)]
mod tests {
	use super::*;

	const R: Region = Region::Box(BoxBounds {
		x0: -2,
		x1: 2,
		y0: 130,
		y1: 133,
		z0: -2,
		z1: 2,
	});

	const S: Region = Region::Sphere(Sphere {
		cx: 10,
		cy: 100,
		cz: -10,
		r: 6,
	});

	#[test]
	fn world_floor_is_immutable_everywhere() {
		assert!(immutable(0, WORLD_FLOOR, 0, &[]));
		assert!(immutable(9999, WORLD_FLOOR, -9999, &[]));
		assert!(!immutable(0, WORLD_FLOOR + 1, 0, &[]));
	}

	#[test]
	fn region_bounds_are_inclusive() {
		assert!(immutable(-2, 130, -2, &[R]));
		assert!(immutable(2, 133, 2, &[R]));
		assert!(!immutable(3, 131, 0, &[R]));
		assert!(!immutable(0, 134, 0, &[R]));
		assert!(!immutable(0, 129, 0, &[R]));
	}

	#[test]
	fn any_region_in_the_set_protects() {
		assert!(immutable(0, 131, 0, &[R, S]));
		assert!(immutable(10, 100, -10, &[R, S]));
		assert!(!immutable(50, 50, 50, &[R, S]));
	}

	/// Sphere membership at the boundary, derived from the radius: the
	/// axis cell at exactly `r` is in, one past is out, and the corner of
	/// the enclosing cube (farther than `r` in every case where `r > 0`)
	/// is out.
	#[test]
	fn sphere_boundary_cells_resolve_by_centre_distance() {
		let Region::Sphere(s) = S else { unreachable!() };
		assert!(S.contains(s.cx + s.r, s.cy, s.cz));
		assert!(!S.contains(s.cx + s.r + 1, s.cy, s.cz));
		assert!(!S.contains(s.cx + s.r, s.cy + s.r, s.cz + s.r));
	}

	#[test]
	fn cells_covers_the_whole_box_once() {
		let v: Vec<_> = R.cells().collect();
		assert_eq!(v.len(), 5 * 5 * 4);
		assert!(v.iter().all(|&(x, y, z)| R.contains(x, y, z)));
		let mut sorted = v.clone();
		sorted.sort_unstable();
		sorted.dedup();
		assert_eq!(sorted.len(), v.len(), "cells yielded a duplicate");
	}

	/// The sphere's cell walk visits exactly the member set: everything
	/// it yields answers `contains`, nothing twice, and every containing
	/// cell of the enclosing cube is yielded — counted against an
	/// independent sweep so a filter bug cannot pass vacuously.
	#[test]
	fn cells_visits_each_sphere_cell_once() {
		let v: Vec<_> = S.cells().collect();
		assert!(!v.is_empty());
		assert!(v.iter().all(|&(x, y, z)| S.contains(x, y, z)));
		let mut sorted = v.clone();
		sorted.sort_unstable();
		sorted.dedup();
		assert_eq!(sorted.len(), v.len(), "cells yielded a duplicate");
		let [x0, x1, y0, y1, z0, z1] = S.bounds();
		let mut expect = 0usize;
		for y in y0..=y1 {
			for z in z0..=z1 {
				for x in x0..=x1 {
					if S.contains(x, y, z) {
						expect += 1;
					}
				}
			}
		}
		assert_eq!(v.len(), expect);
	}
}
