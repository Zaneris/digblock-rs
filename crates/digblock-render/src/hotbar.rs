//! Hotbar HUD view: a pure snapshot of (hotbar assignments × inventory
//! counts) built once per frame. Emission into draw commands lives in
//! `ui/hud.rs`; the icon pipeline in `ui/icons.rs`.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{Hotbar, HotbarItem, HOTBAR_SLOTS};
use digblock_core::inventory::Inventory;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SlotView {
	Empty,
	/// The water tool and whether it's charged — the bucket's own count,
	/// so the bar shows the charge where a stack would show its number.
	Tool {
		full: bool,
	},
	Block {
		block: BlockType,
		count: u32,
	},
}

pub struct HotbarView {
	pub slots: [SlotView; HOTBAR_SLOTS],
	pub selected: usize,
}

impl HotbarView {
	pub fn build(hotbar: &Hotbar, inventory: &Inventory) -> Self {
		let mut slots = [SlotView::Empty; HOTBAR_SLOTS];
		for (i, s) in hotbar.slots().iter().enumerate() {
			slots[i] = match s {
				None => SlotView::Empty,
				Some(HotbarItem::WaterTool) => SlotView::Tool {
					full: inventory.bucket_full(),
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
		let v = HotbarView::build(&hotbar, &inv);
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
			SlotView::Tool { full: false },
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
		let v = HotbarView::build(&hotbar, &inv);
		assert_eq!(v.slots[HOTBAR_SLOTS - 1], SlotView::Tool { full: true });
	}
}
