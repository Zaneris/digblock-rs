//! Edit-time water flow: a bounded relaxation over the level rules in the
//! water block variants. All state lives in the blocks themselves — the sim
//! owns only a work queue, so a world at rest costs nothing, and each queued
//! cell recomputes what it should be from its neighbours.
//!
//! Two rules read as bugs and are not: regen admits Air as well as flow, or a
//! deep-water hole never heals; and water over air or water never spreads
//! sideways, sources included, so a bucket emptied against a wall pours down
//! it and only the landing spreads. That second rule is why a pool deeper than
//! one block cannot pour at all, and so why worldgen ends a stepped pool in a
//! one-block WEIR — see `worldgen::classify::contain`, which owes this module
//! a world already at its fixpoint, since only edits ever schedule a cell.

use std::collections::VecDeque;

use rustc_hash::FxHashSet;

use crate::block::BlockType;

/// Seconds between flow steps (the Minecraft water cadence, kept in
/// seconds rather than game ticks).
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

/// The four lateral steps water spreads along, in the order the rule
/// reads them.
pub const SIDES: [(i32, i32); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];

/// Horizontal spread falloff: each step HALVES the feeding amount, much
/// steeper than Minecraft's m − 1 — water runs a few blocks rather than
/// most of a chunk, and pools hug their source. Strictly decreasing, so
/// relaxation converges.
fn falloff(m: u8) -> u8 {
	m / 2
}

/// The regen rule (see the module doc). Checked before the fall rule:
/// a qualifying cell becomes a terminal source even with water above,
/// which keeps the relaxation monotone — a cell never oscillates
/// source → fall. The floor check stops sources materializing in
/// mid-air over a fall's edge.
fn regenerates(world: &impl FluidWorld, x: i32, y: i32, z: i32, cur: BlockType) -> bool {
	let eligible =
		cur == BlockType::Air || matches!(cur.props().fluid, Some(f) if !f.source && !f.falling);
	if !eligible || !world.get(x, y - 1, z).is_some_and(BlockType::is_whole) {
		return false;
	}
	SIDES
		.iter()
		.filter(|&&(dx, dz)| {
			world
				.get(x + dx, y, z + dz)
				.is_some_and(BlockType::is_fluid_source)
		})
		.count()
		>= 2
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
		let (Some(cur), Some(wanted)) = (world.get(x, y, z), resolve(world, x, y, z)) else {
			return;
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

/// What this cell should hold, from its neighbours alone — the whole flow
/// rule with none of the scheduling. `None` where the cell is not fluid
/// business: unloaded, a solid, or a source (infinite by definition).
///
/// Split out because worldgen's settle drives the same rule over a queue
/// that carries a travel budget instead of this module's. A generated
/// cascade and a dug one must obey ONE rule, and a second statement of it
/// is the thing that would drift.
pub fn resolve(world: &impl FluidWorld, x: i32, y: i32, z: i32) -> Option<BlockType> {
	let cur = world.get(x, y, z)?;
	if cur.is_fluid_source() || cur.props().solid {
		return None;
	}
	Some(if regenerates(world, x, y, z, cur) {
		BlockType::Water
	} else if world.get(x, y + 1, z).is_some_and(|b| b.is_water()) {
		BlockType::WaterFall
	} else {
		let mut m = 0u8;
		for (dx, dz) in SIDES {
			let (nx, nz) = (x + dx, z + dz);
			let Some(a) = world.get(nx, y, nz).and_then(BlockType::water_amount) else {
				continue;
			};
			if a > m && world.get(nx, y - 1, nz).is_some_and(|b| b.props().solid) {
				m = a;
			}
		}
		let next = falloff(m);
		if next > 0 {
			BlockType::water_flow(next)
		} else {
			BlockType::Air
		}
	})
}

#[cfg(test)]
mod tests;
