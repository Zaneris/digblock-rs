//! The Resonant Pick's break pattern (SKILLS-DESIGN §5.6).
//!
//! One hold, several cells: the plane perpendicular to the face you hit,
//! three wide at Mining 55 and the full nine at 85. The pair is derived
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

/// Which axis an axis-aligned unit normal points along (0 = x, 1 = y,
/// 2 = z). Anything else answers y, which is what a degenerate normal
/// deserves — the caller has already refused the edit by then.
fn axis_of(normal: (i32, i32, i32)) -> usize {
	match normal {
		(x, _, _) if x != 0 => 0,
		(_, _, z) if z != 0 => 2,
		_ => 1,
	}
}

/// The axis a 3-WIDE break runs along: the horizontal one in the face's
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
/// [`crate::unlock::pick_cells`]: 1 (no pick), 3, or 9.
fn offsets(normal: (i32, i32, i32), dir: [f32; 3], width: usize) -> Vec<(i32, i32, i32)> {
	let mut out = vec![(0, 0, 0)];
	if width < 3 {
		return out;
	}
	let n = axis_of(normal);
	let row = row_axis(normal, dir);
	// The other in-plane axis, for the full 3×3.
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

/// The cells one hold breaks, target FIRST and every extra holding the
/// same block id as the target (`same` is the caller's world lookup).
///
/// The order is deterministic and identical on both ends, which is what
/// lets `begin_multi`'s single `inv_delta` describe the whole group.
pub fn cells(
	cell: (i32, i32, i32),
	normal: (i32, i32, i32),
	dir: [f32; 3],
	width: usize,
	same: impl Fn((i32, i32, i32)) -> bool,
) -> Vec<(i32, i32, i32)> {
	offsets(normal, dir, width)
		.into_iter()
		.map(|(dx, dy, dz)| (cell.0 + dx, cell.1 + dy, cell.2 + dz))
		.filter(|&c| c == cell || same(c))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

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
