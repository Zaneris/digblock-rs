//! The Resonant Pick's break pattern (SKILLS-DESIGN §5.6).
//!
//! One hold, several cells: the plane perpendicular to the face you hit,
//! a row at the first Mining milestone and the full plane at the second. The
//! pair is derived
//! from the ray, never chosen by the client — the server re-raycasts and
//! runs this same function over its own world, so there is no cheat
//! surface and no second opinion about which cells an action touched.
//!
//! Two rules make the rest of the machinery work unchanged:
//!
//! - **Every extra cell must hold the SAME block id.** That is not a
//!   flavour rule. `PendingEdits::begin_multi` carries ONE `inv_delta`
//!   for the whole group, so a homogeneous break is exactly what makes
//!   `(block, +N)` a correct description of what the player gained — and
//!   what lets a rejection revert it in one subtraction.
//! - **The timing multiplier is level-only** ([`crate::skills`]), never
//!   derived from how many cells actually qualify. A world-derived
//!   multiplier would let client and server disagree by a factor of
//!   three whenever their worlds differ by a single cell, which is far
//!   outside what `dig_grace` absorbs: the honest outcomes are a free
//!   completion or a `DigIncomplete` on a dig the player finished.

use crate::block::BlockType;
use crate::chunk::cell_id;
use crate::worldgen::ore::DigMode;

/// May this cell join the plane as an EXTRA? Asked of every candidate by
/// both ends over their own worlds, so the two swings touch the same set
/// — the plane is derived, and a rule applied on only one end is a
/// rejected swing at best and a silent divergence at worst.
///
/// The AIMED cell is never asked. It has already been through
/// `diggable`, the DigStart gates and the completion's own timing, and it
/// is the only cell allowed to be a slab shrink or a tapped vein. An
/// extra is the plain case or it is not in the group at all:
///
/// - **The same block id**, because `begin_multi` carries ONE `inv_delta`
///   and `(block, +N)` has to describe the whole group.
/// - **Diggable at this y**, which is what keeps the plane off the floor
///   of the world. A nine-wide swing at a wall standing on bedrock
///   includes y = 0, which holds the same Stone id — so nothing else
///   would have stopped it. Offline that punched a hole straight through
///   `protect::immutable`; online the protected-write scan rejected the
///   WHOLE swing, so the pick simply stopped working at the bottom of a
///   mine and blamed a cell the player never aimed at.
/// - **Not a door, and not a slab.** Both are cells the extras loop
///   cannot write correctly, because it writes bare air and the aimed
///   cell is the only one with a value derived for it. A door is one
///   object across two cells: a rank of doors in the plane became a rank
///   of orphaned halves standing on nothing, and the aimed door's own
///   partner sits inside the swung plane, where it was counted as a second block
///   broken and then written a second time by the door branch — one leaf
///   paying out two. A slab shrinks a third per hold; cleared outright it
///   pays one credit for up to three thirds, which is the matter rule
///   running backwards. Both make the standing comment at the extras loop
///   ("only the aimed cell can be a slab shrink or a tapped vein") true
///   instead of merely intended.
/// - **An ordinary break, never an extraction.** The reveal is a stamp on
///   the mesh, so a vein still HOLDS Stone and passes the id test. The
///   plane would clear it to air, credit Rubble instead of ore, pay stone
///   XP, and mark the cell edited — which `ore::revealed` reads as spent
///   forever, so the deposit could never come back. Veins are drawn out
///   one at a time, by the cell you aim at.
///
/// A locked SPAWN is deliberately not here. That is a policy, it is
/// server-only knowledge, and a locked cell in the plane is meant to
/// refuse the whole swing — `a_protected_cell_refuses_the_whole_pick_swing`
/// pins it, on the grounds that nothing should partially break. The world
/// floor is not policy: no dig of any kind touches it, both ends know
/// where it is, and `dig_hardness` refuses it at exactly the y
/// `protect::immutable` does.
pub fn breaks_as_extra(id: u16, value: u16, at: (i32, i32, i32), mode: DigMode) -> bool {
	let b = BlockType::from_id(cell_id(value));
	cell_id(value) == id
		&& b.dig_hardness(at.1).is_some()
		&& !b.is_door()
		&& !b.is_slab()
		&& mode == DigMode::Break
}

