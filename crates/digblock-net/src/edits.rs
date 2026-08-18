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
//!
//! Multi-cell actions (a door pair's place/dig/toggle) are LINKED
//! entries sharing one seq via `begin_multi`: confirm drops them all,
//! reject returns every SURVIVING entry for revert (a per-cell
//! authoritative supersede has already removed just that cell's entry,
//! so a late rejection can never clobber the authoritative value), and
//! the shared inventory delta rides only the first entry so it reverts
//! exactly once.

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
		let seq = self.next_seq;
		self.next_seq += 1;
		for (i, &(cell, prev, placed)) in cells.iter().enumerate() {
			self.pending.push_back(PendingEdit {
				seq,
				cell,
				prev,
				placed,
				tick,
				inv_delta: if i == 0 { inv_delta } else { None },
			});
		}
		seq
	}

	/// Server confirmed our edit: drop every entry under that seq (the
	/// predicted cells and inventory delta all stand).
	pub fn confirm(&mut self, seq: u32) {
		self.pending.retain(|e| e.seq != seq);
	}

	/// Server rejected our edit. Every SURVIVING entry under the seq
	/// returns for revert (cells to `prev`, plus the carried inv_delta);
	/// entries already superseded by authoritative writes are absent, so
	/// their cells stay untouched (their deltas were reverted at
	/// supersede time).
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

	/// Is a prediction of ours still in flight on this cell?
	///
	/// For the JOURNAL sync alone, which is a bulk restatement of what the
	/// server had rather than news about a cell. A restatement must not
	/// overwrite a cell we are waiting on: the server's own answer is
	/// coming, as `EditApplied` or `EditRejected`, and either settles it —
	/// whereas the restatement would flick the block back to its old value
	/// for a round trip. Nothing else needs this: a real authoritative
	/// edit IS the news, and supersedes through
	/// [`Self::on_authoritative`].
	pub fn pending_at(&self, cell: (i32, i32, i32)) -> bool {
		self.pending.iter().any(|e| e.cell == cell)
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
	fn authoritative_beats_pending() {
		let mut p = PendingEdits::default();
		let seq = p.begin(CELL, STONE, AIR, 100, None);
		// Another player's edit lands on the same cell first.
		p.on_authoritative(CELL);
		// Our rejection arrives late: no revert allowed.
		assert!(p.reject(seq).is_empty());
	}

	#[test]
	fn authoritative_supersede_returns_entries_for_inventory_revert() {
		let mut p = PendingEdits::default();
		let dig = Some((BlockType::Stone, 1));
		p.begin(CELL, STONE, AIR, 100, dig);
		let other = p.begin((9, 9, 9), AIR, SAND, 101, None);

		let dropped = p.on_authoritative(CELL);
		assert_eq!(dropped.len(), 1);
		assert_eq!(dropped[0].inv_delta, dig, "delta surfaces for revert");
		assert_eq!(p.len(), 1, "unrelated entry survives");
		assert!(p.iter().any(|e| e.seq == other));

		assert!(p.on_authoritative((0, 0, 0)).is_empty(), "no match, no-op");
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

		// Reject returns every surviving entry...
		let seq2 = p.begin_multi(&[(lower, AIR, door), (upper, AIR, door)], 51, delta);
		let dropped = p.reject(seq2);
		assert_eq!(dropped.len(), 2);
		// ...but a per-cell supersede first removes just that cell, and
		// the late rejection reverts only the remainder.
		let seq3 = p.begin_multi(&[(lower, AIR, door), (upper, AIR, door)], 52, delta);
		let superseded = p.on_authoritative(upper);
		assert_eq!(superseded.len(), 1);
		assert_eq!(superseded[0].cell, upper);
		let rest = p.reject(seq3);
		assert_eq!(rest.len(), 1, "only the un-superseded cell reverts");
		assert_eq!(rest[0].cell, lower);
	}
}
