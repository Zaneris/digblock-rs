//! A kindled light: conjured light standing in the cell the caster's
//! HEAD occupies, on a clock of its own. Not a block — a block would be
//! a journaled edit that outlives its caster, and the sweep that once
//! faded it tied the lifetime to the world's day rather than the cast.
//! Its own clock is what lets the caster port away and leave it burning
//! exactly as long as any other.

use glam::Vec3;

/// Seconds a kindled light stands.
pub const SECS: u32 = 300;

/// The cell a light kindles into: the one holding the caster's eye.
pub fn host_cell(eye: Vec3) -> (i32, i32, i32) {
	(
		eye.x.floor() as i32,
		eye.y.floor() as i32,
		eye.z.floor() as i32,
	)
}

/// Whether a cell can hold conjured light: anything a head can be in —
/// air, water, a walk-through block, by the physics' own answer. Asked of
/// the cell's CURRENT value on both ends, so a caster whose head the server
/// has inside stone (an unadopted claim) is refused rather than lighting the rock.
pub fn hosts(cell: Option<u16>) -> bool {
	cell.is_some_and(|v| !crate::shapes::collides(v))
}

/// Where the light radiates from: the host cell's centre.
pub fn origin(cell: (i32, i32, i32)) -> Vec3 {
	Vec3::new(
		cell.0 as f32 + 0.5,
		cell.1 as f32 + 0.5,
		cell.2 as f32 + 0.5,
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType;
	use crate::chunk::make_cell;

	/// A light never stands inside geometry. The arbiter is the placement contract every other
	/// writer answers to ([`crate::player::placement_blocked`]), asked of a head in the middle of
	/// the cell — not the field this function happens to read, which is the same answer twice.
	/// ONE direction, deliberately: `hosts` is stricter than the capsule wherever a shape leaves
	/// head-room, and stricter can only refuse a cast. Counted, or an always-no `hosts` passes.
	#[test]
	fn a_light_never_kindles_inside_geometry() {
		let cell = (4, 9, -2);
		let eye = origin(cell);
		let (mut fits, mut blocked_n) = (0, 0);
		for b in BlockType::ALL {
			let v = make_cell(b, 0);
			let blocked = crate::player::placement_blocked(eye, cell, v);
			assert!(
				!(hosts(Some(v)) && blocked),
				"{b:?} would light the inside of itself"
			);
			if blocked {
				blocked_n += 1;
			} else {
				fits += 1;
			}
		}
		assert!(fits > 0 && blocked_n > 0, "the sweep saw one kind only");
		// The two the spell actually rests on: standing in the open, and
		// swimming. Named because the doc promises them.
		assert!(hosts(Some(make_cell(BlockType::Air, 0))));
		assert!(hosts(Some(make_cell(BlockType::Water, 0))));
		assert!(!hosts(None), "nowhere is not somewhere");
	}

	/// The light stands in the cell the eye is in, radiating from its
	/// centre — so it cannot sit inside a wall the eye is not in.
	#[test]
	fn the_light_stands_in_the_eyes_own_cell() {
		let eye = Vec3::new(3.7, -2.2, 0.1);
		let cell = host_cell(eye);
		let o = origin(cell);
		assert_eq!(host_cell(o), cell, "the origin is inside the same cell");
		assert!(
			(o - eye).abs().max_element() < 1.0,
			"within a block of the eye"
		);
	}
}
