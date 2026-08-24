//! The generation-time settle: worldgen leaves each weir's lip open, and
//! this pours the fall through it, so a cascade is water the fluid sim
//! itself placed rather than a sheet painted by the generator.
//!
//! It is `fluid::resolve` — the sim's whole rule — driven by a walk of its
//! own, and `a_settle_is_the_sim_with_no_cap` holds the two to ONE answer.
//! What the walk adds is a budget: the lateral steps water has taken since
//! the lip it left, so the reach of a cascade is bounded and the apron a
//! chunk needs to see is bounded with it. Two things make that bound a
//! proof rather than a hope. A cell at travel `t` reads its neighbours as
//! they stood before travel `t` began, so nothing at `t` can feed a cell
//! also at `t` and an influence path is never shorter than its lateral
//! length; and a cell is re-resolved whenever a neighbour changes, so the
//! walk reaches the sim's own fixpoint rather than each cell's first
//! guess. `a_settle_is_the_same_at_a_wider_apron` is the pin on both.

use std::collections::VecDeque;

use rustc_hash::{FxHashMap, FxHashSet};

use super::classify::{ColumnShape, column_block};
use super::{CHUNK_SIZE, Seed, ShapeMap, caves};
use crate::block::BlockType;
use crate::chunk::{ChunkData, WORLD_VBLOCK};
use crate::fluid::{self, FluidWorld};

/// How far, in columns, water may run from where worldgen put it. Falls
/// are free: a fall stays inside one column, whose whole stack is built
/// anyway. A dial, not a knee — most writes are the free fall off the lip
/// and the rest is cascades down stepped ground, a tail that stays flat
/// however far it is followed. Also the apron, so every chunk pays for it.
pub(super) const TRAVEL: i32 = 8;

type Pos = (i32, i32, i32);

/// The aproned slab a settle runs over: the column shapes of one chunk
/// and `apron` columns of context on every side, and the cells worldgen
/// alone derives from them.
pub(super) struct Slab {
	shapes: Vec<ColumnShape>,
	cx: i32,
	cz: i32,
	x0: i32,
	z0: i32,
	w: i32,
	d: i32,
	seed: Seed,
}

impl Slab {
	pub(super) fn new(cx: i32, cz: i32, seed: Seed, apron: i32) -> Self {
		let n = CHUNK_SIZE as i32;
		let (x0, z0) = (cx * n - apron, cz * n - apron);
		let (x1, z1) = (cx * n + n - 1 + apron, cz * n + n - 1 + apron);
		Self {
			shapes: super::column_shapes_in(x0, x1, z0, z1, seed),
			cx,
			cz,
			x0,
			z0,
			w: x1 - x0 + 1,
			d: z1 - z0 + 1,
			seed,
		}
	}

	/// The chunk's own shapes, `[z][x]` — the slice `build_column` fills
	/// from, so the field grid under the apron is evaluated once.
	pub(super) fn chunk_shapes(&self) -> ShapeMap {
		let n = CHUNK_SIZE as i32;
		std::array::from_fn(|z| {
			std::array::from_fn(|x| {
				self.shapes[self
					.at(self.cx * n + x as i32, self.cz * n + z as i32)
					.expect("chunk lies inside its slab")]
			})
		})
	}

	/// Whether a cell lies in the chunk this slab was built around — the
	/// only cells its settle may write.
	pub(super) fn in_chunk(&self, (x, _, z): Pos) -> bool {
		let n = CHUNK_SIZE as i32;
		(0..n).contains(&(x - self.cx * n)) && (0..n).contains(&(z - self.cz * n))
	}

	fn at(&self, x: i32, z: i32) -> Option<usize> {
		let (ix, iz) = (x - self.x0, z - self.z0);
		(ix >= 0 && ix < self.w && iz >= 0 && iz < self.d).then(|| (iz * self.w + ix) as usize)
	}