/// Which axis an axis-aligned unit normal points along. Anything else
/// answers the vertical axis, which is what a degenerate normal
/// deserves — the caller has already refused the edit by then.
fn axis_of(normal: (i32, i32, i32)) -> usize {
	match normal {
		(x, _, _) if x != 0 => 0,
		(_, _, z) if z != 0 => 2,
		_ => 1,
	}
}

/// The axis a ROW break runs along: the horizontal one in the face's
/// plane.
///
/// For a wall (normal along x or z) there is exactly one horizontal
/// choice and it is the other horizontal axis — a vertical row would be
/// a well, not a tunnel. For a floor or ceiling both in-plane axes are
/// horizontal, so it runs ACROSS the view: derived from the look
/// direction, which both ends already have on the wire.
fn row_axis(normal: (i32, i32, i32), dir: [f32; 3]) -> usize {
	match axis_of(normal) {
		0 => 2,
		2 => 0,
		// A floor or ceiling: across the walk, so a corridor widens
		// rather than lengthening.
		_ if dir[0].abs() >= dir[2].abs() => 2,
		_ => 0,
	}
}

/// The offsets one hold breaks, target first. `width` is the roster from
/// [`crate::unlock::pick_cells`]: a single cell (no pick), a row, or the
/// full plane.
fn offsets(normal: (i32, i32, i32), dir: [f32; 3], width: usize) -> Vec<(i32, i32, i32)> {
	let mut out = vec![(0, 0, 0)];
	if width < 3 {
		return out;
	}
	let n = axis_of(normal);
	let row = row_axis(normal, dir);
	// The other in-plane axis, for the full plane.
	let up = (0..3).find(|&a| a != n && a != row).unwrap_or(1);
	let mut push = |a: usize, b: Option<usize>, da: i32, db: i32| {
		let mut o = [0i32; 3];
		o[a] = da;
		if let Some(b) = b {
			o[b] = db;
		}
		if o != [0, 0, 0] {
			out.push((o[0], o[1], o[2]));
		}
	};
	for da in [-1, 1] {
		push(row, None, da, 0);
	}
	if width >= 9 {
		for db in [-1, 1] {
			push(up, None, db, 0);
			for da in [-1, 1] {
				push(row, Some(up), da, db);
			}
		}
	}
	out
}

