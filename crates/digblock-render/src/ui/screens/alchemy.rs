//! The alchemy screen: break whole blocks down into sub-material units
//! (1 block = 27, a 3×3×3 division) and craft sub-block shaped items
//! (stairs, doors) from them. Pure click UI — no drags. The screen never
//! mutates game state: `handle_input` returns actions; the engine
//! applies them locally offline or sends them to the server online (the
//! `InventoryFull` echo is the rebase), and locally gates affordability
//! so honest clients never send failing ops.

use digblock_core::block::{BlockType, Material};
use digblock_core::inventory::Inventory;
use digblock_core::shapes::{craft_recipe, CRAFTABLE, SUBS_PER_BLOCK};
use digblock_platform::{InputEvent, Key, MouseButton};

use crate::ui::draw::Layer;
use crate::ui::hud;
use crate::ui::widgets::{self, Style, Ui, DIM, WHITE};
use crate::ui::{HitMap, InvId, Rect};

/// One requested alchemy operation, in core vocabulary.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AlchAction {
	Convert { block: BlockType, count: u32 },
	Craft { item: BlockType, count: u32 },
}

/// Shift-click batch size (plain click = 1).
const BULK: u32 = 10;

const PANEL_BG: u32 = 0xC0101010;
const SCREEN_DIM: u32 = 0x70000000;

/// The screen's per-frame view of the inventory, built by the engine
/// from the same `Inventory` the actions will be validated against.
pub struct AlchView {
	/// Blocks with a `sub_material` mapping the player holds.
	pub convertibles: Vec<(BlockType, u32)>,
	/// Sub-material holdings, `Material::ALL` order.
	pub subs: [u32; Material::COUNT],
}

impl AlchView {
	pub fn build(inv: &Inventory) -> Self {
		Self {
			convertibles: inv
				.iter_nonzero()
				.filter(|(b, _)| b.sub_material().is_some())
				.collect(),
			subs: [
				inv.sub_count(Material::Wood),
				inv.sub_count(Material::Stone),
			],
		}
	}

	/// Whole batches of `CRAFTABLE[i]` the holdings cover.
	fn affordable(&self, i: usize) -> bool {
		let r = craft_recipe(CRAFTABLE[i]).expect("CRAFTABLE rows have recipes");
		self.subs[r.material as usize] >= r.cost
	}
}

#[derive(Default)]
pub struct AlchScreen {
	open: bool,
	cursor: (f32, f32),
	shift: bool,
}

impl AlchScreen {
	pub fn is_open(&self) -> bool {
		self.open
	}

	pub fn open(&mut self) {
		self.open = true;
		self.shift = false;
	}

	pub fn close(&mut self) {
		self.open = false;
	}

	/// Emit the screen: dim, panel, the three sections (holdings with
	/// convert buttons, sub-material readouts, craft rows).
	pub fn build(&self, ui: &mut Ui<'_, InvId>, size: (u32, u32), view: &AlchView) {
		let scale = ui.style.scale;
		let glyph = ui.style.glyph;
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			SCREEN_DIM,
		);
		let pad = 12.0 * scale;
		let row_h = 26.0 * scale;
		let btn_w = 120.0 * scale;
		let icon_px = hud::ICON_PX * scale;
		let panel_w = 420.0_f32.min(size.0 as f32 - 2.0 * pad) * scale.max(1.0);
		let rows = view.convertibles.len().max(1) + CRAFTABLE.len();
		// Title + section headers (3) + rows + the sub-material line.
		let panel_h = (rows as f32 + 1.0) * row_h + 4.0 * (glyph + 6.0 * scale) + pad * 3.0;
		let panel = Rect::new(
			(size.0 as f32 - panel_w) / 2.0,
			((size.1 as f32 - panel_h) / 2.0).max(pad),
			panel_w,
			panel_h,
		);
		widgets::panel(ui.dl, Layer::Backdrop, panel, PANEL_BG);
		let mut y = panel.y + pad / 2.0;
		let x = panel.x + pad;
		widgets::label(ui.dl, Layer::Content, x, y, glyph, WHITE, "ALCHEMY");
		y += glyph + 6.0 * scale;

