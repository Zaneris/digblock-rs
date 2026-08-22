//! The Ward: a one-cell-thick glass sphere conjured around the caster's
//! HEAD, the spellbook's first spell that writes cells. The glass it
//! writes is [`BlockType::Wardglass`], which digs to nothing — the
//! sphere is spectacle, never a faucet of panes. Every cell is derived
//! here from the head and the radius on BOTH ends, so the client names
//! nothing the server does not re-derive; the plan is all or nothing,
//! the Arc's rule, since a partial sphere would leave the two ends
//! holding different sets.

use crate::block::BlockType;
use crate::chunk::{cell_id, make_cell};

/// A cell address, the chunk store's tuple.
pub type Cell = (i32, i32, i32);

/// The six face neighbours — fluid spreads across faces, so a shell
/// sealed against THESE is sealed against water.
const FACES: [Cell; 6] = [
	(1, 0, 0),
	(-1, 0, 0),
	(0, 1, 0),
	(0, -1, 0),
	(0, 0, 1),
	(0, 0, -1),
];

fn inside(head: Cell, r: i32, c: Cell) -> bool {
	let (dx, dy, dz) = (c.0 - head.0, c.1 - head.1, c.2 - head.2);
	dx * dx + dy * dy + dz * dz <= r * r
}

fn neighbours(c: Cell) -> impl Iterator<Item = Cell> {
	FACES
		.into_iter()
		.map(move |d| (c.0 + d.0, c.1 + d.1, c.2 + d.2))
}

/// The sphere's two sets: the SHELL — every cell within the radius that
/// shares a face with one beyond it — and the INTERIOR, everything
/// within the radius the shell does not claim. Defining the shell as the
/// interior's face-boundary is what makes it both sealed (no face path
/// leaves the interior without crossing it) and as thin as a sealed
/// shell can be (every shell cell touches the outside, so none is
/// redundant). Both are emitted in scan order, so the two ends agree on
/// sequence as well as membership.
pub fn sets(head: Cell, r: i32) -> (Vec<Cell>, Vec<Cell>) {
	let (mut shell, mut interior) = (Vec::new(), Vec::new());
	for x in head.0 - r..=head.0 + r {
		for y in head.1 - r..=head.1 + r {
			for z in head.2 - r..=head.2 + r {
				let c = (x, y, z);
				if !inside(head, r, c) {
					continue;
				}
				if neighbours(c).any(|n| !inside(head, r, n)) {
					shell.push(c);
				} else {
					interior.push(c);
				}
			}
		}
	}
	(shell, interior)
}

/// Why a plan could not be drawn up. A cell the reader cannot answer for
/// is refused outright rather than skipped: skipping would leave a hole
/// exactly where the world was not loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
	Unloaded,
}