	/// The cell worldgen alone would put here: the column rules, then the
	/// carve, with containment's wall span spared exactly as `carve_column`
	/// spares it. Decoration has not run yet — the settle is sequenced
	/// before it so this view is complete, and so trees write into air the
	/// water did not take.
	pub(super) fn pristine(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		if !(0..=WORLD_VBLOCK).contains(&y) {
			return None;
		}
		let s = self.shapes[self.at(x, z)?];
		let b = column_block(y, s);
		let carveable = s.height > s.water
			&& y >= caves::CAVE_FLOOR
			&& b.props().carveable
			&& !(s.wall.0..=s.wall.1).contains(&y);
		if carveable && super::carved(x, y, z, self.seed) {
			Some(BlockType::Air)
		} else {
			Some(b)
		}
	}

	/// Is this column a weir — a pool one block deep with a neighbour that
	/// fails to cover it? That lip is the only cell containment leaves
	/// open, and so the only place a settle can start.
	fn weir(&self, ix: i32, iz: i32) -> bool {
		let s = self.shapes[(iz * self.w + ix) as usize];
		if s.water != s.height + 1 {
			return false;
		}
		fluid::SIDES.iter().any(|(dx, dz)| {
			let (jx, jz) = (ix + dx, iz + dz);
			jx >= 0 && jx < self.w && jz >= 0 && jz < self.d && {
				let nb = self.shapes[(jz * self.w + jx) as usize];
				nb.height.max(nb.water) < s.water
			}
		})
	}

	/// Where every weir in the slab spills: one step off each lip, in
	/// every direction it could leave.
	fn spills(&self) -> Vec<Pos> {
		let mut out = Vec::new();
		for iz in 0..self.d {
			for ix in 0..self.w {
				if !self.weir(ix, iz) {
					continue;
				}
				let s = self.shapes[(iz * self.w + ix) as usize];
				let (x, z) = (self.x0 + ix, self.z0 + iz);
				out.extend(fluid::SIDES.map(|(dx, dz)| (x + dx, s.water, z + dz)));
			}
		}
		out
	}
}

/// One settled cell. `prev` is what it held before the travel that wrote
/// `cur`, so a lateral read from that travel still sees the world as it
/// stood one step earlier.
#[derive(Clone, Copy)]
struct Cell {
	cur: BlockType,
	travel: i32,
	prev: BlockType,
}

/// What a cell at travel `t` in column `(x, z)` is allowed to see: its own
/// column as it stands, every other column as it stood before travel `t`
/// began. A lateral step costs one, so water that arrived at `t` cannot
/// feed a neighbour also at `t` — which is what keeps an influence path
/// at least as long as its lateral span, and so inside the apron.
struct View<'a> {
	slab: &'a Slab,
	cells: &'a FxHashMap<Pos, Cell>,
	t: i32,
	x: i32,
	z: i32,
}

impl FluidWorld for View<'_> {
	fn get(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
		match self.cells.get(&(x, y, z)) {
			Some(c) if c.travel == self.t && (x, z) != (self.x, self.z) => Some(c.prev),
			Some(c) => Some(c.cur),
			None => self.slab.pristine(x, y, z),
		}
	}

	fn set(&mut self, _: i32, _: i32, _: i32, _: BlockType) -> bool {
		false
	}
}

