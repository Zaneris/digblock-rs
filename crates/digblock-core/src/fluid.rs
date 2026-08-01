//! Edit-time water flow: a bounded relaxation over the Minecraft-style
//! level rules encoded in the water block variants (block.rs). All state
//! lives in the blocks themselves — the sim owns only a work queue of
//! cells to recompute, so it costs nothing while the world is at rest.
//!
//! Rules, target-centric (each queued cell computes what it *should* be
//! from its neighbors):
//! - sources (`Water`) are terminal: infinite, never recomputed;
//! - a cell with water above it becomes `WaterFall` (full strength);
//! - otherwise it takes half its strongest side-neighbor amount as
//!   `WaterFlowN` (n ≥ 1), counting only *supported* neighbors — water
//!   resting on a colliding block — because unsupported water is busy
//!   falling;
//! - anything else relaxes to `Air`, which is how removal drains: each
//!   recompute lowers the region's total amount until it dries.
//!
//! Worldgen water follows the same physics but is emitted converged, and
//! only edits schedule cells — so oceans and river cascades stay static
//! until disturbed. One documented divergence: worldgen fall sheets are
//! fed by pool-surface water (which rests on water, not on a colliding
//! block), so a player-disturbed worldgen fall drains rather than
//! re-forming. Player-built cascades (source on solid, spilling over an
//! edge) form and persist correctly.

use std::collections::VecDeque;

use rustc_hash::FxHashSet;

use crate::block::BlockType;

/// Seconds between flow steps (~the Minecraft 5-game-tick water cadence).
pub const FLOW_STEP_SECS: f32 = 0.25;
/// Cell updates per flow step — bounds worst-case main-thread work.
pub const FLOW_BUDGET: usize = 128;
/// Queue hard cap: beyond this, newly scheduled cells are dropped (the
/// region converges anyway as its edits are consumed).
const QUEUE_CAP: usize = 64 * 1024;

/// World access the sim runs against: the engine implements it over the
/// chunk pipeline, tests over a HashMap.
pub trait FluidWorld {
	/// None = outside the world or not loaded — the sim drops such cells.
	fn get(&self, x: i32, y: i32, z: i32) -> Option<BlockType>;
	/// False if the write didn't land; the sim then doesn't propagate.
	fn set(&mut self, x: i32, y: i32, z: i32, b: BlockType) -> bool;
}

#[derive(Default)]
pub struct FluidSim {
	queue: VecDeque<(i32, i32, i32)>,
	queued: FxHashSet<(i32, i32, i32)>,
}

const SIDES: [(i32, i32); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];

/// Horizontal spread falloff: each step takes half the feeding amount
/// (a source runs 8 → 4 → 2 → 1 → stop), much steeper than
/// Minecraft's m − 1 — water reaches 3 blocks instead of 7 and pools
/// hug their source. Strictly decreasing, so relaxation converges.
fn falloff(m: u8) -> u8 {
	m / 2
}

impl FluidSim {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn pending(&self) -> usize {
		self.queue.len()
	}

	/// Queue one cell for recomputation (deduplicated, capped).
	pub fn schedule(&mut self, x: i32, y: i32, z: i32) {
		if self.queue.len() < QUEUE_CAP && self.queued.insert((x, y, z)) {
			self.queue.push_back((x, y, z));
		}
	}

	/// Queue a cell and its six neighbors — what an edit at (x, y, z)
	/// can immediately affect.
	pub fn schedule_around(&mut self, x: i32, y: i32, z: i32) {
		self.schedule(x, y, z);
		for (dx, dz) in SIDES {
			self.schedule(x + dx, y, z + dz);
		}
		self.schedule(x, y - 1, z);
		self.schedule(x, y + 1, z);
	}

	/// Run up to `budget` cell recomputations; returns how many ran.
	/// FIFO order keeps the relaxation breadth-first and deterministic.
	pub fn step(&mut self, world: &mut impl FluidWorld, budget: usize) -> usize {
		let mut done = 0;
		while done < budget {
			let Some((x, y, z)) = self.queue.pop_front() else {
				break;
			};
			self.queued.remove(&(x, y, z));
			done += 1;
			self.update(world, x, y, z);
		}
		done
	}

