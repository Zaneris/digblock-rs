//! Sweeps standing conjured-light cells back to Air. A MIGRATION, not a rule:
//! conjured light is a timed [`crate::kindle`] emitter now and never a
//! cell, so nothing being played today can create one.
//!
//! It stays because worlds written before that change journaled the light
//! as an ordinary edit and the journal's format version did not move, so
//! those cells replay. One has no dig time and is not
//! `is_replaceable`, which makes an orphaned one permanent and
//! unremovable — the sweep is the only thing that can clear it. Once a
//! world holds none it costs a frontier pass that matches nothing.

use super::{Candidates, CellSource, Run, Schedule, Scope, WorldJob};
use crate::block::BlockType;
use crate::chunk::WORLD_VBLOCK;

/// Sweeps per game day. A migration wants to reach an orphan the first
/// time anyone walks past it, so this is far more often than the world
/// jobs that model a rule.
const PER_DAY: u32 = 8;

/// Chunk radius around each player — grass's own reach; an orphan nobody
/// is near waits for someone to come back, which is also when anyone
/// could see it go.
const RADIUS_CHUNKS: i32 = 3;

pub struct ConjuredLightFade;

impl WorldJob for ConjuredLightFade {
	fn name(&self) -> &'static str {
		"conjured-light-fade"
	}

	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: PER_DAY,
			offset: 0.0,
		}
	}

	fn scope(&self) -> Scope {
		Scope {
			blocks: &[BlockType::ConjuredLight],
			radius_chunks: RADIUS_CHUNKS,
			// The full column: a light could be kindled anywhere a player
			// could aim. Frontier candidates keep that affordable — every
			// one of these cells IS an edit, so the edit record finds
			// them all.
			y: (0, WORLD_VBLOCK),
			candidates: Candidates::Frontier {
				surface_shell: false,
			},
		}
	}

	fn apply(
		&self,
		_at: (i32, i32, i32),
		_world: &dyn CellSource,
		_seed: crate::worldgen::Seed,
		_run: Run,
	) -> Option<u16> {
		// Unconditional: a cell holding one of these is an orphan by
		// definition, so there is nothing to check.
		Some(BlockType::Air as u16)
	}
}

/// The one production registration, shared by the server and offline —
/// grass's arrangement, so the rule cannot fork.
pub fn jobs() -> [Box<dyn WorldJob>; 1] {
	[Box::new(ConjuredLightFade)]
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The sweep's scope can never touch a node reveal — the registration
	/// would panic, but this fails with a name at the rule that broke.
	#[test]
	fn the_sweep_touches_no_node_reveal() {
		for &b in ConjuredLightFade.scope().blocks {
			assert!(!super::super::node_host(b), "{b:?} hosts nodes");
			assert!(!b.is_node_block(), "{b:?} is a live face");
		}
	}

	/// The orphan the sweep exists for cannot be cleared any other way:
	/// no dig time, and nothing may be built over it. If either of those
	/// ever changes, this migration has a cheaper replacement.
	#[test]
	fn a_standing_orphan_has_no_other_way_out() {
		let b = BlockType::ConjuredLight;
		assert!(b.hardness_ticks().is_none(), "{b:?} can be dug");
		assert!(!b.is_replaceable(), "{b:?} can be built over");
	}
}