/// Pour every weir in the slab and return what changed, anywhere in it.
/// A 0-1 breadth-first walk in travel order: falling and pooling ride the
/// front of the queue at the same travel, spreading pays one and rides the
/// back, so a `View` at `t` is exactly what the earlier travels left; and
/// water only accumulates, so re-queuing on every change still terminates.
pub(super) fn relax(slab: &Slab, cap: i32) -> FxHashMap<Pos, BlockType> {
	let mut queue: VecDeque<(Pos, i32)> = VecDeque::new();
	let mut queued: FxHashSet<(Pos, i32)> = FxHashSet::default();
	for k in slab.spills() {
		if queued.insert((k, 1)) {
			queue.push_back((k, 1));
		}
	}
	let mut cells: FxHashMap<Pos, Cell> = FxHashMap::default();
	while let Some((k, t)) = queue.pop_front() {
		queued.remove(&(k, t));
		let (x, y, z) = k;
		let Some(cur) = cells
			.get(&k)
			.map(|c| c.cur)
			.or_else(|| slab.pristine(x, y, z))
		else {
			continue;
		};
		let view = View {
			slab,
			cells: &cells,
			t,
			x,
			z,
		};
		let Some(wanted) = fluid::resolve(&view, x, y, z) else {
			continue;
		};
		if wanted == cur {
			continue;
		}
		let c = cells.entry(k).or_insert(Cell {
			cur,
			travel: t,
			prev: cur,
		});
		if c.travel < t {
			c.prev = c.cur;
			c.travel = t;
		}
		c.cur = wanted;
		for (nb, cost) in [
			((x, y - 1, z), 0),
			((x, y + 1, z), 0),
			((x - 1, y, z), 1),
			((x + 1, y, z), 1),
			((x, y, z - 1), 1),
			((x, y, z + 1), 1),
		] {
			let nt = t + cost;
			if nt > cap || !queued.insert((nb, nt)) {
				continue;
			}
			if cost == 0 {
				queue.push_front((nb, nt));
			} else {
				queue.push_back((nb, nt));
			}
		}
	}
	cells.into_iter().map(|(k, c)| (k, c.cur)).collect()
}

