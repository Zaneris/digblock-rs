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
use crate::hotbar::{HOTBAR_SLOTS, Hotbar, HotbarItem, WIRE_EMPTY};
use crate::inventory::Inventory;

/// Whole rows of the hotbar's width, enough of them to sit comfortably
/// above the slottable block-type count, so a full collection always
/// fits.
///
/// That claim used to be a comment and is now a TEST
/// (`the_grid_holds_every_type_a_player_can_own`), because the catalog
/// grew past the old four rows: grid plus bar held every type exactly
/// until §7.2's later materials landed, and a player who owned one of
/// everything would have found a good many of them with nowhere to go.
/// The wire does not care — `from_wire` takes what it is given and pads,
/// so a save written at the old height decodes into the current one with
/// its arrangement intact.
pub const GRID_SLOTS: usize = 63;

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
			if let Some(b) = BlockType::try_from_id(w)
				&& !grid.contains(b)
			{
				grid.slots[i] = Some(b);
			}
		}
		grid
	}
}

/// Repair the layout against the authoritative counts: exhausted slots clear
/// on the BAR as well as in the grid, as do grid slots whose type moved to the
/// bar; owned types placed nowhere go to a free bar slot first and overflow
/// into the grid. Deliberate arrangements survive — a type parked in the grid
/// stays there. The bar sweep runs first, so the slot an exhausted type gives
/// up is free for the same pass to fill.
///
/// A type that isn't [`BlockType::is_slottable`] is swept out of both whatever
/// its count says — the ONE rule keeping water out of the item screens now
/// that its count means "the bucket is charged", and also the migration for
/// saves written when water was an ordinary stack. An emptied slot is RELEASED
/// rather than kept greyed at zero, which is safe only because the engine
/// reconciles and mirrors the layout BEFORE the frame's clicks send edits.
pub fn reconcile_layout(inv: &Inventory, bar: &mut Hotbar, grid: &mut InvGrid) {
	// The tool slot's second axis, which no sweep below can reach: the bar
	// holds `WaterTool` while `Hotbar::active_tool` says WHICH tool, and
	// `from_wire` believes any tool id it is handed. That id arrives from
	// the client on every `Layout`, so ownership has to be re-asserted
	// here — the one place that sees the counts. Without it a crafted
	// `Layout` naming the Resonant Pick is enough to swing it: the
	// server's pick and siphon gates read `active_tool` and nothing else,
	// so its Alchemy unlock and the whole essence price are skipped. The
	// bucket answers to the same rule since it joined the catalog: an
	// unowned claim snaps to the first owned tool, and an EMPTY roster
	// bares the slot — which is also what auto-equips a fresh player's
	// first crafted vessel, since `None` is never "owned".
	if !bar.active_tool().is_some_and(|t| inv.owns_tool(t)) {
		bar.set_active_tool(inv.owned_tools().next());
	}
	for slot in 0..HOTBAR_SLOTS {
		if let Some(HotbarItem::Block(b)) = bar.slots()[slot]
			&& (inv.count(b) == 0 || !b.is_slottable())
		{
			bar.clear_slot(slot);
		}
	}
	for slot in 0..GRID_SLOTS {
		if let Some(b) = grid.get(slot)
			&& (inv.count(b) == 0
				|| !b.is_slottable()
				|| bar.slots().contains(&Some(HotbarItem::Block(b))))
		{
			grid.clear(slot);
		}
	}
	for (b, _) in inv.iter_nonzero() {
		if !b.is_slottable() {
			continue;
		}
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
		// Water is diggable — that IS the scoop — but it is never an item,
		// so it is the one credit that claims no slot anywhere.
		let slottable = types.iter().filter(|(b, _)| b.is_slottable()).count();
		assert!(slottable < types.len(), "the set must include water");
		assert_eq!(on_grid + on_bar - 1, slottable, "everything placed once");
	}

	/// The capacity claim `GRID_SLOTS` makes, as an assertion rather than
	/// a comment: every type a player can actually hold has somewhere to
	/// be. A catalog wave that outgrows the grid fails HERE rather than
	/// silently stranding whatever sorted last.
	#[test]
	fn the_grid_holds_every_type_a_player_can_own() {
		// What a player can actually END UP HOLDING: something has to drop
		// it or the bench has to make it. `is_slottable` alone also counts
		// air and the water flow states, which nothing ever hands you.
		let ownable = B::ALL
			.iter()
			.filter(|b| {
				b.is_slottable() && (b.dig_drop().is_some() || crate::unlock::craft(**b).is_some())
			})
			.count();
		// The bar's last slot is the tool's and holds no block.
		let room = GRID_SLOTS + HOTBAR_SLOTS - 1;
		assert!(
			ownable <= room,
			"{ownable} ownable types into {room} slots — grow GRID_SLOTS"
		);
		// And the headroom is nearly gone, which is worth knowing BEFORE
		// the next wave: the grid is already as tall as the alchemy
		// screen's grid page can be and still fit a short window at the
		// interface-scale ceiling (`panel_sits_above_the_hotbar_and_fits_short_windows`
		// is the wall). Another row needs that screen to scroll first, so
		// a catalog wave that outgrows this is a UI job as well as a block
		// row.
		// Less than a whole spare row: rows come nine at a time, so this is
		// as tight as the grid can sit without dropping below the roster.
		assert!(
			room - ownable < 9,
			"a whole spare row of slack — did the roster shrink?"
		);
	}

	/// A layout saved at the old four-row grid decodes into today's with
	/// its arrangement intact, and one from the future is simply truncated.
	#[test]
	fn a_shorter_saved_grid_decodes_and_pads() {
		let mut wire = vec![WIRE_EMPTY; 36];
		wire[0] = B::Stone as u16;
		wire[35] = B::Dirt as u16;
		let grid = InvGrid::from_wire(&wire);
		assert_eq!(grid.slots()[0], Some(B::Stone));
		assert_eq!(grid.slots()[35], Some(B::Dirt));
		assert!(grid.slots()[36..].iter().all(|s| s.is_none()));
		assert_eq!(
			InvGrid::from_wire(&vec![B::Stone as u16; 999]).slots()[0],
			Some(B::Stone)
		);
	}

	/// Water's count is the bucket's charge, not a stack, so it claims no
	/// slot — including in layouts saved back when it did. That sweep is
	/// the whole migration for existing players.
	#[test]
	fn water_never_holds_a_slot_and_legacy_layouts_give_theirs_up() {
		let inv = inv_of(&[(B::Water, 1), (B::Dirt, 1)]);
		let mut bar = Hotbar::new();
		let mut grid = InvGrid::default();
		reconcile_layout(&inv, &mut bar, &mut grid);
		assert!(
			!bar.slots().contains(&Some(HotbarItem::Block(B::Water))),
			"a charged bucket must not also fill a bar slot"
		);
		assert!(!grid.contains(B::Water));
		assert_eq!(bar.slots()[0], Some(HotbarItem::Block(B::Dirt)));

		// A layout from before the bucket had a state: water sits on the
		// bar and in the grid, with a stack behind it.
		let mut bar = Hotbar::new();
		assert!(bar.assign(0, B::Water));
		let mut grid = InvGrid::default();
		grid.place(3, B::Water);
		reconcile_layout(&inv, &mut bar, &mut grid);
		assert_eq!(
			bar.slots()[0],
			Some(HotbarItem::Block(B::Dirt)),
			"reclaimed"
		);
		assert_eq!(grid.get(3), None);
	}

	/// A client can name any tool it likes on the `Layout` wire —
	/// `Hotbar::from_wire` has no counts to check it against. Reconciling
	/// is what takes an unowned one back, and it is the ONLY thing that
	/// does: the server's pick and siphon gates read `active_tool`
	/// directly, so a tool that survives this call is a tool that gets
	/// used for free.
	#[test]
	fn an_unowned_active_tool_is_taken_back() {
		let inv = inv_of(&[(B::Dirt, 1), (B::Bucket, 1)]);
		let mut grid = InvGrid::default();
		// What a forged Layout does: `from_wire` believes the id.
		let mut wire = [WIRE_EMPTY; HOTBAR_SLOTS];
		wire[HOTBAR_SLOTS - 1] = B::ResonantPick as u16;
		let mut bar = Hotbar::from_wire(&wire);
		assert_eq!(
			bar.active_tool(),
			Some(B::ResonantPick),
			"decoded as claimed"
		);

		reconcile_layout(&inv, &mut bar, &mut grid);
		assert_eq!(
			bar.active_tool(),
			Some(B::Bucket),
			"an unowned tool falls back to the first the player owns"
		);

		// Owning it makes the same claim stick — the gate is the count,
		// not a whitelist.
		let inv = inv_of(&[(B::Dirt, 1), (B::ResonantPick, 1)]);
		let mut bar = Hotbar::from_wire(&wire);
		reconcile_layout(&inv, &mut bar, &mut grid);
		assert_eq!(bar.active_tool(), Some(B::ResonantPick));
		assert!(
			!bar.slots()
				.contains(&Some(HotbarItem::Block(B::ResonantPick))),
			"and owning a tool still never fills a block slot"
		);

		// Losing it mid-session (a rebase that zeroes the count) hands the
		// slot back rather than leaving a phantom pick in hand — and with
		// nothing else owned, the slot goes BARE rather than falling back
		// to a bucket the player never crafted.
		let inv = inv_of(&[(B::Dirt, 1)]);
		reconcile_layout(&inv, &mut bar, &mut grid);
		assert_eq!(bar.active_tool(), None);
	}

	/// The roster the tool slot cycles is the roster ownership is judged
	/// against — one definition, so the client's rail and the server's
	/// gate cannot disagree about what is in hand.
	#[test]
	fn the_owned_roster_is_empty_until_crafted_and_grows_by_count() {
		let inv = Inventory::default();
		assert_eq!(
			inv.owned_tools().count(),
			0,
			"a fresh player owns no tool — the first vessel is crafted"
		);
		let inv = inv_of(&[(B::Bucket, 1), (B::EssenceSiphon, 1), (B::Stone, 40)]);
		let owned: Vec<_> = inv.owned_tools().collect();
		assert!(owned.contains(&B::Bucket) && owned.contains(&B::EssenceSiphon));
		assert!(!owned.contains(&B::Stone), "a stack is not a tool");
		assert!(
			owned.windows(2).all(|w| (w[0] as u16) < (w[1] as u16)),
			"block-id order, so both ends cycle the same way"
		);
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

	/// The bar used to keep an exhausted type greyed at zero. Spending a
	/// stack now releases its slot, and the same pass can hand that slot
	/// to something you actually own.
	#[test]
	fn spending_a_stack_frees_its_bar_slot() {
		let mut inv = inv_of(&[(B::Dirt, 1)]);
		let mut bar = Hotbar::new();
		let mut grid = InvGrid::default();
		reconcile_layout(&inv, &mut bar, &mut grid);
		assert_eq!(bar.slots()[0], Some(HotbarItem::Block(B::Dirt)));

		assert!(inv.try_take(B::Dirt, 1));
		inv.add(B::Stone, 3);
		reconcile_layout(&inv, &mut bar, &mut grid);
		assert_eq!(bar.slots()[0], Some(HotbarItem::Block(B::Stone)));
		assert!(!bar.slots().contains(&Some(HotbarItem::Block(B::Dirt))));
		assert!(!grid.contains(B::Dirt), "and it isn't filed away either");
		// The tool is not a Block, so no sweep can dislodge it.
		assert_eq!(
			bar.slots()[HOTBAR_SLOTS - 1],
			Some(HotbarItem::WaterTool),
			"the tool survives an empty inventory"
		);
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
