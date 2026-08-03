//! The hotbar: a fixed row of slots the player selects from. Pure state —
//! counts live in [`crate::inventory::Inventory`]; a slot only names
//! *what* it holds. Slots are sticky: a block type keeps its slot at zero
//! count (rendered greyed) so the bar never shifts underfoot, matching a
//! future overflow inventory where slots are assignments, not a view of
//! counts.

use crate::block::BlockType;

pub const HOTBAR_SLOTS: usize = 9;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotbarItem {
	/// Places this block; its count comes from the inventory.
	Block(BlockType),
	/// The permanent water-collection tool (pinned to the last slot):
	/// dig-hold on a water source collects it; placing with it selected
	/// places a `Water` source (requires Water stock).
	WaterTool,
}

pub struct Hotbar {
	slots: [Option<HotbarItem>; HOTBAR_SLOTS],
	selected: usize,
	scroll_accum: f32,
}

impl Default for Hotbar {
	fn default() -> Self {
		Self::new()
	}
}

impl Hotbar {
	/// The tool is pinned to the LAST slot (digit 9) — out of the way of
	/// the block slots that fill front-to-back — and selection starts on
	/// slot 1, where the first dug block will land.
	pub fn new() -> Self {
		let mut slots = [None; HOTBAR_SLOTS];
		slots[HOTBAR_SLOTS - 1] = Some(HotbarItem::WaterTool);
		Self {
			slots,
			selected: 0,
			scroll_accum: 0.0,
		}
	}

	pub fn slots(&self) -> &[Option<HotbarItem>; HOTBAR_SLOTS] {
		&self.slots
	}

	pub fn selected_index(&self) -> usize {
		self.selected
	}

	pub fn selected_item(&self) -> Option<HotbarItem> {
		self.slots[self.selected]
	}

	/// First acquisition of a block type claims the first free slot (the
	/// tool holds the last one, so blocks fill 1..8; called on every dig
	/// credit — later credits find it already assigned). A full bar
	/// leaves the type unassigned — it still counts in the inventory,
	/// just isn't reachable from the bar yet.
	pub fn auto_assign(&mut self, b: BlockType) {
		let item = Some(HotbarItem::Block(b));
		if self.slots.contains(&item) {
			return;
		}
		if let Some(free) = self.slots.iter_mut().find(|s| s.is_none()) {
			*free = item;
		}
	}

	/// Number-key selection: digits 1..=9 map to slots 0..=8.
	pub fn select_digit(&mut self, digit: u8) {
		if (1..=HOTBAR_SLOTS as u8).contains(&digit) {
			self.selected = digit as usize - 1;
		}
	}

	/// Wheel selection: accumulate notches, step once per ±1.0, wrapping.
	/// Pixel-delta scrolls (trackpads) arrive fractional; the accumulator
	/// turns mistuned normalization into a feel issue, not a skip.
	pub fn scroll(&mut self, dy: f32) {
		self.scroll_accum += dy;
		while self.scroll_accum >= 1.0 {
			self.scroll_accum -= 1.0;
			self.selected = (self.selected + HOTBAR_SLOTS - 1) % HOTBAR_SLOTS;
		}
		while self.scroll_accum <= -1.0 {
			self.scroll_accum += 1.0;
			self.selected = (self.selected + 1) % HOTBAR_SLOTS;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType as B;

	#[test]
	fn tool_is_pinned_last_and_assignment_is_first_seen() {
		let mut h = Hotbar::new();
		assert_eq!(h.selected_index(), 0, "starts on slot 1");
		assert_eq!(h.selected_item(), None, "slot 1 is empty at first");
		h.auto_assign(B::Dirt);
		h.auto_assign(B::Stone);
		h.auto_assign(B::Dirt); // repeat: keeps its slot
		assert_eq!(h.slots()[0], Some(HotbarItem::Block(B::Dirt)));
		assert_eq!(h.slots()[1], Some(HotbarItem::Block(B::Stone)));
		assert_eq!(h.slots()[2], None, "sticky, no shifting or dupes");
		assert_eq!(
			h.slots()[HOTBAR_SLOTS - 1],
			Some(HotbarItem::WaterTool),
			"tool pinned to the last slot"
		);
	}

	#[test]
	fn full_bar_leaves_new_types_unassigned() {
		let mut h = Hotbar::new();
		for &b in BlockType::ALL.iter().filter(|b| b.dig_drop().is_some()) {
			h.auto_assign(b);
		}
		let assigned: Vec<_> = h.slots().iter().flatten().collect();
		assert!(assigned.len() <= HOTBAR_SLOTS);
		assert_eq!(
			h.slots()[HOTBAR_SLOTS - 1],
			Some(HotbarItem::WaterTool),
			"tool survives"
		);
	}

	#[test]
	fn digit_and_scroll_selection() {
		let mut h = Hotbar::new();
		h.select_digit(9); // the tool slot
		assert_eq!(h.selected_index(), HOTBAR_SLOTS - 1);
		h.select_digit(3);
		assert_eq!(h.selected_index(), 2);
		h.select_digit(0); // out of range: ignored
		h.select_digit(10);
		assert_eq!(h.selected_index(), 2);

		h.scroll(-1.0); // toward higher slots
		assert_eq!(h.selected_index(), 3);
		h.scroll(0.5);
		assert_eq!(h.selected_index(), 3, "accumulates below a notch");
		h.scroll(0.5);
		assert_eq!(h.selected_index(), 2);

		h.select_digit(1);
		h.scroll(1.0); // wraps upward
		assert_eq!(h.selected_index(), HOTBAR_SLOTS - 1);
		h.scroll(-1.0);
		assert_eq!(h.selected_index(), 0);
	}
}