		// Section: convertible holdings.
		widgets::label(
			ui.dl,
			Layer::Content,
			x,
			y,
			glyph,
			DIM,
			"BREAK DOWN (27/BLOCK)",
		);
		y += glyph + 6.0 * scale;
		if view.convertibles.is_empty() {
			widgets::label(ui.dl, Layer::Content, x, y, glyph, DIM, "no raw materials");
			y += row_h;
		}
		for (i, &(block, count)) in view.convertibles.iter().enumerate() {
			let (tex, tint) = hud::item_icon(digblock_core::hotbar::HotbarItem::Block(block));
			ui.dl.icon(
				Layer::Icons,
				Rect::new(x, y + (row_h - icon_px) / 2.0, icon_px, icon_px),
				tex,
				tint,
			);
			widgets::label(
				ui.dl,
				Layer::Content,
				x + icon_px + 8.0 * scale,
				y + (row_h - glyph) / 2.0,
				glyph,
				WHITE,
				&format!("{count}"),
			);
			widgets::button(
				ui,
				InvId::Convert(i as u8),
				Rect::new(
					panel.x + panel.w - pad - btn_w,
					y + 2.0 * scale,
					btn_w,
					row_h - 4.0 * scale,
				),
				"CONVERT",
				count > 0,
			);
			y += row_h;
		}

		// Section: sub-material readouts.
		widgets::label(
			ui.dl,
			Layer::Content,
			x,
			y,
			glyph,
			DIM,
			&format!("SUB-WOOD {}  SUB-STONE {}", view.subs[0], view.subs[1]),
		);
		y += glyph + 6.0 * scale;

		// Section: craft rows.
		widgets::label(ui.dl, Layer::Content, x, y, glyph, DIM, "CRAFT");
		y += glyph + 6.0 * scale;
		for (i, &item) in CRAFTABLE.iter().enumerate() {
			let r = craft_recipe(item).expect("CRAFTABLE rows have recipes");
			let (tex, tint) = hud::item_icon(digblock_core::hotbar::HotbarItem::Block(item));
			ui.dl.icon(
				Layer::Icons,
				Rect::new(x, y + (row_h - icon_px) / 2.0, icon_px, icon_px),
				tex,
				tint,
			);
			let mat = match r.material {
				Material::Wood => "WOOD",
				Material::Stone => "STONE",
			};
			widgets::label(
				ui.dl,
				Layer::Content,
				x + icon_px + 8.0 * scale,
				y + (row_h - glyph) / 2.0,
				glyph,
				if view.affordable(i) { WHITE } else { DIM },
				&format!("{} {mat}", r.cost),
			);
			widgets::button(
				ui,
				InvId::Craft(i as u8),
				Rect::new(
					panel.x + panel.w - pad - btn_w,
					y + 2.0 * scale,
					btn_w,
					row_h - 4.0 * scale,
				),
				"CRAFT",
				view.affordable(i),
			);
			y += row_h;
		}
		widgets::label(
			ui.dl,
			Layer::Content,
			x,
			y + 4.0 * scale,
			glyph,
			DIM,
			"CLICK = 1   SHIFT-CLICK = 10",
		);
	}

	/// Feed one input event; returns the action the engine should apply
	/// (already clamped to what the holdings afford).
	pub fn handle_input(
		&mut self,
		event: &InputEvent,
		hits: &HitMap<InvId>,
		_style: &Style,
		view: &AlchView,
	) -> Option<AlchAction> {
		match *event {
			InputEvent::CursorMoved { x, y } => {
				self.cursor = (x, y);
				None
			}
			InputEvent::Key {
				key: Key::Shift,
				pressed,
			} => {
				self.shift = pressed;
				None
			}
			InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			} => {
				let id = hits.hit(self.cursor.0, self.cursor.1)?;
				let want = if self.shift { BULK } else { 1 };
				match id {
					InvId::Convert(i) => {
						let &(block, have) = view.convertibles.get(i as usize)?;
						let count = want.min(have);
						(count > 0).then_some(AlchAction::Convert { block, count })
					}
					InvId::Craft(i) => {
						let item = *CRAFTABLE.get(i as usize)?;
						let r = craft_recipe(item)?;
						let batches = view.subs[r.material as usize] / r.cost;
						let count = want.min(batches);
						(count > 0).then_some(AlchAction::Craft { item, count })
					}
					_ => None,
				}
			}
			InputEvent::FocusLost => {
				self.shift = false;
				None
			}
			_ => None,
		}
	}
}

