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
	/// THE tool slot marker — permanently the last slot (SKILLS-DESIGN
	/// §6 formalized it: 8 block slots plus one dedicated tool slot).
	/// WHICH tool it holds is a separate axis, [`Hotbar::active_tool`]:
	/// tapping 9 with the slot already selected cycles through the owned
	/// tool roster. The launch tool is the water bucket, whose charge —
	/// one unit of water in the inventory, see
	/// [`crate::inventory::Inventory::bucket_full`] — decides its
	/// clicks: empty, left-click holds on a source to scoop; charged,
	/// right-click pours it back out.
	WaterTool,
}

pub struct Hotbar {
	slots: [Option<HotbarItem>; HOTBAR_SLOTS],
	selected: usize,
	scroll_accum: f32,
	/// The tool the dedicated slot currently holds (always a
	/// [`BlockType::is_tool`] id; the bucket by default). Rides the
	/// `Layout` wire as the 9th entry, so the server's Collect/pour
	/// validation always sees the current pick in wire order.
	active_tool: BlockType,
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
			active_tool: BlockType::Bucket,
		}
	}

	/// The tool the dedicated slot holds.
	#[inline]
	pub fn active_tool(&self) -> BlockType {
		self.active_tool
	}

	/// Advance the active tool through `owned` (the engine's owned
	/// roster, deterministic block-id order), wrapping. With one tool
	/// the extra taps no-op; an active tool that fell out of the roster
	/// snaps to its first entry.
	pub fn cycle_tool(&mut self, owned: &[BlockType]) {
		let Some(pos) = owned.iter().position(|&t| t == self.active_tool) else {
			if let Some(&first) = owned.first() {
				self.active_tool = first;
			}
			return;
		};
		if owned.len() > 1 {
			self.active_tool = owned[(pos + 1) % owned.len()];
		}
	}

	/// Force the active tool. For [`crate::inv_grid::reconcile_layout`]
	/// only — it is the one place that sees the bar AND the counts, so it
	/// is the only place that can tell whether a tool is actually owned.
	/// Everything else cycles through the roster instead.
	pub fn set_active_tool(&mut self, t: BlockType) {
		if t.is_tool() {
			self.active_tool = t;
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
	/// just isn't reachable from the bar yet. A type that isn't
	/// [`BlockType::is_slottable`] is never assigned at all: the dig
	/// credit for a scooped source must not flash a water slot onto the
	/// bar for the frame before `reconcile_layout` sweeps it off again.
	pub fn auto_assign(&mut self, b: BlockType) {
		if !b.is_slottable() {
			return;
		}
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
	/// type never appears twice. Refused (false) for the tool slot —
	/// §6 fixed it in place; blocks live in 1..8.
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

	/// Swap two BLOCK slots (a bar-to-bar drag). The tool slot sits out:
	/// §6 pinned it to the end, so a drag can no longer carry it around
	/// the bar.
	pub fn swap(&mut self, a: usize, b: usize) {
		if a < HOTBAR_SLOTS
			&& b < HOTBAR_SLOTS
			&& self.slots[a] != Some(HotbarItem::WaterTool)
			&& self.slots[b] != Some(HotbarItem::WaterTool)
		{
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

	/// Wire encoding: blocks are their ids, empties `WIRE_EMPTY`, and
	/// the tool slot carries THE ACTIVE TOOL's id — except the bucket,
	/// which keeps the `WIRE_TOOL` sentinel every stored layout already
	/// uses (§6: the sentinel decodes as the migrated bucket id).
	pub fn to_wire(&self) -> [u16; HOTBAR_SLOTS] {
		self.slots.map(|s| match s {
			None => WIRE_EMPTY,
			Some(HotbarItem::WaterTool) if self.active_tool == BlockType::Bucket => WIRE_TOOL,
			Some(HotbarItem::WaterTool) => self.active_tool as u16,
			Some(HotbarItem::Block(b)) => b as u16,
		})
	}

	/// Sanitizing decode of a persisted arrangement: unknown ids turn
	/// into empty slots, a duplicated block keeps its first slot, and
	/// the tool slot is ALWAYS the last one (§6) — `WIRE_TOOL` or a tool
	/// id anywhere in the input just names the active tool (`WIRE_TOOL`
	/// = the migrated bucket), and a legacy layout that had parked the
	/// tool mid-bar comes back with it pinned at the end. Hostile or
	/// stale input always yields a valid bar.
	pub fn from_wire(wire: &[u16; HOTBAR_SLOTS]) -> Self {
		let mut hb = Self::new();
		hb.slots = [None; HOTBAR_SLOTS];
		for (i, &w) in wire.iter().enumerate() {
			let item = match w {
				WIRE_EMPTY => continue,
				WIRE_TOOL => {
					hb.active_tool = BlockType::Bucket;
					continue;
				}
				id => match BlockType::try_from_id(id) {
					Some(b) if b.is_tool() => {
						hb.active_tool = b;
						continue;
					}
					Some(b) => HotbarItem::Block(b),
					None => continue,
				},
			};
			if i < HOTBAR_SLOTS - 1 && !hb.slots.contains(&Some(item)) {
				hb.slots[i] = Some(item);
			}
		}
		hb.slots[HOTBAR_SLOTS - 1] = Some(HotbarItem::WaterTool);
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

	/// §6 pinned the tool slot to the end: nothing lands on it, nothing
	/// clears it, and a drag can no longer carry it around the bar.
	#[test]
	fn the_tool_slot_is_fixed_in_place() {
		let mut h = Hotbar::new();
		let tool = HOTBAR_SLOTS - 1;
		assert!(!h.assign(tool, B::Dirt), "grid drop onto the tool refused");
		assert!(!h.clear_slot(tool), "tool cannot be unassigned");
		h.auto_assign(B::Dirt);
		h.swap(tool, 0);
		assert_eq!(h.slots()[tool], Some(HotbarItem::WaterTool), "unmoved");
		assert_eq!(h.slots()[0], Some(HotbarItem::Block(B::Dirt)), "unmoved");
		h.swap(0, 1); // block-to-block swaps still work
		assert_eq!(h.slots()[1], Some(HotbarItem::Block(B::Dirt)));
	}

	/// §6's second axis: WHICH tool the slot holds. Tapping 9 on the
	/// already-selected slot cycles the owned roster, wrapping; one tool
	/// no-ops; a roster that dropped the active tool snaps to its first.
	#[test]
	fn cycling_walks_the_owned_roster() {
		let mut h = Hotbar::new();
		assert_eq!(h.active_tool(), B::Bucket, "the launch tool");
		h.cycle_tool(&[B::Bucket]);
		assert_eq!(h.active_tool(), B::Bucket, "one tool: taps no-op");
		// A future two-tool roster cycles and wraps. (Bucket is the only
		// tool id today, so stand in a block id for the second — the
		// cycle is pure list arithmetic.)
		h.cycle_tool(&[B::Bucket, B::Stone]);
		assert_eq!(h.active_tool(), B::Stone);
		h.cycle_tool(&[B::Bucket, B::Stone]);
		assert_eq!(h.active_tool(), B::Bucket, "wraps");
		h.cycle_tool(&[B::Dirt]);
		assert_eq!(h.active_tool(), B::Dirt, "dropped roster snaps to first");
	}

	#[test]
	fn wire_round_trip_and_hostile_decode() {
		let mut h = Hotbar::new();
		h.auto_assign(B::Dirt);
		h.auto_assign(B::Stone);
		let wire = h.to_wire();
		assert_eq!(wire[HOTBAR_SLOTS - 1], WIRE_TOOL, "bucket = the sentinel");
		let back = Hotbar::from_wire(&wire);
		assert_eq!(back.slots(), h.slots());
		assert_eq!(back.active_tool(), h.active_tool());

		// Hostile input: unknown id, duplicate block, a legacy mid-bar
		// tool sentinel. The tool slot always comes back pinned last.
		let mut wire = [WIRE_EMPTY; HOTBAR_SLOTS];
		wire[0] = 9999; // unknown → empty
		wire[1] = B::Dirt as u16;
		wire[2] = B::Dirt as u16; // dupe → first wins
		wire[3] = WIRE_TOOL; // legacy parked tool → names the bucket, pins last
		let hb = Hotbar::from_wire(&wire);
		assert_eq!(hb.slots()[0], None);
		assert_eq!(hb.slots()[1], Some(HotbarItem::Block(B::Dirt)));
		assert_eq!(hb.slots()[2], None);
		assert_eq!(hb.slots()[3], None, "the parked spot empties");
		assert_eq!(hb.slots()[HOTBAR_SLOTS - 1], Some(HotbarItem::WaterTool));
		assert_eq!(hb.active_tool(), B::Bucket);

		// A missing tool entry still yields the pinned slot; a tool ID in
		// the input names the active tool instead of occupying a slot.
		let mut wire = [WIRE_EMPTY; HOTBAR_SLOTS];
		wire[0] = B::Stone as u16;
		wire[8] = B::Bucket as u16;
		let hb = Hotbar::from_wire(&wire);
		assert_eq!(hb.slots()[HOTBAR_SLOTS - 1], Some(HotbarItem::WaterTool));
		assert_eq!(hb.active_tool(), B::Bucket);
		assert!(Hotbar::from_wire(&[WIRE_EMPTY; HOTBAR_SLOTS])
			.slots()
			.contains(&Some(HotbarItem::WaterTool)));
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
