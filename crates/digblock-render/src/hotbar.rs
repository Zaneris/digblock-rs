//! Hotbar HUD view: a pure snapshot of (hotbar assignments × inventory
//! counts) built once per frame. Emission into draw commands lives in
//! `ui/hud.rs`; the icon pipeline in `ui/icons.rs`.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{HOTBAR_SLOTS, Hotbar, HotbarItem};
use digblock_core::inventory::Inventory;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SlotView {
	Empty,
	/// The tool slot: WHICH tool to draw, whether the bucket is charged
	/// (the bucket's own count, so the bar shows the charge where a stack
	/// would show its number), and how present the icon is mid-handover.
	/// The slot always holds `HotbarItem::WaterTool` on the wire —
	/// `active_tool` is the second axis — so the icon has to come from the
	/// tool, not from the slot, or every tool in the game draws as a
	/// bucket.
	Tool {
		tool: BlockType,
		full: bool,
		present: f32,
	},
	Block {
		block: BlockType,
		count: u32,
	},
}

/// The tool slot's handover animation.
///
/// Cycling tools shouldn't hard-cut any more than the transmute tab's
/// family tiles do: the outgoing tool shrinks away, the incoming one
/// grows in, and everything else about the exchange — including which
/// tools the faded rail lists — happens at the invisible midpoint, so
/// no tool is ever shown in two places at once.
///
/// It watches the active tool rather than being told about the keypress,
/// so every path that can change it animates: the digit tap, a server
/// `Layout` rebase, a rejoin restoring a saved bar.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ToolSwap {
	shown: BlockType,
	/// Seconds left in the handover; 0 at rest.
	left: f32,
}

impl Default for ToolSwap {
	fn default() -> Self {
		Self {
			shown: BlockType::Bucket,
			left: 0.0,
		}
	}
}

impl ToolSwap {
	/// The handover window — the transmute tiles' `FAMILY_SWAP_SECS`, so
	/// the two exchanges are the same gesture at the same speed.
	pub const SECS: f32 = 0.30;

	/// Advance by `dt`, taking `active` as the tool the hotbar holds NOW.
	/// Read every frame rather than latched, so the slot can never be
	/// left drawing a tool the player doesn't have.
	pub fn advance(&mut self, dt: f32, active: BlockType) {
		if active != self.shown {
			if self.left <= 0.0 {
				self.left = Self::SECS;
			} else if crate::ui::handover(1.0 - self.left / Self::SECS).0 {
				// A second tap while the incoming tool is still GROWING IN.
				// Mirroring `left` about the changeover puts the handover
				// back in its shrinking half at the same presence, so the
				// icon the player is looking at shrinks away and the new one
				// grows in behind it. Letting it fall through instead
				// swapped the icon in place at whatever size it had reached
				// — the one hard cut this whole animation exists to avoid,
				// and the easiest to hit, since cycling a roster means
				// tapping 9 repeatedly.
				self.left = Self::SECS - self.left;
			}
		}
		self.left = (self.left - dt).max(0.0);
		// Past the changeover — present has passed through zero — what
		// grows back in is whatever is active by then, so a second tap
		// mid-handover lands on the newest tool rather than queueing.
		if self.left <= 0.0 || crate::ui::handover(1.0 - self.left / Self::SECS).0 {
			self.shown = active;
		}
	}

	/// The tool to DRAW — the outgoing one until the changeover.
	pub fn shown(&self) -> BlockType {
		self.shown
	}

	/// 0..=1, scaling the icon and its alpha; 1.0 at rest.
	pub fn present(&self) -> f32 {
		if self.left <= 0.0 {
			1.0
		} else {
			crate::ui::handover(1.0 - self.left / Self::SECS).1
		}
	}
}

pub struct HotbarView {
	pub slots: [SlotView; HOTBAR_SLOTS],
	pub selected: usize,
}

