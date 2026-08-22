//! A kindled light: conjured light standing in the cell the caster's
//! HEAD occupies, on a clock of its own. Not a block — a block would be
//! a journaled edit that outlives its caster, and the sweep that once
//! faded it tied the lifetime to the world's day rather than the cast.
//! Its own clock is what lets the caster port away and leave it burning
//! exactly as long as any other.

use crate::block::BlockType;
use crate::chunk::cell_id;
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
/// air, water, a walk-through block. Asked of the cell's CURRENT value on
/// both ends, so a caster whose head the server has inside stone (an
/// unadopted claim) is refused rather than lighting the rock.
pub fn hosts(cell: Option<u16>) -> bool {
	cell.is_some_and(|v| !BlockType::from_id(cell_id(v)).props().collides)
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
	use crate::chunk::make_cell;

	/// A light kindles exactly where a head fits: every block a player
	/// can stand inside hosts one, every block that stops a player does
	/// not, and an unloaded cell never does — counted, because a sweep
	/// that found no host of either kind proved nothing.
	#[test]
	fn a_light_kindles_wherever_a_head_fits() {
		let (mut hosts_n, mut refuses_n) = (0, 0);
		for b in BlockType::ALL {
			let cell = Some(make_cell(b, 0));
			assert_eq!(hosts(cell), !b.props().collides, "{b:?}");
			if hosts(cell) {
				hosts_n += 1;
			} else {
				refuses_n += 1;
			}
		}
		assert!(hosts_n > 0 && refuses_n > 0, "the sweep saw one kind only");
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
