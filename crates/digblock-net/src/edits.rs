//! Predicted block edits awaiting server confirmation.
//!
//! Precedence is structural, not timing-based: every authoritative edit
//! broadcast first `confirm`s our own seq (if it is ours), then
//! `on_authoritative(cell)` drops any other pending entries on that cell,
//! then the broadcast value is applied. A rejection arriving after an
//! authoritative write to the same cell finds no entry and reverts
//! nothing — authority always wins.

use digblock_core::block::BlockType;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub struct PendingEdit {
	pub seq: u32,
	pub cell: (i32, i32, i32),
	/// What we overwrote locally — the revert target.
	pub prev: BlockType,
	/// What we wrote (Air for dig).
	pub placed: BlockType,
	/// Predicted tick when applied.
	pub tick: u32,
}

#[derive(Default)]
pub struct PendingEdits {
	next_seq: u32,
	pending: VecDeque<PendingEdit>,
}

impl PendingEdits {
	/// Record a locally-applied predicted edit; returns its seq.
	pub fn begin(
		&mut self,
		cell: (i32, i32, i32),
		prev: BlockType,
		placed: BlockType,
		tick: u32,
	) -> u32 {
		let seq = self.next_seq;
		self.next_seq += 1;
		self.pending.push_back(PendingEdit {
			seq,
			cell,
			prev,
			placed,
			tick,
		});
		seq
	}

	/// Server confirmed our edit: drop the entry.
	pub fn confirm(&mut self, seq: u32) {
		self.pending.retain(|e| e.seq != seq);
	}

	/// Server rejected our edit. `Some(edit)` → caller reverts the cell
	/// to `edit.prev`; `None` → an authoritative write already owns that
	/// cell, do not touch it.
	pub fn reject(&mut self, seq: u32) -> Option<PendingEdit> {
		let idx = self.pending.iter().position(|e| e.seq == seq)?;
		self.pending.remove(idx)
	}

	/// An authoritative edit landed on `cell`: all pending entries there
	/// are superseded.
	pub fn on_authoritative(&mut self, cell: (i32, i32, i32)) {
		self.pending.retain(|e| e.cell != cell);
	}

	pub fn len(&self) -> usize {
		self.pending.len()
	}

	pub fn is_empty(&self) -> bool {
		self.pending.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const CELL: (i32, i32, i32) = (5, 130, -2);

	#[test]
	fn reject_reverts_confirm_drops() {
		let mut p = PendingEdits::default();
		let a = p.begin(CELL, BlockType::Stone, BlockType::Air, 100);
		let b = p.begin((6, 130, -2), BlockType::Air, BlockType::Dirt, 101);
		p.confirm(a);
		assert_eq!(p.len(), 1);
		let e = p.reject(b).expect("pending entry returns for revert");
		assert_eq!(e.cell, (6, 130, -2));
		assert_eq!(e.prev, BlockType::Air);
		assert!(p.is_empty());
		// Double reject: gone.
		assert!(p.reject(b).is_none());
	}

	#[test]
	fn authoritative_beats_pending() {
		let mut p = PendingEdits::default();
		let seq = p.begin(CELL, BlockType::Stone, BlockType::Air, 100);
		// Another player's edit lands on the same cell first.
		p.on_authoritative(CELL);
		// Our rejection arrives late: no revert allowed.
		assert!(p.reject(seq).is_none());
	}
}
