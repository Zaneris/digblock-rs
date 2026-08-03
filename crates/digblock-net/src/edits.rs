//! Predicted block edits awaiting server confirmation.
//!
//! Precedence is structural, not timing-based: every authoritative edit
//! broadcast first `confirm`s our own seq (if it is ours), then
//! `on_authoritative(cell)` drops any other pending entries on that cell,
//! then the broadcast value is applied. A rejection arriving after an
//! authoritative write to the same cell finds no entry and reverts
//! nothing — authority always wins.
//!
//! Each entry also carries the inventory delta predicted with it (dig
//! credit / place debit). The cell and the delta revert on different
//! events: a rejection reverts both, but a superseding authoritative
//! write owns the CELL while the server never applied our INVENTORY
//! change — so `on_authoritative` returns the dropped entries and the
//! caller reverts only their deltas.

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
	/// Predicted inventory delta applied locally with this edit:
	/// `(block, +1)` for a dig credit, `(block, -1)` for a place debit.
	pub inv_delta: Option<(BlockType, i32)>,
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
		inv_delta: Option<(BlockType, i32)>,
	) -> u32 {
		let seq = self.next_seq;
		self.next_seq += 1;
		self.pending.push_back(PendingEdit {
			seq,
			cell,
			prev,
			placed,
			tick,
			inv_delta,
		});
		seq
	}

	/// Server confirmed our edit: drop the entry (the predicted cell and
	/// inventory delta both stand).
	pub fn confirm(&mut self, seq: u32) {
		self.pending.retain(|e| e.seq != seq);
	}

	/// Server rejected our edit. `Some(edit)` → caller reverts the cell
	/// to `edit.prev` AND reverts `edit.inv_delta`; `None` → an
	/// authoritative write already owns that cell, do not touch it (its
	/// delta was already reverted when it was superseded).
	pub fn reject(&mut self, seq: u32) -> Option<PendingEdit> {
		let idx = self.pending.iter().position(|e| e.seq == seq)?;
		self.pending.remove(idx)
	}

	/// An authoritative edit landed on `cell`: all pending entries there
	/// are superseded. Returns them so the caller reverts their
	/// `inv_delta`s — never their cells.
	pub fn on_authoritative(&mut self, cell: (i32, i32, i32)) -> Vec<PendingEdit> {
		let mut dropped = Vec::new();
		self.pending.retain(|e| {
			if e.cell == cell {
				dropped.push(*e);
				false
			} else {
				true
			}
		});
		dropped
	}

	/// All in-flight predictions — the `InventoryFull` re-base sums their
	/// deltas on top of the snapshot.
	pub fn iter(&self) -> impl Iterator<Item = &PendingEdit> {
		self.pending.iter()
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
		let a = p.begin(CELL, BlockType::Stone, BlockType::Air, 100, None);
		let b = p.begin(
			(6, 130, -2),
			BlockType::Air,
			BlockType::Dirt,
			101,
			Some((BlockType::Dirt, -1)),
		);
		p.confirm(a);
		assert_eq!(p.len(), 1);
		let e = p.reject(b).expect("pending entry returns for revert");
		assert_eq!(e.cell, (6, 130, -2));
		assert_eq!(e.prev, BlockType::Air);
		assert_eq!(e.inv_delta, Some((BlockType::Dirt, -1)));
		assert!(p.is_empty());
		// Double reject: gone.
		assert!(p.reject(b).is_none());
	}

	#[test]
	fn authoritative_beats_pending() {
		let mut p = PendingEdits::default();
		let seq = p.begin(CELL, BlockType::Stone, BlockType::Air, 100, None);
		// Another player's edit lands on the same cell first.
		p.on_authoritative(CELL);
		// Our rejection arrives late: no revert allowed.
		assert!(p.reject(seq).is_none());
	}

	#[test]
	fn authoritative_supersede_returns_entries_for_inventory_revert() {
		let mut p = PendingEdits::default();
		let dig = Some((BlockType::Stone, 1));
		p.begin(CELL, BlockType::Stone, BlockType::Air, 100, dig);
		let other = p.begin((9, 9, 9), BlockType::Air, BlockType::Sand, 101, None);

		let dropped = p.on_authoritative(CELL);
		assert_eq!(dropped.len(), 1);
		assert_eq!(dropped[0].inv_delta, dig, "delta surfaces for revert");
		assert_eq!(p.len(), 1, "unrelated entry survives");
		assert!(p.iter().any(|e| e.seq == other));

		assert!(p.on_authoritative((0, 0, 0)).is_empty(), "no match, no-op");
	}
}
