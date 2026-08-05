//! Face directions — pure grid geometry, shared by the block table,
//! mesher, and anything that reasons about faces. Deliberately BELOW
//! block.rs in the layering: the base data modules must not depend on
//! the 1400-line mesher.

/// Directions in the Java normal-table order (VertTex.glsl normal()):
/// 0=West(+x) 1=North(+z) 2=South(−z) 3=Bottom(−y) 4=Top(+y) 5=East(−x).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Dir {
	West = 0,
	North = 1,
	South = 2,
	Bottom = 3,
	Top = 4,
	East = 5,
}

pub const DIRS: [Dir; 6] = [
	Dir::West,
	Dir::North,
	Dir::South,
	Dir::Bottom,
	Dir::Top,
	Dir::East,
];

impl Dir {
	#[inline]
	pub fn offset(self) -> (i32, i32, i32) {
		match self {
			Dir::West => (1, 0, 0),
			Dir::North => (0, 0, 1),
			Dir::South => (0, 0, -1),
			Dir::Bottom => (0, -1, 0),
			Dir::Top => (0, 1, 0),
			Dir::East => (-1, 0, 0),
		}
	}

	#[inline]
	pub fn opposite(self) -> Dir {
		match self {
			Dir::West => Dir::East,
			Dir::North => Dir::South,
			Dir::South => Dir::North,
			Dir::Bottom => Dir::Top,
			Dir::Top => Dir::Bottom,
			Dir::East => Dir::West,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn opposite_negates_offset_and_involutes() {
		for d in DIRS {
			let (x, y, z) = d.offset();
			assert_eq!(d.opposite().offset(), (-x, -y, -z));
			assert_eq!(d.opposite().opposite(), d);
		}
	}
}
