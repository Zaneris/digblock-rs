//! Cells no one may modify.
//!
//! Two sources, one predicate. The world floor is permanent and applies
//! to every world; a [`Region`] is an optional operator-declared box (the
//! spawn lock). Everything that can write a cell — the server's edit
//! validation, its fluid sim, and the client's local prediction — asks
//! [`immutable`] first, so a protected cell cannot be dug, placed over,
//! interacted with, or flowed into by water sourced outside it.

/// The bottom-most layer of the world. Digging it would leave a hole
/// through the floor of a finite world with nothing underneath.
///
/// The same plane as [`crate::block::BEDROCK_Y`] BY CONSTRUCTION, not by
/// coincidence: the two names exist because the floor is asked about in
/// two vocabularies — "how long does this take to dig" ([`crate::block::
/// BlockType::dig_hardness`], which answers None here) and "may this cell
/// be written" ([`immutable`], which answers no) — and both questions
/// have to draw the line at the same y or a dig refuses where a place
/// lands, or worse. Aliasing rather than repeating the literal is what
/// makes that impossible to get wrong; there is one number.
pub const WORLD_FLOOR: i32 = crate::block::BEDROCK_Y;

/// An inclusive box of unmodifiable cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
	pub x0: i32,
	pub x1: i32,
	pub y0: i32,
	pub y1: i32,
	pub z0: i32,
	pub z1: i32,
}

impl Region {
	pub fn contains(&self, x: i32, y: i32, z: i32) -> bool {
		(self.x0..=self.x1).contains(&x)
			&& (self.y0..=self.y1).contains(&y)
			&& (self.z0..=self.z1).contains(&z)
	}

	/// Every cell in the region, for scans that need to inspect or clear
	/// it up front.
	pub fn cells(&self) -> impl Iterator<Item = (i32, i32, i32)> + '_ {
		(self.y0..=self.y1).flat_map(move |y| {
			(self.z0..=self.z1).flat_map(move |z| (self.x0..=self.x1).map(move |x| (x, y, z)))
		})
	}
}

/// Whether this cell may never be modified. `region` is the active spawn
/// lock, if any.
#[inline]
pub fn immutable(x: i32, y: i32, z: i32, region: Option<&Region>) -> bool {
	y <= WORLD_FLOOR || region.is_some_and(|r| r.contains(x, y, z))
}

/// Player-facing refusals, shared by every rejection path so the wording
/// can't drift between the server's verdict and local prediction. Two of
/// them: "won't break" and "can't build here" are different situations
/// and one sentence for both reads as a bug.
pub const REFUSE_DIG_MSG: &str = "this block is holding firm";
pub const REFUSE_PLACE_MSG: &str = "nothing will stay put here";

#[cfg(test)]
mod tests {
	use super::*;

	const R: Region = Region {
		x0: -2,
		x1: 2,
		y0: 130,
		y1: 133,
		z0: -2,
		z1: 2,
	};

	#[test]
	fn world_floor_is_immutable_everywhere() {
		assert!(immutable(0, WORLD_FLOOR, 0, None));
		assert!(immutable(9999, WORLD_FLOOR, -9999, None));
		assert!(!immutable(0, WORLD_FLOOR + 1, 0, None));
	}

	#[test]
	fn region_bounds_are_inclusive() {
		assert!(immutable(-2, 130, -2, Some(&R)));
		assert!(immutable(2, 133, 2, Some(&R)));
		assert!(!immutable(3, 131, 0, Some(&R)));
		assert!(!immutable(0, 134, 0, Some(&R)));
		assert!(!immutable(0, 129, 0, Some(&R)));
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
}
