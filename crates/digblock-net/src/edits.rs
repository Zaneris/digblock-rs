//! Predicted block edits awaiting server confirmation.
//!
//! Precedence is structural, not timing-based: every authoritative edit broadcast first `confirm`s
//! our own seq (if it is ours), then `on_authoritative(cell)` drops any other pending entries on
//! that cell, then the broadcast value is applied. A rejection arriving after an authoritative
//! write to the same cell finds no entry and reverts nothing — authority wins.
//!
//! Each entry also carries the inventory delta predicted with it. The cell and the delta settle on
//! different events: a superseding authoritative write owns the CELL at once, while the DELTA must
//! wait for the seq's own verdict — the server answers every request and may yet apply a superseded
//! edit, so a refund at supersede time would part the two ends' counts permanently.
//!
//! Multi-cell actions are LINKED entries sharing one seq via `begin_multi`: confirm drops them all,
//! reject returns every SURVIVING entry for revert, and the shared inventory delta rides only the
//! first entry so it reverts exactly once.

use crate::protocol::NO_CLIENT_SEQ;
use digblock_core::block::BlockType;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub struct PendingEdit {
	pub seq: u32,
	pub cell: (i32, i32, i32),
	/// What we overwrote locally (full u16 cell) — the revert target.
	pub prev: u16,
	/// What we wrote (full u16 cell; Air for dig).
	pub placed: u16,
	/// Predicted tick when applied.
	pub tick: u32,
	/// Predicted inventory delta applied locally with this edit: a
	/// positive count for a dig credit, a negative one for a place debit.
	/// On a multi-cell entry group, only the FIRST entry carries it.
	pub inv_delta: Option<(BlockType, i32)>,
	/// An authoritative write landed on this cell first: the cell is the
	/// broadcast's and must never revert to `prev`. The delta is still
	/// live — the seq's own verdict settles it.
	pub superseded: bool,
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
		prev: u16,
		placed: u16,
		tick: u32,
		inv_delta: Option<(BlockType, i32)>,
	) -> u32 {
		self.begin_multi(&[(cell, prev, placed)], tick, inv_delta)
	}

	/// Record a linked multi-cell prediction under ONE seq (door pairs).
	/// `cells` is (cell, prev, placed) per touched cell; `inv_delta`
	/// lands on the first entry only.
	pub fn begin_multi(
		&mut self,
		cells: &[((i32, i32, i32), u16, u16)],
		tick: u32,
		inv_delta: Option<(BlockType, i32)>,
	) -> u32 {
		self.next_seq = self.next_seq.wrapping_add(1);
		if self.next_seq == NO_CLIENT_SEQ {
			self.next_seq = self.next_seq.wrapping_add(1);
		}
		let seq = self.next_seq;
		for (i, &(cell, prev, placed)) in cells.iter().enumerate() {
			self.pending.push_back(PendingEdit {
				seq,
				cell,
				prev,
				placed,
				tick,
				inv_delta: if i == 0 { inv_delta } else { None },
				superseded: false,
			});
		}
		seq
	}

	/// Server confirmed our edit: drop every entry under that seq,
	/// superseded ones included (the inventory delta stands either way —
	/// the server applied it for real).
	pub fn confirm(&mut self, seq: u32) {
		self.pending.retain(|e| e.seq != seq);
	}

	/// Server rejected our edit. Every entry under the seq returns for
	/// revert: the carried inv_delta always, the cell to `prev` only
	/// where `superseded` is false — a superseded cell belongs to the
	/// authoritative write that took it.
	pub fn reject(&mut self, seq: u32) -> Vec<PendingEdit> {
		let mut dropped = Vec::new();
		self.pending.retain(|e| {
			if e.seq == seq {
				dropped.push(*e);
				false
			} else {
				true
			}
		});
		dropped
	}

	/// Is a prediction of ours still in flight on this cell? For the JOURNAL sync alone, a bulk
	/// restatement of what the server had rather than news about a cell. A restatement must not
	/// overwrite a cell we are waiting on: the server's own answer is coming, as `EditApplied` or
	/// `EditRejected`, and either settles it, whereas the restatement would flick the block back
	/// for a round trip. A real authoritative edit IS the news: [`Self::on_authoritative`].
	pub fn pending_at(&self, cell: (i32, i32, i32)) -> bool {
		// A superseded entry no longer owns its cell — a restatement of
		// what took it is free to land.
		self.pending.iter().any(|e| e.cell == cell && !e.superseded)
	}

	/// An authoritative edit landed on `cell`: pending entries there are
	/// marked superseded — the broadcast value owns the cell — but they
	/// stay pending, because their deltas are NOT settled here. The
	/// server answers every seq, and that verdict decides each delta:
	/// `confirm` keeps it, `reject` hands it back for refund.
	pub fn on_authoritative(&mut self, cell: (i32, i32, i32)) {
		for e in self.pending.iter_mut().filter(|e| e.cell == cell) {
			e.superseded = true;
		}
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
	const STONE: u16 = BlockType::Stone as u16;
	const AIR: u16 = BlockType::Air as u16;
	const DIRT: u16 = BlockType::Dirt as u16;
	const SAND: u16 = BlockType::Sand as u16;

	#[test]
	fn reject_reverts_confirm_drops() {
		let mut p = PendingEdits::default();
		let a = p.begin(CELL, STONE, AIR, 100, None);
		let b = p.begin((6, 130, -2), AIR, DIRT, 101, Some((BlockType::Dirt, -1)));
		p.confirm(a);
		assert_eq!(p.len(), 1);
		let e = p.reject(b);
		assert_eq!(e.len(), 1, "pending entry returns for revert");
		assert_eq!(e[0].cell, (6, 130, -2));
		assert_eq!(e[0].prev, AIR);
		assert_eq!(e[0].inv_delta, Some((BlockType::Dirt, -1)));
		assert!(p.is_empty());
		// Double reject: gone.
		assert!(p.reject(b).is_empty());
	}

	#[test]
	fn authoritative_beats_pending_cell_but_not_delta() {
		let mut p = PendingEdits::default();
		let dig = Some((BlockType::Stone, 1));
		let seq = p.begin(CELL, STONE, AIR, 100, dig);
		// Another player's edit lands on the same cell first: the cell is
		// no longer ours, but the delta still awaits our seq's verdict.
		p.on_authoritative(CELL);
		assert!(!p.pending_at(CELL), "a restatement may land here now");
		assert_eq!(p.len(), 1, "the entry parks until the verdict");
		// Our rejection arrives late: the delta comes back for refund,
		// flagged so the caller leaves the cell alone.
		let e = p.reject(seq);
		assert_eq!(e.len(), 1);
		assert!(e[0].superseded);
		assert_eq!(e[0].inv_delta, dig);
		assert!(p.is_empty());
	}

	#[test]
	fn confirm_after_supersede_keeps_the_delta() {
		// Two predictions race on one cell (a slab grown twice): the first
		// seq's own ack supersedes the second, whose delta must STAND —
		// the server goes on to apply it and debit for real.
		let mut p = PendingEdits::default();
		let a = p.begin(CELL, AIR, DIRT, 100, Some((BlockType::Dirt, -1)));
		let b = p.begin(CELL, DIRT, SAND, 101, Some((BlockType::Dirt, -1)));
		p.confirm(a);
		p.on_authoritative(CELL);
		assert_eq!(p.len(), 1, "the second prediction still awaits its verdict");
		assert!(p.iter().all(|e| e.seq == b && e.superseded));
		// Its own ack lands: dropped with nothing handed back to refund.
		p.confirm(b);
		assert!(p.is_empty());

		// Unrelated entries are untouched by a supersede elsewhere.
		let other = p.begin((9, 9, 9), AIR, SAND, 102, None);
		p.on_authoritative((0, 0, 0));
		assert!(p.iter().any(|e| e.seq == other && !e.superseded));
	}

	#[test]
	fn seqs_skip_the_companion_sentinel() {
		// A companion broadcast arrives under `NO_CLIENT_SEQ`; a real
		// prediction must never be confirmable by one.
		let mut p = PendingEdits::default();
		for _ in 0..4 {
			let seq = p.begin(CELL, STONE, AIR, 100, None);
			assert_ne!(seq, NO_CLIENT_SEQ);
			p.confirm(seq);
		}
	}

	#[test]
	fn multi_cell_group_shares_one_seq_and_delta() {
		let mut p = PendingEdits::default();
		let lower = (2, 10, 3);
		let upper = (2, 11, 3);
		let door: u16 = 24 | 0x8000; // id + a state bit: opaque payloads ok
		let delta = Some((BlockType::Dirt, -1));
		let seq = p.begin_multi(&[(lower, AIR, door), (upper, AIR, door)], 50, delta);
		assert_eq!(p.len(), 2);
		assert_eq!(
			p.iter().filter(|e| e.inv_delta.is_some()).count(),
			1,
			"the shared delta rides one entry"
		);

		// Confirm drops the whole group.
		p.confirm(seq);
		assert!(p.is_empty());

		// Reject returns every entry...
		let seq2 = p.begin_multi(&[(lower, AIR, door), (upper, AIR, door)], 51, delta);
		let dropped = p.reject(seq2);
		assert_eq!(dropped.len(), 2);
		assert!(dropped.iter().all(|e| !e.superseded));
		// ...and a per-cell supersede first flags just that cell, so the
		// late rejection reverts both deltas but only the un-superseded
		// cell.
		let seq3 = p.begin_multi(&[(lower, AIR, door), (upper, AIR, door)], 52, delta);
		p.on_authoritative(upper);
		let rest = p.reject(seq3);
		assert_eq!(rest.len(), 2, "every entry comes back for its delta");
		assert!(rest.iter().all(|e| e.superseded == (e.cell == upper)));
	}
}