	fn update(&mut self, world: &mut impl FluidWorld, x: i32, y: i32, z: i32) {
		let Some(cur) = world.get(x, y, z) else {
			return; // unloaded: drop, the region will re-schedule on edit
		};
		// Only empty and in-transit water cells recompute; sources are
		// infinite by definition and solids are not fluid business.
		if cur == BlockType::Water || cur.props().collides {
			return;
		}
		let wanted = if world.get(x, y + 1, z).is_some_and(|b| b.is_water()) {
			BlockType::WaterFall
		} else {
			let mut m = 0u8;
			for (dx, dz) in SIDES {
				let (nx, nz) = (x + dx, z + dz);
				let Some(a) = world.get(nx, y, nz).and_then(BlockType::water_amount) else {
					continue;
				};
				if a > m && world.get(nx, y - 1, nz).is_some_and(|b| b.props().collides) {
					m = a;
				}
			}
			let next = falloff(m);
			if next > 0 {
				BlockType::water_flow(next)
			} else {
				BlockType::Air
			}
		};
		if wanted != cur && world.set(x, y, z, wanted) {
			for (dx, dz) in SIDES {
				self.schedule(x + dx, y, z + dz);
			}
			self.schedule(x, y - 1, z);
			self.schedule(x, y + 1, z);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rustc_hash::FxHashMap;

	/// Flat stone floor at y = 0 inside |x|,|z| ≤ 24, air above up to
	/// y = 24, overridable per cell; outside → None (unloaded).
	struct MapWorld {
		cells: FxHashMap<(i32, i32, i32), BlockType>,
	}

	impl MapWorld {
		fn new() -> Self {
			Self {
				cells: FxHashMap::default(),
			}
		}

		fn water_cells(&self) -> Vec<((i32, i32, i32), u8)> {
			let mut v: Vec<_> = self
				.cells
				.iter()
				.filter(|(_, b)| b.is_water())
				.map(|(&p, &b)| (p, b as u8))
				.collect();
			v.sort();
			v
		}
	}

	impl FluidWorld for MapWorld {
		fn get(&self, x: i32, y: i32, z: i32) -> Option<BlockType> {
			if x.abs() > 24 || z.abs() > 24 || !(0..=24).contains(&y) {
				return None;
			}
			Some(*self.cells.get(&(x, y, z)).unwrap_or(if y == 0 {
				&BlockType::Stone
			} else {
				&BlockType::Air
			}))
		}

		fn set(&mut self, x: i32, y: i32, z: i32, b: BlockType) -> bool {
			if self.get(x, y, z).is_none() {
				return false;
			}
			self.cells.insert((x, y, z), b);
			true
		}
	}

	fn settle(sim: &mut FluidSim, world: &mut MapWorld) {
		for _ in 0..10_000 {
			if sim.step(world, 512) == 0 {
				return;
			}
		}
		panic!("sim never converged");
	}

	#[test]
	fn source_spreads_to_a_plus_shaped_pool() {
		let mut w = MapWorld::new();
		let mut sim = FluidSim::new();
		w.set(0, 1, 0, BlockType::Water);
		sim.schedule_around(0, 1, 0);
		settle(&mut sim, &mut w);
		// Amount halves with each Manhattan step: 8 at the source, then
		// 4 → 2 → 1, nothing beyond distance 3.
		for (dx, dz, expect) in [
			(1, 0, Some(4u8)),
			(0, -2, Some(2)),
			(3, 0, Some(1)),
			(2, 1, Some(1)),
			(4, 0, None),
			(2, 2, None),
		] {
			let b = w.get(dx, 1, dz).unwrap();
			assert_eq!(b.water_amount().filter(|_| b != BlockType::Water), expect);
		}
		// 1 source + the full Manhattan ball of radius 3.
		assert_eq!(
			w.water_cells().len(),
			1 + (1..=3).map(|d| 4 * d).sum::<usize>()
		);
	}

	#[test]
	fn removing_the_source_drains_everything() {
		let mut w = MapWorld::new();
		let mut sim = FluidSim::new();
		w.set(0, 1, 0, BlockType::Water);
		sim.schedule_around(0, 1, 0);
		settle(&mut sim, &mut w);
		w.set(0, 1, 0, BlockType::Air);
		sim.schedule_around(0, 1, 0);
		settle(&mut sim, &mut w);
		assert_eq!(w.water_cells(), vec![], "fully drained");
	}

	#[test]
	fn water_falls_into_a_pit_and_spreads_on_its_floor() {
		let mut w = MapWorld::new();
		let mut sim = FluidSim::new();
		// A 1-wide shaft at (3, 0, 0): the floor opens, stone at the
		// bottom is the world edge (y < 0 → None, never solid), so give
		// the pit its own floor by keeping y = 0 solid around it and
		// dropping the source next to the hole at surface level.
		w.set(3, 0, 0, BlockType::Air); // hole in the floor... bottomless
		w.set(0, 1, 0, BlockType::Water);
		sim.schedule_around(0, 1, 0);
		settle(&mut sim, &mut w);
		// Flow reaches the hole and pours in.
		assert!(w.get(3, 1, 0).unwrap().is_water(), "flow crosses the hole");
		assert_eq!(
			w.get(3, 0, 0).unwrap(),
			BlockType::WaterFall,
			"water falls through the opened floor"
		);
	}

	#[test]
	fn landed_fall_spreads_at_full_strength() {
		let mut w = MapWorld::new();
		let mut sim = FluidSim::new();
		// Floating source high up: falls straight down, lands on the
		// floor, then spreads a full radius-3 pool at y = 1 (falls are
		// full strength, so the landing restarts the halving ladder).
		w.set(0, 6, 0, BlockType::Water);
		sim.schedule_around(0, 6, 0);
		settle(&mut sim, &mut w);
		for y in 1..6 {
			assert_eq!(w.get(0, y, 0).unwrap(), BlockType::WaterFall, "y={y}");
		}
		assert_eq!(w.get(1, 1, 0).unwrap().water_amount(), Some(4));
		assert_eq!(w.get(3, 1, 0).unwrap().water_amount(), Some(1));
		assert_eq!(w.get(4, 1, 0).unwrap().water_amount(), None);
	}

	#[test]
	fn budget_bounds_each_step() {
		let mut w = MapWorld::new();
		let mut sim = FluidSim::new();
		w.set(0, 1, 0, BlockType::Water);
		sim.schedule_around(0, 1, 0);
		assert!(sim.step(&mut w, 3) <= 3);
		assert!(sim.pending() > 0, "remaining work stays queued");
	}

	#[test]
	fn relaxation_is_deterministic() {
		let run = || {
			let mut w = MapWorld::new();
			let mut sim = FluidSim::new();
			w.set(0, 1, 0, BlockType::Water);
			w.set(4, 1, 2, BlockType::Water);
			w.set(2, 0, 1, BlockType::Air); // a hole
			sim.schedule_around(0, 1, 0);
			sim.schedule_around(4, 1, 2);
			settle(&mut sim, &mut w);
			w.water_cells()
		};
		assert_eq!(run(), run());
	}
}
