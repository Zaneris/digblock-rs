//! The Resonant Pick's break pattern (SKILLS-DESIGN §5.6): one hold, several cells — the plane
//! perpendicular to the face you hit, a row at the first Mining milestone and the full plane at
//! the second. The set is derived from the ray on BOTH ends plus a declared [`DigOrientation`]
//! that itself rides the reliable stream and is clamped by both ends — the orientation is chosen,
//! the cells are not, so there is no cheat surface and no second opinion about which cells an
//! action touched. Two rules make the rest of the machinery work unchanged:
//!
//! - **Every extra cell must pay the SAME credit**, because `PendingEdits::begin_multi` carries
//!   ONE `inv_delta` for the whole group, which is also what lets a rejection revert it in one
//!   subtraction. Equal block ids are not enough: provenance decides what a cell hands back
//!   ([`BlockType::dig_credit`]), so a plane straddling a quarry face and a mason's wall holds
//!   one id while paying out two items.
//! - **The timing multiplier is level-only** ([`crate::skills`]), never derived from how many
//!   cells qualify: a world-derived one would let the two ends disagree by the whole multiplier
//!   over a single differing cell, far outside what `dig_grace` absorbs.

use crate::block::BlockType;
use crate::chunk::cell_id;
use crate::worldgen::nodes::DigMode;

/// The pick's declared dig shape, cycled by the tool-ability key (or its touch tap). The toggle
/// always swaps between the face's two in-plane axes — across your look versus along it — one
/// uniform rule on every face, and it never bores along the normal. Wire form is a bare u8.
///
/// The DEFAULT is `Plane`, not `Horizontal`, and the choice is a compatibility statement: before
/// the toggle existed, width alone decided the shape — a row at the first milestone, the plane at
/// the second. `Plane` clamped by the unlock reproduces exactly that at every level, so a client
/// that never touches the toggle (a v40 peer included, which cannot send one) digs what it always
/// dug.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DigOrientation {
	/// A row across the look: the tunnel-widening cut.
	Horizontal = 0,
	/// The face's other in-plane axis: a true column on a wall (a doorway cut); on a floor or
	/// ceiling, the along-your-look row (a trench lengthens instead of widening).
	Vertical = 1,
	/// The full plane — [`crate::unlock::PICK_PLANE`] cells, orientation-free by shape.
	#[default]
	Plane = 2,
}

impl DigOrientation {
	pub fn from_wire(v: u8) -> Self {
		match v {
			1 => Self::Vertical,
			2 => Self::Plane,
			_ => Self::Horizontal,
		}
	}

	pub fn to_wire(self) -> u8 {
		self as u8
	}

	/// Degrade against the unlock: `Plane` below the plane width falls to `Horizontal`. BOTH ends
	/// run this over the same `unlock::pick_cells` width, which is what keeps their footprints
	/// bit-identical when a client asks for more than its Mining level has opened.
	pub fn clamped(self, width: usize) -> Self {
		if self == Self::Plane && width < crate::unlock::PICK_PLANE {
			Self::Horizontal
		} else {
			self
		}
	}
}