/// The cells one hold breaks, target FIRST and every extra the caller's
/// `extra_ok` accepts — which should be [`breaks_as_extra`] against that
/// end's own world, and nothing looser.
///
/// The order is deterministic and identical on both ends, which is what
/// lets `begin_multi`'s single `inv_delta` describe the whole group.
pub fn cells(
	cell: (i32, i32, i32),
	normal: (i32, i32, i32),
	dir: [f32; 3],
	width: usize,
	extra_ok: impl Fn((i32, i32, i32)) -> bool,
) -> Vec<(i32, i32, i32)> {
	offsets(normal, dir, width)
		.into_iter()
		.map(|(dx, dy, dz)| (cell.0 + dx, cell.1 + dy, cell.2 + dz))
		.filter(|&c| c == cell || extra_ok(c))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::chunk::make_cell;

	/// The four ways a neighbour is NOT an ordinary extra. Each one was a
	/// real break: the plane reached bedrock, shattered a rank of doors
	/// into orphaned halves, double-paid the aimed door's own partner, and
	/// cleared revealed veins to air for Rubble — the last one destroying
	/// the deposit permanently, since a cleared cell is an edited cell and
	/// `ore::revealed` never comes back from that.
	#[test]
	fn an_extra_has_to_be_the_plain_case() {
		let stone = make_cell(BlockType::Stone, 0);
		let id = cell_id(stone);
		// The plain case, well above the floor.
		assert!(breaks_as_extra(id, stone, (0, 40, 0), DigMode::Break));

		// A different block is not in the group — `inv_delta` is one pair.
		let dirt = make_cell(BlockType::Dirt, 0);
		assert!(!breaks_as_extra(id, dirt, (0, 40, 0), DigMode::Break));

		// The world floor holds the same id and must still be refused.
		assert!(
			!breaks_as_extra(id, stone, (0, 0, 0), DigMode::Break),
			"the plane reached bedrock"
		);

		// A door is one object across two cells; the extras loop writes
		// bare air and would leave its partner floating.
		let door = make_cell(BlockType::LumberDoor, 0);
		assert!(!breaks_as_extra(
			cell_id(door),
			door,
			(0, 40, 0),
			DigMode::Break
		));

		// A slab is thirds: cleared outright it pays one credit for up to
		// three of them.
		let slab = make_cell(BlockType::WoodSlab, 0);
		assert!(!breaks_as_extra(
			cell_id(slab),
			slab,
			(0, 40, 0),
			DigMode::Break
		));

		// A revealed vein still HOLDS Stone, so only the mode can tell.
		assert!(!breaks_as_extra(
			id,
			stone,
			(0, 40, 0),
			DigMode::Extract(BlockType::Emberore)
		));
		assert!(!breaks_as_extra(
			id,
			stone,
			(0, 40, 0),
			DigMode::NeedsSiphon
		));
	}

	/// A wall breaks HORIZONTALLY: three wide is a tunnel, and the
	/// vertical row it would otherwise cut is a well nobody asked for.
	#[test]
	fn a_three_wide_break_runs_across_the_face() {
		let look = [0.0, 0.0, 1.0];
		// Facing +z at a wall: the row runs along x.
		let out = cells((0, 0, 0), (0, 0, -1), look, 3, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (-1, 0, 0), (1, 0, 0)]);
		// Facing +x at a wall: along z.
		let out = cells((0, 0, 0), (-1, 0, 0), [1.0, 0.0, 0.0], 3, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (0, 0, -1), (0, 0, 1)]);
		// Straight down at the floor, walking along x: the row runs
		// ACROSS the walk, widening the corridor rather than lengthening.
		let out = cells((0, 0, 0), (0, 1, 0), [1.0, -0.2, 0.0], 3, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (0, 0, -1), (0, 0, 1)]);
		// And the same floor walked along z runs along x.
		let out = cells((0, 0, 0), (0, 1, 0), [0.0, -0.2, 1.0], 3, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (-1, 0, 0), (1, 0, 0)]);
	}

	/// Nine cells, all in the face's plane and never behind it — a pick
	/// widens the wall it is aimed at, it does not tunnel through it.
	#[test]
	fn the_full_pattern_is_a_plane_not_a_bore() {
		for normal in [
			(1, 0, 0),
			(-1, 0, 0),
			(0, 1, 0),
			(0, -1, 0),
			(0, 0, 1),
			(0, 0, -1),
		] {
			let out = cells((5, 5, 5), normal, [0.3, -0.9, 0.2], 9, |_| true);
			assert_eq!(out.len(), 9, "{normal:?}");
			assert_eq!(out[0], (5, 5, 5), "the aimed cell leads");
			let n = axis_of(normal);
			for &c in &out {
				let o = [c.0 - 5, c.1 - 5, c.2 - 5];
				assert_eq!(o[n], 0, "{normal:?} — {c:?} left the plane");
				assert!(o.iter().all(|v| v.abs() <= 1), "{c:?}");
			}
			let mut seen = out.clone();
			seen.sort_unstable();
			seen.dedup();
			assert_eq!(seen.len(), 9, "{normal:?} — a cell listed twice");
		}
	}

	/// Homogeneity, which `begin_multi`'s single `inv_delta` depends on:
	/// a neighbour of a different id is simply not in the group, and the
	/// aimed cell is always in it however alone it stands.
	#[test]
	fn only_matching_neighbours_join_the_group() {
		let out = cells((0, 0, 0), (0, 0, -1), [0.0, 0.0, 1.0], 9, |c| c.1 == 0);
		assert!(out.contains(&(0, 0, 0)) && out.contains(&(1, 0, 0)));
		assert!(!out.iter().any(|c| c.1 != 0), "a foreign row joined");
		// Alone in the world, the target still breaks.
		let out = cells((0, 0, 0), (0, 0, -1), [0.0, 0.0, 1.0], 9, |_| false);
		assert_eq!(out, vec![(0, 0, 0)]);
		// And with no pick at all there is nothing but the target.
		let out = cells((0, 0, 0), (0, 0, -1), [0.0, 0.0, 1.0], 1, |_| true);
		assert_eq!(out, vec![(0, 0, 0)]);
	}
}