impl HotbarView {
	pub fn build(hotbar: &Hotbar, inventory: &Inventory, swap: &ToolSwap) -> Self {
		let mut slots = [SlotView::Empty; HOTBAR_SLOTS];
		for (i, s) in hotbar.slots().iter().enumerate() {
			slots[i] = match s {
				None => SlotView::Empty,
				Some(HotbarItem::WaterTool) => SlotView::Tool {
					tool: swap.shown(),
					full: inventory.bucket_full(),
					present: swap.present(),
				},
				Some(HotbarItem::Block(b)) => SlotView::Block {
					block: *b,
					count: inventory.count(*b),
				},
			};
		}
		Self {
			slots,
			selected: hotbar.selected_index(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn view_mirrors_hotbar_and_counts() {
		let mut hotbar = Hotbar::new();
		hotbar.auto_assign(BlockType::Dirt);
		hotbar.auto_assign(BlockType::Stone);
		let mut inv = Inventory::default();
		inv.add(BlockType::Dirt, 3);
		let v = HotbarView::build(&hotbar, &inv, &ToolSwap::default());
		assert_eq!(
			v.slots[0],
			SlotView::Block {
				block: BlockType::Dirt,
				count: 3
			}
		);
		assert_eq!(
			v.slots[1],
			SlotView::Block {
				block: BlockType::Stone,
				count: 0
			}
		);
		assert_eq!(v.slots[2], SlotView::Empty);
		assert_eq!(
			v.slots[HOTBAR_SLOTS - 1],
			SlotView::Tool {
				tool: BlockType::Bucket,
				full: false,
				present: 1.0
			},
			"an empty bucket"
		);
		assert_eq!(v.selected, 0);
	}

	/// The tool's charge is the water count, so a scoop shows up in the
	/// bar without anything else being told about it.
	#[test]
	fn a_charged_bucket_shows_in_its_slot() {
		let hotbar = Hotbar::new();
		let mut inv = Inventory::default();
		inv.add(BlockType::Water, 1);
		let v = HotbarView::build(&hotbar, &inv, &ToolSwap::default());
		assert_eq!(
			v.slots[HOTBAR_SLOTS - 1],
			SlotView::Tool {
				tool: BlockType::Bucket,
				full: true,
				present: 1.0
			}
		);
	}

	/// Cycling tools trades one icon for the next the way the transmute
	/// tab's family tiles do: the outgoing one shrinks away to NOTHING,
	/// and only then does the incoming one exist — so the slot never
	/// shows two tools, and the rail never lists the one in the slot.
	#[test]
	fn the_tool_slot_hands_over_instead_of_cutting() {
		const DT: f32 = 1.0 / 60.0;
		let mut swap = ToolSwap::default();
		swap.advance(DT, BlockType::Bucket);
		assert_eq!(swap.shown(), BlockType::Bucket);
		assert_eq!(swap.present(), 1.0, "at rest, fully present");

		// The tap: the OLD tool is still what's drawn, and it shrinks.
		let mut lowest: f32 = 1.0;
		let mut shrinking = true;
		let mut flipped_at = None;
		for i in 0..24 {
			swap.advance(DT, BlockType::ResonantPick);
			let p = swap.present();
			if swap.shown() == BlockType::ResonantPick && flipped_at.is_none() {
				flipped_at = Some(i);
				shrinking = false;
			}
			if shrinking {
				assert!(p <= lowest, "the outgoing tool only shrinks");
			}
			lowest = lowest.min(p);
		}
		let flipped = flipped_at.expect("the exchange happens");
		assert!(lowest < 0.06, "it passes through nothing: {lowest}");
		assert!(
			flipped >= 6,
			"the outgoing half is a gesture, not one frame: {flipped}"
		);
		assert_eq!(swap.shown(), BlockType::ResonantPick);
		assert_eq!(swap.present(), 1.0, "and settles fully present");
	}

	/// Cycling a roster means tapping 9 repeatedly, so the second tap
	/// routinely lands while the first exchange is still growing its icon
	/// in. That must keep handing over, not cut: the icon on screen
	/// shrinks away first, however many taps arrive.
	#[test]
	fn a_tap_mid_handover_keeps_handing_over() {
		const DT: f32 = 1.0 / 60.0;
		let mut swap = ToolSwap::default();
		// Run until the pick is not just shown but visibly ON SCREEN.
		// Tapping exactly at the changeover proves nothing: presence is 0
		// there, which is the whole point of the changeover.
		let mut grew = false;
		for _ in 0..18 {
			swap.advance(DT, BlockType::ResonantPick);
			if swap.shown() == BlockType::ResonantPick && swap.present() > 0.4 {
				grew = true;
				break;
			}
		}
		assert!(grew, "reached the growing half with the icon visible");
		let at_tap = swap.present();
		assert!(at_tap < 1.0, "mid-growth, not settled: {at_tap}");

		// The second tap. Whatever is on screen must not be replaced by a
		// different icon at the same size.
		swap.advance(DT, BlockType::EssenceSiphon);
		assert_eq!(
			swap.shown(),
			BlockType::ResonantPick,
			"the visible icon stays put and shrinks — no swap in place"
		);
		assert!(
			swap.present() <= at_tap + 1e-4,
			"and it shrinks from where it was rather than jumping: \
			 {at_tap} -> {}",
			swap.present()
		);
		let mut lowest = swap.present();
		let mut flipped = None;
		for i in 0..24 {
			swap.advance(DT, BlockType::EssenceSiphon);
			lowest = lowest.min(swap.present());
			if swap.shown() == BlockType::EssenceSiphon && flipped.is_none() {
				flipped = Some(i);
			}
		}
		assert!(lowest < 0.06, "it still passes through nothing: {lowest}");
		assert!(flipped.is_some(), "and the siphon does arrive");
		assert_eq!(swap.shown(), BlockType::EssenceSiphon);
		assert_eq!(swap.present(), 1.0);
	}
}
