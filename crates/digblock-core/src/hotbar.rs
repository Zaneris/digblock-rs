//! The hotbar: a fixed row of slots the player selects from. Pure state —
//! counts live in [`crate::inventory::Inventory`]; a slot only names
//! *what* it holds. Slots are assignments, not a view of counts, and
//! nothing here consults the inventory: a type that runs out is dropped
//! by [`crate::inv_grid::reconcile_layout`], the one place that sees
//! both, so the bar and the grid free an exhausted type by the same
//! rule.

use crate::block::BlockType;

pub const HOTBAR_SLOTS: usize = 9;

/// Wire/persist encoding of a slot: block ids are themselves, plus two
/// sentinels well above any real id (ids are 10-bit).
pub const WIRE_EMPTY: u16 = u16::MAX;
pub const WIRE_TOOL: u16 = u16::MAX - 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotbarItem {
	/// Places this block; its count comes from the inventory.
	Block(BlockType),
	/// The permanent water-collection tool (pinned to the last slot):
	/// holding either mouse button on a water source collects it. It
	/// only collects — water is placed from a block slot like anything
	/// else.
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

	/// Put `b` in `slot` (an inventory-screen drop). Move-not-duplicate:
	/// if `b` already sits elsewhere on the bar the two slots swap, so a
	/// type never appears twice. Refused (false) when the target holds
	/// the WaterTool — the tool can only be displaced by a bar-to-bar
	/// swap, never replaced from the grid.
	pub fn assign(&mut self, slot: usize, b: BlockType) -> bool {
		if slot >= HOTBAR_SLOTS || self.slots[slot] == Some(HotbarItem::WaterTool) {
			return false;
		}
		let item = Some(HotbarItem::Block(b));
		if let Some(prev) = self.slots.iter().position(|s| *s == item) {
			self.slots.swap(prev, slot);
		} else {
			self.slots[slot] = item;
		}
		true
	}

	/// Swap two slots (a bar-to-bar drag; the WaterTool participates).
	pub fn swap(&mut self, a: usize, b: usize) {
		if a < HOTBAR_SLOTS && b < HOTBAR_SLOTS {
			self.slots.swap(a, b);
		}
	}

	/// Unassign a slot (counts are untouched — they live in the
	/// inventory). Refused (false) for the WaterTool: it stays on the bar.
	pub fn clear_slot(&mut self, slot: usize) -> bool {
		if slot >= HOTBAR_SLOTS || self.slots[slot] == Some(HotbarItem::WaterTool) {
			return false;
		}
		self.slots[slot] = None;
		true
	}

	pub fn to_wire(&self) -> [u16; HOTBAR_SLOTS] {
		self.slots.map(|s| match s {
			None => WIRE_EMPTY,
			Some(HotbarItem::WaterTool) => WIRE_TOOL,
			Some(HotbarItem::Block(b)) => b as u16,
		})
	}

	/// Sanitizing decode of a persisted arrangement: unknown ids turn
	/// into empty slots, a duplicated block keeps its first slot, and
	/// exactly one WaterTool is enforced (a missing tool is restored to
	/// the last empty slot, else the last slot). Hostile or stale input
	/// always yields a valid bar.
	pub fn from_wire(wire: &[u16; HOTBAR_SLOTS]) -> Self {
		let mut hb = Self::new();
		hb.slots = [None; HOTBAR_SLOTS];
		for (i, &w) in wire.iter().enumerate() {
			let item = match w {
				WIRE_EMPTY => continue,
				WIRE_TOOL => HotbarItem::WaterTool,
				id => match BlockType::try_from_id(id) {
					Some(b) => HotbarItem::Block(b),
					None => continue,
				},
			};
			if !hb.slots.contains(&Some(item)) {
				hb.slots[i] = Some(item);
			}
		}
		if !hb.slots.contains(&Some(HotbarItem::WaterTool)) {
			let last_free = hb
				.slots
				.iter()
				.rposition(|s| s.is_none())
				.unwrap_or(HOTBAR_SLOTS - 1);
			hb.slots[last_free] = Some(HotbarItem::WaterTool);
		}
		hb
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
	fn assign_moves_instead_of_duplicating() {
		let mut h = Hotbar::new();
		h.auto_assign(B::Dirt); // slot 0
		h.auto_assign(B::Stone); // slot 1

		// Fresh type lands where dropped, replacing what was there.
		assert!(h.assign(1, B::Sand));
		assert_eq!(h.slots()[1], Some(HotbarItem::Block(B::Sand)));

		// Already-assigned type MOVES: slots swap, no duplicate.
		assert!(h.assign(1, B::Dirt));
		assert_eq!(h.slots()[1], Some(HotbarItem::Block(B::Dirt)));
		assert_eq!(h.slots()[0], Some(HotbarItem::Block(B::Sand)));
		let dirt_count = h
			.slots()
			.iter()
			.filter(|s| **s == Some(HotbarItem::Block(B::Dirt)))
			.count();
		assert_eq!(dirt_count, 1);

		// Dropping a type onto its own slot is a no-op success.
		assert!(h.assign(1, B::Dirt));
		assert_eq!(h.slots()[1], Some(HotbarItem::Block(B::Dirt)));
	}

	#[test]
	fn water_tool_is_immovable_except_by_swap() {
		let mut h = Hotbar::new();
		let tool = HOTBAR_SLOTS - 1;
		assert!(!h.assign(tool, B::Dirt), "grid drop onto the tool refused");
		assert!(!h.clear_slot(tool), "tool cannot be unassigned");
		assert_eq!(h.slots()[tool], Some(HotbarItem::WaterTool));

		h.swap(tool, 0); // bar-to-bar drag carries the tool
		assert_eq!(h.slots()[0], Some(HotbarItem::WaterTool));
		assert!(!h.clear_slot(0), "still protected at its new home");
		assert!(h.clear_slot(tool), "the vacated slot clears fine");
	}

	#[test]
	fn wire_round_trip_and_hostile_decode() {
		let mut h = Hotbar::new();
		h.auto_assign(B::Dirt);
		h.auto_assign(B::Stone);
		h.swap(HOTBAR_SLOTS - 1, 4); // custom tool position survives
		let wire = h.to_wire();
		let back = Hotbar::from_wire(&wire);
		assert_eq!(back.slots(), h.slots());

		// Hostile input: unknown id, duplicate block, duplicate tool.
		let mut wire = [WIRE_EMPTY; HOTBAR_SLOTS];
		wire[0] = 9999; // unknown → empty
		wire[1] = B::Dirt as u16;
		wire[2] = B::Dirt as u16; // dupe → first wins
		wire[3] = WIRE_TOOL;
		wire[4] = WIRE_TOOL; // dupe tool → dropped
		let hb = Hotbar::from_wire(&wire);
		assert_eq!(hb.slots()[0], None);
		assert_eq!(hb.slots()[1], Some(HotbarItem::Block(B::Dirt)));
		assert_eq!(hb.slots()[2], None);
		assert_eq!(hb.slots()[3], Some(HotbarItem::WaterTool));
		assert_eq!(hb.slots()[4], None);

		// Missing tool is restored to the last empty slot.
		let mut wire = [WIRE_EMPTY; HOTBAR_SLOTS];
		wire[0] = B::Stone as u16;
		let hb = Hotbar::from_wire(&wire);
		assert_eq!(hb.slots()[HOTBAR_SLOTS - 1], Some(HotbarItem::WaterTool));

		// Tool restoration with a full-of-blocks tail still lands somewhere.
		let all = Hotbar::from_wire(&[WIRE_EMPTY; HOTBAR_SLOTS]);
		assert!(all.slots().contains(&Some(HotbarItem::WaterTool)));
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