/// The writes one cast performs: shell cells of air or water become
/// wardglass, interior water becomes air, everything else stands. Water
/// in the shell must be claimed too — a wave left standing is a hole —
/// and terrain in the shell is left alone, so a sphere cast on the
/// ground is a dome fused to it. `read` answers with the cell's CURRENT
/// value, `None` for one not loaded.
pub fn plan(
	head: Cell,
	r: i32,
	read: impl Fn(Cell) -> Option<u16>,
) -> Result<Vec<(Cell, u16)>, Refusal> {
	let (shell, interior) = sets(head, r);
	let mut out = Vec::new();
	for c in shell {
		let v = read(c).ok_or(Refusal::Unloaded)?;
		let b = BlockType::from_id(cell_id(v));
		if b == BlockType::Air || b.is_water() {
			out.push((c, make_cell(BlockType::Wardglass, 0)));
		}
	}
	for c in interior {
		let v = read(c).ok_or(Refusal::Unloaded)?;
		if BlockType::from_id(cell_id(v)).is_water() {
			out.push((c, make_cell(BlockType::Air, 0)));
		}
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::unlock;
	use std::collections::HashSet;

	const HEAD: Cell = (3, -7, 11);

	/// Every radius the ladder can cast at — derived from the unlock
	/// table so a retune sweeps the new sizes.
	fn radii() -> Vec<i32> {
		let mut out: Vec<i32> = (0..=100)
			.map(unlock::ward_radius)
			.filter(|&r| r > 0)
			.collect();
		out.dedup();
		assert!(out.len() >= 2, "the ladder has ranks");
		out
	}

	/// The shell is sealed and thin at every castable radius: no interior
	/// cell has a face neighbour outside the sphere, and every shell cell
	/// has one — so nothing leaks and nothing is redundant. Counted, since
	/// an empty set satisfies both.
	#[test]
	fn the_shell_is_sealed_and_one_cell_thin() {
		for r in radii() {
			let (shell, interior) = sets(HEAD, r);
			let shell_set: HashSet<Cell> = shell.iter().copied().collect();
			assert!(shell.len() > 6 && interior.len() > 6, "r={r}");
			for &c in &interior {
				for n in neighbours(c) {
					assert!(
						inside(HEAD, r, n),
						"r={r}: interior {c:?} faces the outside through {n:?}"
					);
				}
				assert!(!shell_set.contains(&c), "r={r}: {c:?} in both sets");
			}
			for &c in &shell {
				assert!(
					neighbours(c).any(|n| !inside(HEAD, r, n)),
					"r={r}: shell {c:?} touches nothing outside"
				);
			}
		}
	}

	/// The caster stands inside their own ward: the head cell and the
	/// body below it are interior at every rank, and a wider rank holds
	/// strictly more.
	#[test]
	fn the_caster_is_inside_and_a_wider_rank_holds_more() {
		let mut last = 0;
		for r in radii() {
			let (_, interior) = sets(HEAD, r);
			assert!(interior.contains(&HEAD), "r={r}");
			assert!(interior.contains(&(HEAD.0, HEAD.1 - 1, HEAD.2)), "r={r}");
			assert!(interior.len() > last, "r={r}");
			last = interior.len();
		}
	}

	/// The plan writes exactly what the rules say: air and water in the
	/// shell turn to wardglass, water inside turns to air, and rock in
	/// either place stands. Each rule is checked to have fired.
	#[test]
	fn the_plan_glasses_the_shell_and_drains_the_inside() {
		let r = radii()[0];
		let (shell, interior) = sets(HEAD, r);
		// Below the head's y is water, the head's own column is stone at
		// the far +x edge, everything else air.
		let world = |c: Cell| -> Option<u16> {
			Some(if c.0 == HEAD.0 + r {
				make_cell(BlockType::Stone, 0)
			} else if c.1 < HEAD.1 {
				make_cell(BlockType::Water, 0)
			} else {
				make_cell(BlockType::Air, 0)
			})
		};
		let writes = plan(HEAD, r, world).unwrap();
		let written: HashSet<Cell> = writes.iter().map(|w| w.0).collect();
		let (mut glassed_air, mut glassed_water, mut drained, mut spared) = (0, 0, 0, 0);
		for &c in &shell {
			let v = world(c).unwrap();
			let b = BlockType::from_id(cell_id(v));
			let w = writes.iter().find(|w| w.0 == c).map(|w| w.1);
			match b {
				BlockType::Air => {
					assert_eq!(w, Some(make_cell(BlockType::Wardglass, 0)), "{c:?}");
					glassed_air += 1;
				}
				BlockType::Water => {
					assert_eq!(w, Some(make_cell(BlockType::Wardglass, 0)), "{c:?}");
					glassed_water += 1;
				}
				_ => {
					assert_eq!(w, None, "{c:?}");
					spared += 1;
				}
			}
		}
		for &c in &interior {
			let b = BlockType::from_id(cell_id(world(c).unwrap()));
			let w = writes.iter().find(|w| w.0 == c).map(|w| w.1);
			if b.is_water() {
				assert_eq!(w, Some(make_cell(BlockType::Air, 0)), "{c:?}");
				drained += 1;
			} else {
				assert_eq!(w, None, "{c:?}");
			}
		}
		assert!(glassed_air > 0 && glassed_water > 0 && drained > 0 && spared > 0);
		assert_eq!(written.len(), writes.len(), "no cell written twice");
	}

	/// One unloaded cell refuses the whole cast — a skipped cell would be
	/// a hole exactly where the world was not there to ask.
	#[test]
	fn an_unloaded_cell_refuses_the_whole_cast() {
		let r = radii()[0];
		let hole = (HEAD.0, HEAD.1 + r, HEAD.2);
		let world = |c: Cell| (c != hole).then_some(make_cell(BlockType::Air, 0));
		assert_eq!(plan(HEAD, r, world), Err(Refusal::Unloaded));
		assert!(plan(HEAD, r, |_| Some(make_cell(BlockType::Air, 0))).is_ok());
	}
}