/// The conversion yield, re-exported for the engine's HUD hints.
pub const fn subs_per_block() -> u32 {
	SUBS_PER_BLOCK
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::draw::DrawList;
	use digblock_core::block::BlockType as B;

	const SIZE: (u32, u32) = (1280, 800);

	fn rig(inv: &Inventory) -> (AlchScreen, HitMap<InvId>, AlchView, Style) {
		let view = AlchView::build(inv);
		let style = Style::new(1.0);
		let mut screen = AlchScreen::default();
		screen.open();
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style,
		};
		screen.build(&mut ui, SIZE, &view);
		(screen, hits, view, style)
	}

	fn click(
		screen: &mut AlchScreen,
		hits: &HitMap<InvId>,
		style: &Style,
		view: &AlchView,
		id: InvId,
		shift: bool,
	) -> Option<AlchAction> {
		let r = hits.rect_of(id).expect("button exists");
		let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
		let feed = |s: &mut AlchScreen, ev: &InputEvent| s.handle_input(ev, hits, style, view);
		if shift {
			feed(
				screen,
				&InputEvent::Key {
					key: Key::Shift,
					pressed: true,
				},
			);
		}
		feed(screen, &InputEvent::CursorMoved { x: cx, y: cy });
		let act = feed(
			screen,
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
		);
		if shift {
			feed(
				screen,
				&InputEvent::Key {
					key: Key::Shift,
					pressed: false,
				},
			);
		}
		act
	}

	#[test]
	fn convert_click_and_shift_bulk_clamp() {
		let mut inv = Inventory::default();
		inv.add(B::Wood, 3);
		let (mut s, hits, view, style) = rig(&inv);
		assert_eq!(
			click(&mut s, &hits, &style, &view, InvId::Convert(0), false),
			Some(AlchAction::Convert {
				block: B::Wood,
				count: 1
			})
		);
		// Shift asks for 10, holdings clamp to 3.
		assert_eq!(
			click(&mut s, &hits, &style, &view, InvId::Convert(0), true),
			Some(AlchAction::Convert {
				block: B::Wood,
				count: 3
			})
		);
	}

	#[test]
	fn craft_gated_on_affordability() {
		// 54 sub-wood: three wood-stair batches (18 each), zero stone.
		let mut inv = Inventory::default();
		inv.add_sub(digblock_core::block::Material::Wood, 54);
		let (mut s, hits, view, style) = rig(&inv);
		assert_eq!(CRAFTABLE[0], B::WoodStairs);
		assert_eq!(
			click(&mut s, &hits, &style, &view, InvId::Craft(0), true),
			Some(AlchAction::Craft {
				item: B::WoodStairs,
				count: 3
			}),
			"shift clamps to affordable batches"
		);
		// Stone stairs unaffordable: the button registered no hit rect.
		assert!(hits.rect_of(InvId::Craft(1)).is_none());
	}

	#[test]
	fn empty_inventory_builds_with_no_actionable_hits() {
		let inv = Inventory::default();
		let (_s, hits, _view, _style) = rig(&inv);
		assert!(hits.rect_of(InvId::Convert(0)).is_none());
		assert!(hits.rect_of(InvId::Craft(0)).is_none());
		assert!(hits.rect_of(InvId::Craft(2)).is_none());
	}
}
