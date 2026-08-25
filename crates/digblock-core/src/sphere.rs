//! The Sphere: a one-cell-thick shell of the caster's HELD block conjured
//! around their head, the spellbook's first spell that writes cells. The
//! shell is built of real blocks paid out of the caster's inventory, so a
//! dig hands them back as any placed block would. Every cell is derived
//! here from the head, the radius and the block on BOTH ends, so the
//! client names nothing but the block the server does not re-derive; the
//! plan is all or nothing, the Arc's rule, since a partial sphere would
//! leave the two ends holding different sets.
//!
//! The plan's ORDER is the spectacle: the cells land in plan order
//! across the cast's own channel, floor to crown with each ring swept
//! around, so the shell rises about the caster as they channel and seals
//! overhead the tick the channel completes — and an interrupted channel
//! UNWINDS what had landed. Both ends run the same [`Closing`]; nothing
//! is animated but the world itself.

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

/// The widest sphere anything may ask for, ladder or debug knob. Every castable rank
/// ([`crate::unlock::sphere_radius`]) sits well inside it — held by
/// `the_cap_clears_every_castable_rank` — and the cap is what keeps an arbitrary caller honest:
/// [`inside`] SQUARES the radius, so a large enough one overflows, and the scan is cubic in it,
/// so a merely big one hangs the frame that asked.
pub const MAX_RADIUS: i32 = 24;

fn inside(head: Cell, r: i32, c: Cell) -> bool {
	let (dx, dy, dz) = (c.0 - head.0, c.1 - head.1, c.2 - head.2);
	dx * dx + dy * dy + dz * dz <= r * r
}

fn neighbours(c: Cell) -> impl Iterator<Item = Cell> {
	FACES
		.into_iter()
		.map(move |d| (c.0 + d.0, c.1 + d.1, c.2 + d.2))
}

/// Whether a cell exists at all. A reader answers `None` both for a
/// column it has not loaded and for a `y` outside the world, and the two
/// deserve opposite answers here: the first refuses the cast, the second
/// is simply not there to write. Derived from the one place that defines
/// the vertical bound rather than restating it.
fn in_world(c: Cell) -> bool {
	crate::chunk::split_world(c.0, c.1, c.2).is_some()
}

/// The sphere's two sets: the SHELL — every cell within the radius that shares a face with one
/// beyond it — and the INTERIOR, everything within the radius the shell does not claim. Defining
/// the shell as the interior's face-boundary is what makes it both sealed and as thin as a sealed
/// shell can be. Both are emitted in scan order, so the two ends agree on sequence as well as
/// membership. The radius is clamped to [`MAX_RADIUS`] here — the one gate every caller uses.
pub fn sets(head: Cell, r: i32) -> (Vec<Cell>, Vec<Cell>) {
	let r = r.clamp(0, MAX_RADIUS);
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
/// exactly where the world was not loaded. A cell OUTSIDE the world is
/// the other case entirely — see [`in_world`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
	Unloaded,
}

/// What a sphere may be built of, DERIVED so a reshaped row moves on its own: a block the bar
/// can hold, that a body collides with (`shapes::collides` — a shell is a wall, and a torch is
/// not), that is a material's plain block or a thing rather than a worked FORM (a slab, stair
/// or door is oriented by its placer against a face, and the sphere has none to offer), and whose
/// whole object at rest fits ONE cell by `shapes::object_bounds` — the test a door fails today.
pub fn accepts(b: BlockType) -> bool {
	use crate::block::ShapeFamily;
	use crate::shapes::{GRID, collides, object_bounds};
	let at_rest = make_cell(b, 0);
	if b == BlockType::Air || !b.is_slottable() || !collides(at_rest) {
		return false;
	}
	if !matches!(b.shape_family(), Some(ShapeFamily::Block) | None) {
		return false;
	}
	let (_, bounds) = object_bounds((0, 0, 0), at_rest);
	bounds.is_none_or(|bx| bx.max.iter().all(|&m| m <= GRID))
}

