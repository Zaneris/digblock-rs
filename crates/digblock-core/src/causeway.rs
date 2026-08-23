//! The Causeway: a short trail of conjured glass laid under the caster's
//! feet the moment they cross into air, and taken up behind them once
//! [`TRAIL`] planks stand. The glass is [`BlockType::Wardglass`], the
//! Ward's own `conjured` row, so a plank digs to nothing and the road is
//! never a faucet of panes.
//!
//! Unlike the Ward, the caster's client NAMES each cell rather than both
//! ends deriving it: a plank is needed the tick the feet leave support,
//! and a round trip is several ticks. What makes that assertion safe is
//! the trail's fixed length — every plank laid retires another, so a
//! crafted client's standing footprint is [`TRAIL`] cells whatever it
//! claims. The server bounds the claim ([`claim_ok`]) rather than
//! re-deriving it.

use crate::block::BlockType;
use crate::chunk::cell_id;
use glam::Vec3;

pub use crate::ward::Cell;

/// Planks a causeway holds up at once. The head retires the tail, so
/// this is also the whole of what a caster can have standing — the
/// bound that lets the client name its own cells.
pub const TRAIL: usize = 5;

/// How far, in cells on any axis, a claimed plank may sit from the one
/// the server's own copy of the player would lay. It covers the two
/// honest sources of disagreement: the adoption envelope
/// (`POS_TOLERANCE`) lets the two positions differ by under a cell, and
/// the claim describes a tick already a round trip old.
pub const CLAIM_SLACK: i32 = 2;

/// What a plank may claim: air and standing water, exactly the Ward
/// shell's rule, so the two glass spells answer one question. Water
/// included is what lets a causeway cross a lake.
pub fn claims(cell: u16) -> bool {
	let b = BlockType::from_id(cell_id(cell));
	b == BlockType::Air || b.is_water()
}

/// The cell a plank goes in for an eye here: the highest one whose TOP
/// lies at or below the feet, so the plank meets the feet rather than
/// closing around them. Asked only of an unsupported player
/// ([`Trail::wants`]) — a foot resting mid-cell on a slab is standing on
/// something already, and this would name the cell beneath it.
pub fn step_cell(pos: Vec3) -> Cell {
	let feet = pos.y - crate::player::EYE_TO_FEET;
	(
		pos.x.floor() as i32,
		(feet - 1.0).floor() as i32,
		pos.z.floor() as i32,
	)
}

/// Whether a claimed plank is close enough to the one this position
/// would lay. The server's whole geometric check: it never re-derives
/// the claim, because the tick that produced it is gone.
pub fn claim_ok(claim: Cell, pos: Vec3) -> bool {
	let own = step_cell(pos);
	(claim.0 - own.0).abs() <= CLAIM_SLACK
		&& (claim.1 - own.1).abs() <= CLAIM_SLACK
		&& (claim.2 - own.2).abs() <= CLAIM_SLACK
}

/// The planks one caster holds up, oldest first.
#[derive(Clone, Debug, Default)]
pub struct Trail {
	cells: std::collections::VecDeque<Cell>,
}

impl Trail {
	pub fn is_empty(&self) -> bool {
		self.cells.is_empty()
	}

	pub fn len(&self) -> usize {
		self.cells.len()
	}

	pub fn holds(&self, c: Cell) -> bool {
		self.cells.contains(&c)
	}

	pub fn cells(&self) -> impl Iterator<Item = Cell> + '_ {
		self.cells.iter().copied()
	}

	/// The plank this player needs right now, or `None`. Grounded is the
	/// whole gate — a supported player needs nothing, which is why
	/// standing still spends no planks and no velocity or facing enters
	/// the answer for the two ends to disagree about. A cell the trail
	/// already holds is not needed twice.
	pub fn wants(
		&self,
		pos: Vec3,
		grounded: bool,
		cell_at: &dyn Fn(i32, i32, i32) -> Option<u16>,
	) -> Option<Cell> {
		if grounded {
			return None;
		}
		let c = step_cell(pos);
		if self.holds(c) {
			return None;
		}
		cell_at(c.0, c.1, c.2).is_some_and(claims).then_some(c)
	}

	/// Lay a plank, returning the one retired to make room. Only cells
	/// this actually wrote ever enter the trail, which is what stops a
	/// causeway taking up a pane it merely walked over — a Ward's shell
	/// is not air, so [`Trail::wants`] never names one.
	pub fn lay(&mut self, c: Cell) -> Option<Cell> {
		if self.holds(c) {
			return None;
		}
		self.cells.push_back(c);
		(self.cells.len() > TRAIL)
			.then(|| self.cells.pop_front())
			.flatten()
	}

	/// Take the whole road up — expiry, an early end, or the caster
	/// leaving. Oldest first, the order they were laid in.
	pub fn take(&mut self) -> Vec<Cell> {
		self.cells.drain(..).collect()
	}
}

#[cfg(test)]
mod tests;
