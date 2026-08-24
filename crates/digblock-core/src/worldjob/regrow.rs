//! A drawn node may come back. Once a day every spent face still standing
//! where its find was rolls a chance to become the live node again — and
//! the odds rise with how busy the world has been, so a shared server's
//! nodes return faster than a solo world's.
//!
//! The write is the live face itself, stored: the anti-dupe's edited bit
//! is monotone, so no write of the HOST could ever make `nodes::revealed`
//! see the find again. That is the one stored node in the game, and it is
//! safe for the same reason the rest are derived — no live face is an
//! item, so only this sweep can ever put one in a cell.

use super::{Candidates, CellSource, Run, Schedule, Scope, WorldJob};
use crate::block::BlockType;
use crate::chunk::{WORLD_VBLOCK, cell_id};
use crate::worldgen::Seed;
use crate::worldgen::nodes::{NODES, drawn_kind, host_block, node_at, spent_block};
use crate::worldgen::ore;

/// The hour the sweep comes due, on the game's 24-hour face — evening,
/// so a day's draws have the day to stand spent before any return.
const HOUR: u32 = 21;

/// Chunk radius around each player — grass's reach, within typical
/// residency: a face nobody is near waits for someone to come back,
/// which is also when anyone could see it return.
const RADIUS_CHUNKS: i32 = 3;

/// Chance per spent face per run, in percent, for each player the world
/// held at its busiest over the past game day. One player is the solo
/// rate; a server's rate is this times its peak head-count.
const PERCENT_PER_PLAYER: u32 = 5;

/// Where the head-count bonus stops climbing. A full server's nodes
/// return within a couple of days; they never return every night.
const MAX_PERCENT: u32 = 50;

/// The roll's denominator — [`chance_percent`] answers in percent, so a
/// face comes back when a draw from this many falls under it.
const ROLL_OF: u64 = 100;
const _: () = assert!(
	(MAX_PERCENT as u64) < ROLL_OF,
	"a busy world would regrow every face every night"
);

const REGROW_SALT: u64 = 0x7265_6772_6f77; // "regrow"

/// The regrowth odds for a world whose day peaked at `players` online:
/// linear in the head-count, capped at [`MAX_PERCENT`].
pub fn chance_percent(players: u32) -> u32 {
	players.saturating_mul(PERCENT_PER_PLAYER).min(MAX_PERCENT)
}

/// Every spent face — the sweep's whole scope. Derived from the node
/// roster so a new kind is covered by its own `spent_block` row.
fn spent_faces() -> &'static [BlockType] {
	static FACES: std::sync::OnceLock<Vec<BlockType>> = std::sync::OnceLock::new();
	FACES.get_or_init(|| NODES.iter().filter_map(|&n| spent_block(n)).collect())
}

pub struct NodeRegrow;

impl WorldJob for NodeRegrow {
	fn name(&self) -> &'static str {
		"node-regrow"
	}

	fn schedule(&self) -> Schedule {
		Schedule::daily_at(HOUR)
	}

	fn rolled(&self) -> bool {
		true
	}

	fn scope(&self) -> Scope {
		Scope {
			blocks: spent_faces(),
			radius_chunks: RADIUS_CHUNKS,
			// The full column: a spent face stands wherever a node did,
			// from the deep seams to a lake bed. Every one IS an edit, so
			// the frontier finds them all.
			y: (0, WORLD_VBLOCK),
			candidates: Candidates::Frontier {
				surface_shell: false,
			},
		}
	}

	fn apply(
		&self,
		at: (i32, i32, i32),
		world: &dyn CellSource,
		seed: Seed,
		run: Run,
	) -> Option<u16> {
		let cell = world.cell_at(at.0, at.1, at.2)?;
		let node = drawn_kind(BlockType::try_from_id(cell_id(cell))?)?;
		// Only where the find WAS: a spent face is an item, and one a
		// player carried home and placed stands nowhere worldgen put a
		// node of its kind. Asked through the face's OWN host, the way
		// `revealed` asks — position alone answers for a heart in a cell
		// the trunk filled with leaves, which is a node site conjured out
		// of one carried item.
		if node_at(host_block(node)?, at, seed) != Some(node) {
			return None;
		}
		let roll = ore::mix(
			at.0,
			at.1,
			at.2,
			seed.value.to_bits(),
			REGROW_SALT ^ run.slot,
		) % ROLL_OF;
		(roll < u64::from(chance_percent(run.players))).then_some(node as u16)
	}
}

/// The one production registration, shared by the server and offline.
pub fn jobs() -> [Box<dyn WorldJob>; 1] {
	[Box::new(NodeRegrow)]
}

#[cfg(test)]
mod tests;