/// The writes one cast performs, in the order they land (`closing_key`): shell cells of air or
/// water become `block` at rest, interior water becomes air, everything else stands. Water in the
/// shell must be claimed too — a wave left standing is a hole. `read` answers the cell's CURRENT
/// value, `None` for one not loaded; a cell past the world's vertical bounds is SKIPPED rather than
/// refused, safe because the floor is bedrock. The block is the caller's to check with [`accepts`].
pub fn plan(
	head: Cell,
	r: i32,
	block: BlockType,
	read: impl Fn(Cell) -> Option<u16>,
) -> Result<Vec<(Cell, u16)>, Refusal> {
	let (shell, interior) = sets(head, r);
	let mut out = Vec::new();
	for c in shell {
		if !in_world(c) {
			continue;
		}
		let v = read(c).ok_or(Refusal::Unloaded)?;
		let b = BlockType::from_id(cell_id(v));
		if b == BlockType::Air || b.is_water() {
			out.push((c, make_cell(block, 0)));
		}
	}
	for c in interior {
		if !in_world(c) {
			continue;
		}
		let v = read(c).ok_or(Refusal::Unloaded)?;
		if BlockType::from_id(cell_id(v)).is_water() {
			out.push((c, make_cell(BlockType::Air, 0)));
		}
	}
	out.sort_by(|a, b| {
		closing_key(head, a.0)
			.partial_cmp(&closing_key(head, b.0))
			.expect("finite")
	});
	Ok(out)
}

/// Where a cell falls in the closing: its layer, then its bearing from the caster, then its
/// distance out along that bearing. TOTAL over distinct cells, and the third term is what makes
/// it so: a layer's ring is several cells thick once interior drains join the shell, so a bearing
/// alone ties every cell along one. Angle and radius together name an integer offset uniquely, so
/// the sort is a pure function of the SET and both ends land the same sequence.
fn closing_key(head: Cell, c: Cell) -> (i32, f32, i32) {
	let (dx, dz) = (c.0 - head.0, c.2 - head.2);
	(c.1, (dz as f32).atan2(dx as f32), dx * dx + dz * dz)
}

/// What a plan costs in blocks: the cells that stand `block` up, which is every write but the
/// interior drains. The inventory pays this as the channel begins and takes back, at the end,
/// whatever the climb declined to write or an unwind put down again.
pub fn blocks_needed(writes: &[(Cell, u16)], block: BlockType) -> u32 {
	let placed = make_cell(block, 0);
	u32::try_from(writes.iter().filter(|w| w.1 == placed).count()).unwrap_or(u32::MAX)
}

/// How many of `total` cells have landed after `step` of a `span`-tick
/// climb: linear, and exactly all of them at the span — a sphere of any
/// size closes in the channel's own time.
pub fn landed_by(total: usize, step: u32, span: u32) -> usize {
	if step >= span {
		return total;
	}
	(total as u64 * u64::from(step) / u64::from(span)) as usize
}

/// A sphere in the middle of landing — the plan, how far through it the
/// clock has got, and what has actually been written so far with what
/// it replaced, which is what an unwind restores. Each end runs its own
/// through its own write path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closing {
	writes: Vec<(Cell, u16)>,
	span: u32,
	step: u32,
	landed: Vec<(Cell, u16, u16)>,
}

impl Closing {
	/// `span` is the climb in ticks — the cast's channel.
	pub fn new(writes: Vec<(Cell, u16)>, span: u32) -> Self {
		Self {
			writes,
			span: span.max(1),
			step: 0,
			landed: Vec::new(),
		}
	}

	/// Advance one tick and hand back the cells that land on it. The
	/// caller writes each through its own path and `record`s what it
	/// replaced — a cell it declined to write is simply not recorded,
	/// and an unwind will not touch it.
	pub fn land(&mut self) -> Vec<(Cell, u16)> {
		let from = landed_by(self.writes.len(), self.step, self.span);
		self.step += 1;
		let to = landed_by(self.writes.len(), self.step, self.span);
		self.writes[from..to].to_vec()
	}

	pub fn record(&mut self, cell: Cell, written: u16, prev: u16) {
		self.landed.push((cell, written, prev));
	}

