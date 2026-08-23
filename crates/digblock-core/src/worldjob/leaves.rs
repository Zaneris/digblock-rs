//! Leaves rot away once nothing holds them to a tree.
//!
//! A leaf is anchored by a trunk block within [`MAX_STEPS`] through
//! connected foliage, or by any leaf of that walk resting on solid
//! ground — which is what spares bushes, whose worldgen hangs foliage
//! with no wood at all. Everything else slowly, raggedly turns to air.
//!
//! The frontier candidate source is the propagation engine: a felled
//! trunk's edits put only the adjacent ring of leaves under judgment,
//! and each rotted ring is itself an edit that recruits the next — so a
//! canopy melts outward from the cut over several runs, and no sweep
//! ever walks the world's standing foliage.

use std::collections::VecDeque;

use rustc_hash::FxHashSet;

use super::{Candidates, CellSource, Run, Schedule, Scope, WorldJob};
use crate::block::{BlockType, RenderClass};
use crate::chunk::{WORLD_VBLOCK, cell_id};
use crate::dir::{DIRS, Dir};
use crate::worldgen;

/// Sweep cadence. Far denser than the grass spreads, because decay is
/// the answer to a player's own cut — a canopy still standing hours
/// later reads as a bug, not as slowness.
const SWEEPS_PER_DAY: u32 = 48;

/// Chunk radius around each player a sweep covers — the grass sweeps'
/// choice, for the same reason: within typical residency.
const RADIUS_CHUNKS: i32 = 3;

/// The longest walk through connected foliage that still counts as attached, in leaf cells
/// including the starting one. One past the farthest leaf worldgen ever hangs on a trunk, with the
/// spare step absorbing terrain that juts into a canopy. `every_pristine_structure_leaf_survives`
/// walks real worldgen to keep this ahead of the structures.
const MAX_STEPS: usize = 8;

/// Chance a judged orphan rots this run, per mille. Under one on
/// purpose, so a ring thins raggedly over a couple of runs instead of
/// vanishing as a slab.
const DECAY_PERMILLE: u64 = 500;

const DECAY_SALT: u64 = 0x6c65_6166; // "leaf"

/// The blocks a canopy reads as its tree: the trunk, and the heartwood
/// faces standing in it. A drained face is a real edit in the middle of
/// a trunk, and a regrown live one is too — both must keep holding the
/// canopy.
fn trunk(b: BlockType) -> bool {
	matches!(
		b,
		BlockType::Wood | BlockType::Heartwood | BlockType::DrainedHeartwood
	)
}

/// What a resting leaf reads as ground: a full opaque block that is not
/// itself foliage — a floating pile of leaves cannot hold itself up.
fn ground(b: BlockType) -> bool {
	!b.is_foliage() && b.props().class == RenderClass::Opaque
}

/// Whether the walk through connected foliage from `at`, capped at
/// [`MAX_STEPS`] leaves, meets an anchor. `None` when it touches an
/// unloaded cell — unjudgeable, retried on a later run, never guessed.
fn anchored(world: &dyn CellSource, at: (i32, i32, i32)) -> Option<bool> {
	let mut seen = FxHashSet::default();
	let mut queue = VecDeque::new();
	seen.insert(at);
	queue.push_back((at, 1usize));
	while let Some(((x, y, z), steps)) = queue.pop_front() {
		for d in DIRS {
			let (dx, dy, dz) = d.offset();
			let n = (x + dx, y + dy, z + dz);
			let b = BlockType::from_id(cell_id(world.cell_at(n.0, n.1, n.2)?));
			if trunk(b) || (matches!(d, Dir::Bottom) && ground(b)) {
				return Some(true);
			}
			if b.is_foliage() && steps < MAX_STEPS && seen.insert(n) {
				queue.push_back((n, steps + 1));
			}
		}
	}
	Some(false)
}

pub struct LeafDecay;

/// The production registration, mirrored by both arms.
pub fn jobs() -> [Box<dyn WorldJob>; 1] {
	[Box::new(LeafDecay)]
}

impl WorldJob for LeafDecay {
	fn name(&self) -> &'static str {
		"leaf-decay"
	}

	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: SWEEPS_PER_DAY,
			offset: 0.0,
		}
	}

	fn scope(&self) -> Scope {
		Scope {
			blocks: &[BlockType::Leaves, BlockType::PineLeaves],
			radius_chunks: RADIUS_CHUNKS,
			y: (0, WORLD_VBLOCK),
			candidates: Candidates::Frontier {
				// Pristine terrain cannot orphan a leaf — decoration only
				// grows anchored canopies — so the edit record is complete.
				surface_shell: false,
			},
		}
	}

	fn apply(
		&self,
		(x, y, z): (i32, i32, i32),
		world: &dyn CellSource,
		seed: f64,
		run: Run,
	) -> Option<u16> {
		// `?` folds "unjudgeable" into "leave it alone".
		if anchored(world, (x, y, z))? {
			return None;
		}
		let bits = worldgen::ore::mix(x, y, z, seed.to_bits(), DECAY_SALT ^ run.slot);
		(bits % 1000 < DECAY_PERMILLE).then_some(BlockType::Air as u16)
	}
}

#[cfg(test)]
mod tests;