/// Write a settle's cells into the chunk's stack. Cells outside the
/// chunk belong to their own column's settle, which computes them from
/// the same rule over its own apron.
pub(super) fn apply(stack: &mut [ChunkData], slab: &Slab, water: &FxHashMap<Pos, BlockType>) {
	let n = CHUNK_SIZE as i32;
	for (&k, &b) in water.iter().filter(|(k, _)| slab.in_chunk(**k)) {
		let (x, y, z) = k;
		let data = &mut stack[(y / n) as usize];
		data.set(
			(x - slab.cx * n) as usize,
			(y % n) as usize,
			(z - slab.cz * n) as usize,
			b,
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fluid::FluidSim;

	const SEED: Seed = Seed::current(1234.5678);
	const CHUNKS: (std::ops::Range<i32>, std::ops::Range<i32>) = (105..118, 76..88);

	fn inside(slab: &Slab, m: FxHashMap<Pos, BlockType>) -> FxHashMap<Pos, BlockType> {
		m.into_iter().filter(|(k, _)| slab.in_chunk(*k)).collect()
	}

	/// THE determinism pin. A chunk's settle must not depend on how much
	/// world it loaded: run it again with three times the apron and the
	/// cells inside the chunk must be identical. Failing here means the
	/// travel cap has stopped bounding the reach, and two neighbouring
	/// chunks would disagree about a waterfall crossing their border.
	#[test]
	fn a_settle_is_the_same_at_a_wider_apron() {
		let mut checked = 0;
		let mut with_water = 0;
		for cx in CHUNKS.0 {
			for cz in CHUNKS.1.clone() {
				let tight = Slab::new(cx, cz, SEED, TRAVEL);
				let wide = Slab::new(cx, cz, SEED, TRAVEL * 3);
				let a = inside(&tight, relax(&tight, TRAVEL));
				let b = inside(&tight, relax(&wide, TRAVEL));
				checked += 1;
				if !a.is_empty() {
					with_water += 1;
				}
				assert_eq!(
					a, b,
					"chunk ({cx},{cz}) settles differently at a wider apron"
				);
			}
		}
		// A sweep that photographed nothing proves nothing.
		assert!(checked > 100, "only {checked} chunks compared");
		assert!(with_water > 0, "no chunk in the sample settled anything");
	}

	/// The slab as the real sim sees it: pristine cells under a write map.
	struct SimSlab<'a> {
		slab: &'a Slab,
		writes: FxHashMap<Pos, BlockType>,
	}

	impl FluidWorld for SimSlab<'_> {
		fn get(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
			self.writes
				.get(&(x, y, z))
				.copied()
				.or_else(|| self.slab.pristine(x, y, z))
		}

		fn set(&mut self, x: i32, y: i32, z: i32, b: BlockType) -> bool {
			self.writes.insert((x, y, z), b);
			true
		}
	}

	/// With no cap, the walk IS the sim: `FluidSim` from the same spills,
	/// run to convergence over the same slab, must land on the same cells.
	/// This is the test that a cheaper walk fails — one that resolves each
	/// cell once at its first visit leaves cells the sim would still
	/// change, and a player's edit beside them then makes water appear.
	#[test]
	fn a_settle_is_the_sim_with_no_cap() {
		let mut compared = 0;
		for cx in CHUNKS.0 {
			for cz in CHUNKS.1.clone() {
				let slab = Slab::new(cx, cz, SEED, TRAVEL);
				let walked = relax(&slab, i32::MAX);
				if walked.is_empty() {
					continue;
				}
				let mut world = SimSlab {
					slab: &slab,
					writes: FxHashMap::default(),
				};
				let mut sim = FluidSim::new();
				for (x, y, z) in slab.spills() {
					sim.schedule(x, y, z);
				}
				while sim.step(&mut world, 1024) > 0 {}
				assert_eq!(
					walked, world.writes,
					"chunk ({cx},{cz}): the walk and the sim disagree"
				);
				compared += walked.len();
			}
		}
		assert!(compared > 1000, "only {compared} cells compared");
	}

	/// The settle only ever ADDS water: worldgen's own cells are already at
	/// the fixpoint, so anything it takes away is containment having missed
	/// a wall.
	#[test]
	fn a_settle_never_takes_water_away() {
		let mut added = 0;
		for cx in CHUNKS.0 {
			for cz in CHUNKS.1.clone() {
				let slab = Slab::new(cx, cz, SEED, TRAVEL);
				for (&(x, y, z), &b) in &relax(&slab, TRAVEL) {
					let was = slab.pristine(x, y, z).unwrap_or(BlockType::Air);
					assert!(
						!was.is_water() || b.is_water(),
						"settle removed water at ({x},{y},{z}): {was:?} -> {b:?}"
					);
					if !was.is_water() && b.is_water() {
						added += 1;
					}
				}
			}
		}
		assert!(added > 0, "the sample settled no water at all");
	}

	/// Water a neighbour's lip pours in can stand ABOVE everything this
	/// chunk's own columns reach, and the built stack must reach it too: a
	/// cell the stack has no chunk for is dropped silently, cutting the top
	/// off a fall for the sim to drain on its first touch. So every settled
	/// cell lands in a real chunk, and the sample must hold one that high.
	#[test]
	fn the_stack_reaches_every_settled_cell() {
		let n = CHUNK_SIZE as i32;
		let mut above = 0;
		for cx in -140..-130 {
			for cz in 76..84 {
				let slab = Slab::new(cx, cz, SEED, TRAVEL);
				let water = inside(&slab, relax(&slab, TRAVEL));
				if water.is_empty() {
					continue;
				}
				let terrain_top = slab
					.chunk_shapes()
					.iter()
					.flatten()
					.map(|s| s.height.max(s.water))
					.max()
					.unwrap();
				let stack = super::super::build_column(cx, cz, SEED);
				for (&(x, y, z), &b) in &water {
					let chunk = &stack[(y / n) as usize];
					assert!(
						!std::sync::Arc::ptr_eq(chunk, &crate::chunk::AIR_CHUNK),
						"chunk ({cx},{cz}) has no chunk for settled water at ({x},{y},{z})"
					);
					// Decoration may plant into a settled cell; what may not
					// happen is the cell reverting to worldgen's own answer.
					let got = chunk.get(
						(x - cx * n) as usize,
						(y % n) as usize,
						(z - cz * n) as usize,
					);
					assert!(
						got == b || got != slab.pristine(x, y, z).unwrap(),
						"settled water at ({x},{y},{z}) was lost: wanted {b:?}, built {got:?}"
					);
					if y > terrain_top {
						above += 1;
					}
				}
			}
		}
		assert!(
			above > 0,
			"no settled cell in the sample stood above its chunk's terrain"
		);
	}
}