	pub fn done(&self) -> bool {
		self.step >= self.span
	}

	pub fn step(&self) -> u32 {
		self.step
	}

	/// What an interrupt puts back: every recorded `(cell, written, prev)`
	/// crown-first, so the shell recedes the way it rose. A caller
	/// restores a cell only while it still holds `written` — somebody
	/// who dug a block out mid-climb keeps their hole.
	pub fn unwind(self) -> Vec<(Cell, u16, u16)> {
		let mut landed = self.landed;
		landed.reverse();
		landed
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::unlock;
	use std::collections::HashSet;

	/// A head well clear of both world bounds, so a sphere about it is
	/// entirely inside the world — the floor case is
	/// [`a_sphere_clipped_by_the_world_floor_still_casts`]'s alone.
	const HEAD: Cell = (3, 70, 11);

	/// Every radius the ladder can cast at — derived from the unlock
	/// table so a retune sweeps the new sizes.
	fn radii() -> Vec<i32> {
		let mut out: Vec<i32> = (0..=100)
			.map(unlock::sphere_radius)
			.filter(|&r| r > 0)
			.collect();
		out.dedup();
		assert!(out.len() >= 2, "the ladder has ranks");
		out
	}

	/// A dense cube of flags about a head, one cell wider than the sphere
	/// in every direction — the flood fill's scratch, and the blocked set
	/// it reads. Dense because the sweep below runs one fill per shell
	/// cell and a hash lookup per neighbour would make that minutes.
	struct Cube {
		head: Cell,
		r: i32,
		flags: Vec<bool>,
	}

	impl Cube {
		fn new(head: Cell, r: i32) -> Self {
			let n = (2 * r + 3) as usize;
			Self {
				head,
				r,
				flags: vec![false; n * n * n],
			}
		}

		fn idx(&self, c: Cell) -> Option<usize> {
			let n = 2 * self.r + 3;
			let (x, y, z) = (
				c.0 - self.head.0 + self.r + 1,
				c.1 - self.head.1 + self.r + 1,
				c.2 - self.head.2 + self.r + 1,
			);
			let ok = [x, y, z].iter().all(|v| (0..n).contains(v));
			ok.then(|| ((z * n + y) * n + x) as usize)
		}

		fn get(&self, c: Cell) -> bool {
			self.idx(c).is_some_and(|i| self.flags[i])
		}

		fn set(&mut self, c: Cell) {
			if let Some(i) = self.idx(c) {
				self.flags[i] = true;
			}
		}
	}

	/// Whether water starting at the head could ever reach beyond the radius, crossing faces and
	/// stopping at `blocked`. The definition of sealed that owes nothing to [`sets`]: it asks the
	/// sphere the question fluid asks, instead of re-running the predicate that built it. A cell
	/// one step outside the radius is still inside the cube, so the fill can never leave it.
	fn escapes(head: Cell, r: i32, blocked: &Cube) -> bool {
		let mut seen = Cube::new(head, r);
		let mut stack = vec![head];
		seen.set(head);
		while let Some(c) = stack.pop() {
			if !inside(head, r, c) {
				return true;
			}
			for n in neighbours(c) {
				if blocked.get(n) || seen.get(n) {
					continue;
				}
				seen.set(n);
				stack.push(n);
			}
		}
		false
	}

	fn shell_cube(head: Cell, r: i32, shell: &[Cell]) -> Cube {
		let mut cube = Cube::new(head, r);
		for &c in shell {
			cube.set(c);
		}
		cube
	}

	/// The shell is a SEALED surface confined to ONE rind, and every pane facing the inside is
	/// load-bearing. Three statements, none of which re-runs the predicate `sets` classified by —
	/// the only way a shell with a hole could pass: sealed, as fluid asks it (a face fill from the
	/// head never reaches past the radius); thin, as geometry asks it (every shell cell lies in the
	/// outermost rind); and minimal (knock out a pane the interior touches and the fill leaks).
	#[test]
	fn the_shell_seals_one_rind_and_every_facing_pane_holds() {
		for r in radii() {
			let (shell, interior) = sets(HEAD, r);
			assert!(shell.len() > 6 && interior.len() > 6, "r={r}");
			assert!(
				!escapes(HEAD, r, &shell_cube(HEAD, r, &shell)),
				"r={r}: the shell leaks"
			);
			let shell_set: HashSet<Cell> = shell.iter().copied().collect();
			for &c in &interior {
				assert!(!shell_set.contains(&c), "r={r}: {c:?} in both sets");
			}
			for &c in &shell {
				let (dx, dy, dz) = (c.0 - HEAD.0, c.1 - HEAD.1, c.2 - HEAD.2);
				let d2 = dx * dx + dy * dy + dz * dz;
				assert!(
					d2 > (r - 1) * (r - 1) && d2 <= r * r,
					"r={r}: shell {c:?} sits {d2} from the head, off the rind"
				);
			}
			let inner: HashSet<Cell> = interior.iter().copied().collect();
			let mut facing = 0;
			for &gap in shell
				.iter()
				.filter(|&&c| neighbours(c).any(|n| inner.contains(&n)))
			{
				let mut holed = shell_cube(HEAD, r, &shell);
				let i = holed.idx(gap).expect("a shell cell is in its own cube");
				holed.flags[i] = false;
				assert!(escapes(HEAD, r, &holed), "r={r}: {gap:?} held nothing in");
				facing += 1;
			}
			assert!(facing > 6, "r={r}: the sweep found no facing pane");
		}
	}

	/// The caster stands inside their own sphere: the head cell and the
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

	/// The block the sweeps build with — the first one the predicate
	/// admits, so a retune of the roster cannot leave them building
	/// with something the spell refuses.
	fn matter() -> BlockType {
		BlockType::ALL
			.iter()
			.copied()
			.find(|&b| accepts(b))
			.expect("something can be built into a sphere")
	}

	/// The plan writes exactly what the rules say: air and water in the
	/// shell turn to the held block, water inside turns to air, and rock
	/// in either place stands. Each rule is checked to have fired, and
	/// the block count the inventory pays is exactly the cells stood up.
	#[test]
	fn the_plan_walls_the_shell_and_drains_the_inside() {
		let r = radii()[0];
		let block = matter();
		let wall = make_cell(block, 0);
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
		let writes = plan(HEAD, r, block, world).unwrap();
		let written: HashSet<Cell> = writes.iter().map(|w| w.0).collect();
		let (mut walled_air, mut walled_water, mut drained, mut spared) = (0, 0, 0, 0);
		for &c in &shell {
			let v = world(c).unwrap();
			let b = BlockType::from_id(cell_id(v));
			let w = writes.iter().find(|w| w.0 == c).map(|w| w.1);
			match b {
				BlockType::Air => {
					assert_eq!(w, Some(wall), "{c:?}");
					walled_air += 1;
				}
				BlockType::Water => {
					assert_eq!(w, Some(wall), "{c:?}");
					walled_water += 1;
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
		assert!(walled_air > 0 && walled_water > 0 && drained > 0 && spared > 0);
		assert_eq!(written.len(), writes.len(), "no cell written twice");
		assert_eq!(
			blocks_needed(&writes, block),
			walled_air + walled_water,
			"the bill is the cells stood up and not the drains"
		);
	}

	/// What the shell may be made of, asked of every block and checked against the properties
	/// the answer derives from rather than a list: every accepted block is one the bar holds, one a
	/// body cannot walk through, never a worked form, and one whose object at rest stays inside its
	/// cell — so the torch fails on its empty collision, the two-cell door on geometry, and a piece
	/// that grows a second cell would too. Counted both ways, so one answer for all could not pass.
	#[test]
	fn a_sphere_is_built_of_one_cell_walls_the_bar_can_hold() {
		use crate::shapes::{GRID, collides, object_bounds};
		let (mut yes, mut no) = (0, 0);
		for b in BlockType::ALL {
			if accepts(b) {
				assert!(
					b.is_slottable(),
					"{b:?} is accepted but the bar cannot hold it"
				);
				assert!(
					collides(make_cell(b, 0)),
					"{b:?} is a wall a body walks through"
				);
				assert!(
					!b.is_door() && !b.is_slab() && !b.is_stair(),
					"{b:?} is a worked form"
				);
				let (_, bx) = object_bounds((0, 0, 0), make_cell(b, 0));
				assert!(
					bx.is_none_or(|bx| bx.max.iter().all(|&m| m <= GRID)),
					"{b:?} wears more than one cell"
				);
				yes += 1;
			} else {
				no += 1;
			}
		}
		assert!(yes > 0 && no > 0, "the sweep saw one kind only");
		assert!(accepts(BlockType::Stone), "plain matter builds");
		assert!(accepts(BlockType::Chair), "furniture stands in one cell");
		assert!(!accepts(BlockType::LumberDoor), "a door is two cells");
		assert!(!accepts(BlockType::Torch), "a torch is walked through");
		assert!(!accepts(BlockType::Water), "the bar never holds water");
		assert!(
			!accepts(BlockType::Wardglass),
			"conjured glass is not an item"
		);
	}

	/// The plan climbs: no cell lands below one that landed before it,
	/// and the crown — the cell straight above the head at the radius —
	/// is the very last. Counted, since a one-cell plan climbs trivially.
	#[test]
	fn the_closing_climbs_to_the_crown() {
		for r in radii() {
			let writes = plan(HEAD, r, matter(), |_| Some(make_cell(BlockType::Air, 0))).unwrap();
			assert!(writes.len() > 10, "r={r}");
			for w in writes.windows(2) {
				assert!(
					w[0].0.1 <= w[1].0.1,
					"r={r}: {:?} landed before {:?}",
					w[0].0,
					w[1].0
				);
			}
			assert_eq!(
				writes.last().unwrap().0,
				(HEAD.0, HEAD.1 + r, HEAD.2),
				"r={r}"
			);
		}
	}

	/// A flooded sphere is where the closing key's ties lived: every interior cell drains, so a
	/// layer carries several cells on one bearing and a key of layer-and-bearing alone could not
	/// tell them apart. No two consecutive entries share a key, so the SET decides the sequence
	/// and both ends land the same one however their scan enumerated it. Counted, since a plan of
	/// shell cells alone would prove nothing.
	#[test]
	fn a_flooded_sphere_orders_by_a_key_that_never_ties() {
		for r in radii() {
			let writes = plan(HEAD, r, matter(), |_| Some(make_cell(BlockType::Water, 0))).unwrap();
			let (shell, interior) = sets(HEAD, r);
			assert_eq!(
				writes.len(),
				shell.len() + interior.len(),
				"r={r}: water walls the shell and drains the rest"
			);
			for w in writes.windows(2) {
				let (a, b) = (closing_key(HEAD, w[0].0), closing_key(HEAD, w[1].0));
				assert!(
					a < b,
					"r={r}: {:?} and {:?} do not order ({a:?} vs {b:?})",
					w[0].0,
					w[1].0
				);
			}
		}
	}

	/// The spell where it is wanted most: a caster within a radius of bedrock. Cells under the
	/// floor are not there to write, so the plan skips them and the cast succeeds — and the sphere
	/// is still sealed, because the floor the world clipped it against is the undiggable one.
	/// Refusing instead shut the widest rank out of the whole deep band.
	#[test]
	fn a_sphere_clipped_by_the_world_floor_still_casts() {
		use crate::block::BEDROCK_Y;
		for r in radii() {
			// Head one cell above the floor: all but a sliver of the
			// sphere hangs below the world.
			let head = (0, BEDROCK_Y + 1, 0);
			// Bedrock at the floor, air above it, nothing below — exactly
			// what a real reader answers.
			let world = |c: Cell| match c.1 {
				y if y < BEDROCK_Y => None,
				y if y == BEDROCK_Y => Some(make_cell(BlockType::Stone, 0)),
				_ => Some(make_cell(BlockType::Air, 0)),
			};
			let writes =
				plan(head, r, matter(), world).expect("the floor is not an unloaded column");
			assert!(writes.len() > 10, "r={r}: the dome is worth casting");
			assert!(
				writes.iter().all(|&(c, _)| c.1 > BEDROCK_Y),
				"r={r}: nothing is written at or under the floor"
			);
			// Sealed: the shell plus the floor stops a fill from the head.
			let (shell, _) = sets(head, r);
			let mut blocked = shell_cube(head, r, &shell);
			for x in head.0 - r..=head.0 + r {
				for z in head.2 - r..=head.2 + r {
					blocked.set((x, BEDROCK_Y, z));
				}
			}
			assert!(!escapes(head, r, &blocked), "r={r}: the dome leaks");
		}
	}

	/// The cap is a bound on the arbitrary, never on the ladder: every
	/// castable rank fits inside it, and a radius past it is clamped
	/// rather than squared into an overflow or scanned for minutes.
	#[test]
	fn the_cap_clears_every_castable_rank() {
		let widest = *radii().last().expect("the ladder has ranks");
		assert!(MAX_RADIUS >= widest, "the ladder outgrew the cap");
		let capped = sets(HEAD, MAX_RADIUS);
		assert_eq!(sets(HEAD, i32::MAX), capped, "a huge radius is the cap");
		assert_eq!(sets(HEAD, MAX_RADIUS + 1), capped);
		// A radius of nothing is the head cell alone, all shell — and a
		// negative one cannot ask for less than that.
		assert_eq!(sets(HEAD, -5), sets(HEAD, 0));
		assert_eq!(sets(HEAD, 0), (vec![HEAD], Vec::new()));
	}

	/// A closing lands every cell exactly once, in plan order, and all
	/// of them by its span — whether the plan is larger than the span
	/// or smaller, at every channel length the ladder has.
	#[test]
	fn a_closing_lands_the_plan_once_in_order() {
		let spans: Vec<u32> = {
			let mut v: Vec<u32> = (0..=100).map(unlock::channel_ticks).collect();
			v.sort_unstable();
			v.dedup();
			v
		};
		assert!(spans.len() >= 2, "the ladder quickens the channel");
		for span in spans {
			for total in [1usize, 7, span as usize - 1, span as usize, 1237] {
				let writes: Vec<(Cell, u16)> = (0..total as i32).map(|i| ((i, 0, 0), 1)).collect();
				let mut c = Closing::new(writes.clone(), span);
				let mut landed = Vec::new();
				let mut ticks = 0;
				while !c.done() {
					landed.extend(c.land());
					ticks += 1;
				}
				assert_eq!(ticks, span, "total={total}");
				assert_eq!(landed, writes, "total={total} span={span}");
				assert!(c.land().is_empty(), "nothing lands after the seal");
			}
		}
	}

	/// An unwind hands back exactly what was recorded, crown-first: the
	/// last cell written is the first restored, and a cell never
	/// recorded (declined at write time) is not in it.
	#[test]
	fn an_unwind_restores_what_landed_in_reverse() {
		let writes: Vec<(Cell, u16)> = (0..10).map(|i| ((0, i, 0), 7)).collect();
		let mut c = Closing::new(writes, 2);
		let mut recorded = Vec::new();
		for cell_val in c.land() {
			if cell_val.0.1 % 2 == 0 {
				continue; // declined
			}
			c.record(cell_val.0, cell_val.1, 3);
			recorded.push((cell_val.0, cell_val.1, 3));
		}
		assert!(recorded.len() > 1, "the half-climb recorded something");
		recorded.reverse();
		assert_eq!(c.unwind(), recorded);
	}

	/// One unloaded cell refuses the whole cast — a skipped cell would be
	/// a hole exactly where the world was not there to ask.
	#[test]
	fn an_unloaded_cell_refuses_the_whole_cast() {
		let r = radii()[0];
		let hole = (HEAD.0, HEAD.1 + r, HEAD.2);
		let world = |c: Cell| (c != hole).then_some(make_cell(BlockType::Air, 0));
		assert_eq!(plan(HEAD, r, matter(), world), Err(Refusal::Unloaded));
		assert!(plan(HEAD, r, matter(), |_| Some(make_cell(BlockType::Air, 0))).is_ok());
	}
}
