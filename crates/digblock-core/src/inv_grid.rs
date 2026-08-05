//! The inventory grid: a fixed field of slots behind the hotbar, holding
//! every owned block type the bar doesn't. Like the hotbar, slots are
//! *assignments* — counts live in [`crate::inventory::Inventory`] — so a
//! type lives in exactly one place: a bar slot or a grid slot, never
//! both. The arrangement is player-authored (drag-and-drop) and persists
//! (server identity store, local saves); [`reconcile_layout`] repairs it
//! against the authoritative counts after every inventory change.
//!
//! The WaterTool never enters the grid (it is pinned to the bar), so grid
//! slots hold plain [`BlockType`]s — the invariant is in the type.

use crate::block::BlockType;
use crate::hotbar::{Hotbar, HotbarItem, WIRE_EMPTY};
use crate::inventory::Inventory;

/// 4 rows of 9 (matching the hotbar's width) — comfortably above the
/// block-type count, so a full collection always fits.
pub const GRID_SLOTS: usize = 36;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InvGrid {
	slots: [Option<BlockType>; GRID_SLOTS],
}

impl Default for InvGrid {
	fn default() -> Self {
		Self {
			slots: [None; GRID_SLOTS],
		}
	}
}

impl InvGrid {
	pub fn slots(&self) -> &[Option<BlockType>; GRID_SLOTS] {
		&self.slots
	}

	pub fn get(&self, slot: usize) -> Option<BlockType> {
		self.slots.get(slot).copied().flatten()
	}

	pub fn contains(&self, b: BlockType) -> bool {
		self.slots.contains(&Some(b))
	}

	/// Put `b` in `slot` (a drop). Move-not-duplicate: if `b` already
	/// sits elsewhere in the grid the two slots swap, so a type never
	/// appears twice.
	pub fn place(&mut self, slot: usize, b: BlockType) {
		if slot >= GRID_SLOTS {
			return;
		}
		if let Some(prev) = self.slots.iter().position(|s| *s == Some(b)) {
			self.slots.swap(prev, slot);
		} else {
			self.slots[slot] = Some(b);
		}
	}

	pub fn clear(&mut self, slot: usize) {
		if let Some(s) = self.slots.get_mut(slot) {
			*s = None;
		}
	}

	/// Swap two slots (a grid-to-grid drag; empty targets just move).
	pub fn swap(&mut self, a: usize, b: usize) {
		if a < GRID_SLOTS && b < GRID_SLOTS {
			self.slots.swap(a, b);
		}
	}

	/// File `b` into the first empty slot (a bar unassign, a new
	/// acquisition overflowing a full bar). No-op if already present or
	/// no slot is free.
	pub fn file(&mut self, b: BlockType) {
		if self.contains(b) {
			return;
		}
		if let Some(free) = self.slots.iter_mut().find(|s| s.is_none()) {
			*free = Some(b);
		}
	}

	pub fn to_wire(&self) -> [u16; GRID_SLOTS] {
		self.slots.map(|s| match s {
			None => WIRE_EMPTY,
			Some(b) => b as u16,
		})
	}

	/// Sanitizing decode of a persisted/wire arrangement: unknown ids
	/// (including the hotbar's tool sentinel — the tool never enters the
	/// grid) turn into empty slots, a duplicated type keeps its first
	/// slot, extra entries beyond [`GRID_SLOTS`] are ignored. Hostile or
	/// stale input always yields a valid grid.
	pub fn from_wire(wire: &[u16]) -> Self {
		let mut grid = Self::default();
		for (i, &w) in wire.iter().take(GRID_SLOTS).enumerate() {
			if let Some(b) = BlockType::try_from_id(w) {
				if !grid.contains(b) {
					grid.slots[i] = Some(b);
				}
			}
		}
		grid
	}
}

