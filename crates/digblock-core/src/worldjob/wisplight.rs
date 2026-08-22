//! Conjured light fades. Every standing Wisplight sweeps back to Air on
//! a fraction-of-a-day cadence, which is the WHOLE of the spell's
//! expiry: a kindled light lives "until the next sweep", up to one
//! cadence, and because both arms run the same scheduler a quit-and-
//! reload can never leave a permanent free lamp — the next session's
//! sweep clears it. No per-cell age (that would need state the journal
//! deliberately does not carry); the cadence IS the lifetime.

use super::{Candidates, CellSource, Run, Schedule, Scope, WorldJob};
use crate::block::BlockType;
use crate::chunk::WORLD_VBLOCK;

/// Sweeps per game day. Each is what puts every kindled light out, so
/// this is the knob a longer or shorter conjuration is tuned by.
const PER_DAY: u32 = 8;

/// Chunk radius around each player — grass's own reach; a light kindled
/// and walked away from waits for someone to come near again, which is
/// also when anyone could see it fade.
const RADIUS_CHUNKS: i32 = 3;

pub struct WisplightFade;

impl WorldJob for WisplightFade {
	fn name(&self) -> &'static str {
		"wisplight-fade"
	}

	fn schedule(&self) -> Schedule {
		Schedule {
			per_day: PER_DAY,
			offset: 0.0,
		}
	}

	fn scope(&self) -> Scope {
		Scope {
			blocks: &[BlockType::Wisplight],
			radius_chunks: RADIUS_CHUNKS,
			// The full column: a light can be kindled anywhere a player
			// can aim. Frontier candidates keep that affordable — every
			// Wisplight IS an edit, so the edit record finds them all.
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
		_seed: f64,
		_run: Run,
	) -> Option<u16> {
		// Unconditional: a standing conjured light has nothing to check —
		// it fades because the sweep came.
		Some(BlockType::Air as u16)
	}
}

/// The one production registration, shared by the server and offline —
/// grass's arrangement, so the rule cannot fork.
pub fn jobs() -> [Box<dyn WorldJob>; 1] {
	[Box::new(WisplightFade)]
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The sweep's scope can never touch a node host — the registration
	/// would panic, but this fails with a name at the rule that broke.
	#[test]
	fn the_wisplight_sweep_touches_no_node_host() {
		for &b in WisplightFade.scope().blocks {
			assert!(!super::super::node_host(b), "{b:?} hosts nodes");
		}
	}
}