/// May this cell join the plane as an EXTRA? Asked of every candidate by both ends over their own
/// worlds, so the two swings touch the same set; the AIMED cell is never asked, being the only one
/// allowed to be a slab shrink or a tapped vein. Clearing a vein as an extra would pay the host's
/// credit and mark the cell edited, which the anti-dupe reads as spent forever. A locked SPAWN is
/// deliberately absent — server-only knowledge, and it must refuse the WHOLE swing.
pub fn breaks_as_extra(
	id: u16,
	credit: Option<BlockType>,
	value: u16,
	at: (i32, i32, i32),
	mode: DigMode,
	edited: bool,
	alchemy: u32,
) -> bool {
	let b = BlockType::from_id(cell_id(value));
	cell_id(value) == id
		&& b.dig_credit(edited, alchemy) == credit
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

/// The axis a ROW break runs along: the horizontal one in the face's plane. For a wall there is
/// exactly one horizontal choice, the other horizontal axis — a vertical row would be a well, not
/// a tunnel. For a floor or ceiling both in-plane axes are horizontal, so it runs ACROSS the view,
/// derived from the look direction both ends already have on the wire.
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
/// full plane. `Horizontal` emits along `row` and `Vertical` along `up` —
/// the plane keeps its canonical `row`/`up` walk so its nine cells hold
/// one byte-identical order whatever the toggle says.
fn offsets(
	normal: (i32, i32, i32),
	dir: [f32; 3],
	width: usize,
	o: DigOrientation,
) -> Vec<(i32, i32, i32)> {
	let mut out = vec![(0, 0, 0)];
	if width < 3 {
		return out;
	}
	let n = axis_of(normal);
	let row = row_axis(normal, dir);
	// The other in-plane axis, for the vertical row and the full plane.
	let up = (0..3).find(|&a| a != n && a != row).unwrap_or(1);
	// The belt to `clamped`'s suspenders: a Plane that somehow reaches a
	// row-width call still emits the horizontal row.
	let plane = width >= crate::unlock::PICK_PLANE && o == DigOrientation::Plane;
	let mut push = |a: usize, b: Option<usize>, da: i32, db: i32| {
		let mut off = [0i32; 3];
		off[a] = da;
		if let Some(b) = b {
			off[b] = db;
		}
		if off != [0, 0, 0] {
			out.push((off[0], off[1], off[2]));
		}
	};
	let along = if o == DigOrientation::Vertical {
		up
	} else {
		row
	};
	for da in [-1, 1] {
		push(along, None, da, 0);
	}
	if plane {
		let across = if along == row { up } else { row };
		for db in [-1, 1] {
			push(across, None, db, 0);
			for da in [-1, 1] {
				push(along, Some(across), da, db);
			}
		}
	}
	out
}

/// The cells one hold breaks, target FIRST and every extra the caller's `extra_ok` accepts — which
/// should be [`breaks_as_extra`] against that end's own world, and nothing looser. The order is
/// deterministic and identical on both ends, which is what lets `begin_multi`'s single `inv_delta`
/// describe the whole group; `o` must already be [`DigOrientation::clamped`] against `width`.
pub fn cells(
	cell: (i32, i32, i32),
	normal: (i32, i32, i32),
	dir: [f32; 3],
	width: usize,
	o: DigOrientation,
	extra_ok: impl Fn((i32, i32, i32)) -> bool,
) -> Vec<(i32, i32, i32)> {
	offsets(normal, dir, width, o)
		.into_iter()
		.map(|(dx, dy, dz)| (cell.0 + dx, cell.1 + dy, cell.2 + dz))
		.filter(|&c| c == cell || extra_ok(c))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::chunk::make_cell;

	/// The five ways a neighbour is NOT an ordinary extra: the plane reaching bedrock, shattering a
	/// rank of doors into orphaned halves, double-paying the aimed door's own partner, clearing a
	/// revealed vein to air for Rubble — which destroys the deposit permanently, since a cleared
	/// cell is an edited cell and `ore::revealed` never comes back from that — and the conditional
	/// rock rule, where two cells hold the same id and still pay out different items.
	#[test]
	fn an_extra_has_to_be_the_plain_case() {
		let stone = make_cell(BlockType::Stone, 0);
		let id = cell_id(stone);
		// The aimed cell is the mountain's own rock, dug by a mason.
		let mason = crate::unlock::craft(BlockType::Stone).expect("clean stone is craftable");
		let rubble = BlockType::Stone.dig_credit(false, mason);
		let extra =
			|value, at, mode, edited| breaks_as_extra(id, rubble, value, at, mode, edited, mason);
		// The plain case, well above the floor.
		assert!(extra(stone, (0, 40, 0), DigMode::Break, false));

		// A different block is not in the group — `inv_delta` is one pair.
		let dirt = make_cell(BlockType::Dirt, 0);
		assert!(!extra(dirt, (0, 40, 0), DigMode::Break, false));

		// Nor is a stone somebody PLACED, which holds the very same id and
		// hands back the clean block where its neighbours crumble.
		assert!(
			!extra(stone, (0, 40, 0), DigMode::Break, true),
			"a placed stone pays a different item"
		);

		// The world floor holds the same id and must still be refused.
		assert!(
			!extra(stone, (0, 0, 0), DigMode::Break, false),
			"the plane reached bedrock"
		);

		// A door is one object across two cells; the extras loop writes
		// bare air and would leave its partner floating.
		let door = make_cell(BlockType::LumberDoor, 0);
		assert!(!breaks_as_extra(
			cell_id(door),
			BlockType::LumberDoor.dig_credit(false, mason),
			door,
			(0, 40, 0),
			DigMode::Break,
			false,
			mason
		));

		// A slab is thirds: cleared outright it pays one credit for up to
		// three of them.
		let slab = make_cell(BlockType::WoodSlab, 0);
		assert!(!breaks_as_extra(
			cell_id(slab),
			BlockType::WoodSlab.dig_credit(false, mason),
			slab,
			(0, 40, 0),
			DigMode::Break,
			false,
			mason
		));

		// A revealed vein still HOLDS Stone, so only the mode can tell.
		assert!(!extra(
			stone,
			(0, 40, 0),
			DigMode::Extract(BlockType::Emberore),
			false
		));
		assert!(!extra(stone, (0, 40, 0), DigMode::NeedsSiphon, false));
	}

	/// A wall breaks HORIZONTALLY: three wide is a tunnel — the expected
	/// vectors are the pre-toggle goldens, pinned byte-identical under
	/// the DEFAULT orientation clamped at row width.
	#[test]
	fn a_three_wide_break_runs_across_the_face() {
		let o = DigOrientation::default().clamped(3);
		assert_eq!(
			o,
			DigOrientation::Horizontal,
			"the default clamps to the shipped row"
		);
		let look = [0.0, 0.0, 1.0];
		// Facing +z at a wall: the row runs along x.
		let out = cells((0, 0, 0), (0, 0, -1), look, 3, o, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (-1, 0, 0), (1, 0, 0)]);
		// Facing +x at a wall: along z.
		let out = cells((0, 0, 0), (-1, 0, 0), [1.0, 0.0, 0.0], 3, o, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (0, 0, -1), (0, 0, 1)]);
		// Straight down at the floor, walking along x: the row runs
		// ACROSS the walk, widening the corridor rather than lengthening.
		let out = cells((0, 0, 0), (0, 1, 0), [1.0, -0.2, 0.0], 3, o, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (0, 0, -1), (0, 0, 1)]);
		// And the same floor walked along z runs along x.
		let out = cells((0, 0, 0), (0, 1, 0), [0.0, -0.2, 1.0], 3, o, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (-1, 0, 0), (1, 0, 0)]);
	}

	/// The vertical toggle: a true column on a wall — the doorway cut —
	/// and on a floor or ceiling the ALONG-your-look row, because a ±y
	/// row there would leave the face plane and bore. One rule on every
	/// face: the toggle swaps to the face's other in-plane axis.
	#[test]
	fn a_vertical_row_is_the_faces_other_axis() {
		let o = DigOrientation::Vertical;
		// A wall: the column.
		let out = cells((0, 0, 0), (0, 0, -1), [0.0, 0.0, 1.0], 3, o, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (0, -1, 0), (0, 1, 0)]);
		let out = cells((0, 0, 0), (-1, 0, 0), [1.0, 0.0, 0.0], 3, o, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (0, -1, 0), (0, 1, 0)]);
		// A floor walked along x: the trench LENGTHENS (±x), never ±y.
		let out = cells((0, 0, 0), (0, 1, 0), [1.0, -0.2, 0.0], 3, o, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (-1, 0, 0), (1, 0, 0)]);
		// The same floor walked along z lengthens along z.
		let out = cells((0, 0, 0), (0, 1, 0), [0.0, -0.2, 1.0], 3, o, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (0, 0, -1), (0, 0, 1)]);
		for normal in [(0, 1, 0), (0, -1, 0)] {
			let out = cells((0, 0, 0), normal, [0.7, -0.5, 0.2], 3, o, |_| true);
			assert!(
				out.iter().all(|c| c.1 == 0),
				"{normal:?}: a floor's vertical row bored along the normal"
			);
		}
		// At plane width the toggle still means a ROW: vertical is not
		// clamped away by the wider unlock.
		let out = cells((0, 0, 0), (0, 0, -1), [0.0, 0.0, 1.0], 9, o, |_| true);
		assert_eq!(out, vec![(0, 0, 0), (0, -1, 0), (0, 1, 0)]);
	}

	/// The clamp table, and the byte-identity it protects: the DEFAULT is
	/// the plane so a client that never toggles digs what it always dug —
	/// a row at row width, the plane at plane width.
	#[test]
	fn the_clamp_reproduces_the_pre_toggle_shapes() {
		use DigOrientation as O;
		assert_eq!(O::default(), O::Plane);
		assert_eq!(O::Plane.clamped(3), O::Horizontal);
		assert_eq!(O::Plane.clamped(1), O::Horizontal);
		assert_eq!(O::Plane.clamped(crate::unlock::PICK_PLANE), O::Plane);
		assert_eq!(
			O::Horizontal.clamped(crate::unlock::PICK_PLANE),
			O::Horizontal
		);
		assert_eq!(O::Vertical.clamped(3), O::Vertical);
		// The wire roundtrip, and the hostile-byte rule.
		for o in [O::Horizontal, O::Vertical, O::Plane] {
			assert_eq!(O::from_wire(o.to_wire()), o);
		}
		assert_eq!(O::from_wire(7), O::Horizontal);
		// Belt and suspenders: even UNclamped, a plane ask at row width
		// emits the horizontal row.
		let out = cells((0, 0, 0), (0, 0, -1), [0.0, 0.0, 1.0], 3, O::Plane, |_| {
			true
		});
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
			let out = cells(
				(5, 5, 5),
				normal,
				[0.3, -0.9, 0.2],
				9,
				DigOrientation::default().clamped(9),
				|_| true,
			);
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
		let out = cells(
			(0, 0, 0),
			(0, 0, -1),
			[0.0, 0.0, 1.0],
			9,
			DigOrientation::Plane,
			|c| c.1 == 0,
		);
		assert!(out.contains(&(0, 0, 0)) && out.contains(&(1, 0, 0)));
		assert!(!out.iter().any(|c| c.1 != 0), "a foreign row joined");
		// Alone in the world, the target still breaks.
		let out = cells(
			(0, 0, 0),
			(0, 0, -1),
			[0.0, 0.0, 1.0],
			9,
			DigOrientation::Plane,
			|_| false,
		);
		assert_eq!(out, vec![(0, 0, 0)]);
		// And with no pick at all there is nothing but the target.
		let out = cells(
			(0, 0, 0),
			(0, 0, -1),
			[0.0, 0.0, 1.0],
			1,
			DigOrientation::Horizontal,
			|_| true,
		);
		assert_eq!(out, vec![(0, 0, 0)]);
	}
}