/// Repair the layout against the authoritative counts: grid slots whose
/// type hit zero or moved to the bar clear; owned types placed nowhere
/// go to a free bar slot first (the classic dig-credit auto-assign),
/// overflowing into the first empty grid slot. Deliberate arrangements
/// survive — a type the player parked in the grid stays in the grid.
pub fn reconcile_layout(inv: &Inventory, bar: &mut Hotbar, grid: &mut InvGrid) {
	for slot in 0..GRID_SLOTS {
		if let Some(b) = grid.get(slot) {
			if inv.count(b) == 0 || bar.slots().contains(&Some(HotbarItem::Block(b))) {
				grid.clear(slot);
			}
		}
	}
	for (b, _) in inv.iter_nonzero() {
		let on_bar = bar.slots().contains(&Some(HotbarItem::Block(b)));
		if on_bar || grid.contains(b) {
			continue;
		}
		bar.auto_assign(b);
		if !bar.slots().contains(&Some(HotbarItem::Block(b))) {
			grid.file(b);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType as B;
	use crate::hotbar::{HOTBAR_SLOTS, WIRE_TOOL};

	fn inv_of(types: &[(B, u32)]) -> Inventory {
		let mut inv = Inventory::default();
		for &(b, n) in types {
			inv.add(b, n);
		}
		inv
	}

	#[test]
	fn reconcile_files_new_types_bar_first_then_grid() {
		let inv = inv_of(&[(B::Dirt, 5), (B::Stone, 2)]);
		let mut bar = Hotbar::new();
		let mut grid = InvGrid::default();
		reconcile_layout(&inv, &mut bar, &mut grid);
		assert_eq!(bar.slots()[0], Some(HotbarItem::Block(B::Dirt)));
		assert_eq!(bar.slots()[1], Some(HotbarItem::Block(B::Stone)));
		assert!(!grid.contains(B::Dirt), "bar had room — grid stays empty");

		// Bar full: the overflow lands in the grid.
		let types: Vec<_> = B::ALL
			.iter()
			.filter(|b| b.dig_drop().is_some())
			.map(|&b| (b, 1u32))
			.collect();
		assert!(types.len() > HOTBAR_SLOTS, "test needs an overflowing set");
		let inv = inv_of(&types);
		let mut bar = Hotbar::new();
		let mut grid = InvGrid::default();
		reconcile_layout(&inv, &mut bar, &mut grid);
		let on_bar = bar.slots().iter().flatten().count();
		let on_grid = grid.slots().iter().flatten().count();
		assert_eq!(on_bar, HOTBAR_SLOTS, "bar filled (incl. tool)");
		assert_eq!(on_grid + on_bar - 1, types.len(), "everything placed once");
	}

	#[test]
	fn reconcile_clears_zeroed_and_bar_assigned_types() {
		let mut inv = inv_of(&[(B::Dirt, 1), (B::Stone, 1)]);
		let mut bar = Hotbar::new();
		let mut grid = InvGrid::default();
		grid.place(3, B::Dirt);
		grid.place(7, B::Stone);
		// Dirt moves to the bar; Stone's count zeroes out.
		bar.auto_assign(B::Dirt);
		assert!(inv.try_take(B::Stone, 1));
		reconcile_layout(&inv, &mut bar, &mut grid);
		assert!(!grid.contains(B::Dirt), "bar-assigned type left the grid");
		assert!(!grid.contains(B::Stone), "zero-count type left the grid");
	}

	#[test]
	fn parked_types_stay_parked() {
		let inv = inv_of(&[(B::Dirt, 5)]);
		let mut bar = Hotbar::new();
		let mut grid = InvGrid::default();
		grid.place(11, B::Dirt); // player parked it deliberately
		reconcile_layout(&inv, &mut bar, &mut grid);
		assert_eq!(grid.get(11), Some(B::Dirt), "no bounce back to the bar");
		assert!(!bar.slots().contains(&Some(HotbarItem::Block(B::Dirt))));
	}

	#[test]
	fn place_moves_instead_of_duplicating() {
		let mut grid = InvGrid::default();
		grid.place(0, B::Dirt);
		grid.place(1, B::Stone);
		grid.place(1, B::Dirt); // move-not-duplicate: swaps
		assert_eq!(grid.get(1), Some(B::Dirt));
		assert_eq!(grid.get(0), Some(B::Stone));
		assert_eq!(
			grid.slots().iter().filter(|s| **s == Some(B::Dirt)).count(),
			1
		);
		grid.swap(1, 5);
		assert_eq!(grid.get(5), Some(B::Dirt));
		assert_eq!(grid.get(1), None);
	}

	#[test]
	fn wire_round_trip_and_hostile_decode() {
		let mut grid = InvGrid::default();
		grid.place(2, B::Sand);
		grid.place(35, B::Wood);
		assert_eq!(InvGrid::from_wire(&grid.to_wire()), grid);

		// Hostile: unknown id, the tool sentinel, a duplicate, oversize.
		let mut wire = vec![WIRE_EMPTY; GRID_SLOTS + 8];
		wire[0] = 9999;
		wire[1] = WIRE_TOOL;
		wire[2] = B::Dirt as u16;
		wire[3] = B::Dirt as u16;
		wire[GRID_SLOTS + 2] = B::Stone as u16; // beyond the grid: ignored
		let grid = InvGrid::from_wire(&wire);
		assert_eq!(grid.get(0), None);
		assert_eq!(grid.get(1), None, "tool never enters the grid");
		assert_eq!(grid.get(2), Some(B::Dirt));
		assert_eq!(grid.get(3), None, "dupe keeps first slot");
		assert!(!grid.contains(B::Stone));

		// Short input (older peer): remaining slots empty.
		let short = InvGrid::from_wire(&[B::Sand as u16]);
		assert_eq!(short.get(0), Some(B::Sand));
	}
}
