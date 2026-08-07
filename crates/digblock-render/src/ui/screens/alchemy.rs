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

// Layout bases (logical px × ui scale). ROW_H fits ROW_ICON with room;
// the button sits inside the row.
const ROW_H: f32 = 40.0;
const ROW_ICON: f32 = 32.0;
const BTN_W: f32 = 152.0;
const BTN_H: f32 = 32.0;
const PAD: f32 = 12.0;
const GAP_ABOVE_BAR: f32 = 24.0;
const HDR_GAP: f32 = 8.0;
const ICON_TEXT_GAP: f32 = 8.0;
const TEXT_BTN_GAP: f32 = 16.0;

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

/// One row of the screen, shared by the measure and emit passes.
enum Row {
	Title,
	/// Section headers, readouts, and hints — one dim text line.
	Header(String),
	Convert {
		i: usize,
		block: BlockType,
		count: u32,
	},
	Craft {
		i: usize,
		item: BlockType,
		cost: u32,
		mat: &'static str,
		ok: bool,
	},
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

	/// The screen's row stream, built once per frame — the measure pass
	/// and the emit pass walk the SAME rows, so the panel can never
	/// disagree with its content.
	fn rows(view: &AlchView) -> Vec<Row> {
		let mut rows = vec![Row::Title, Row::Header("EXTRACTION (1 BLOCK = 27)".into())];
		if view.convertibles.is_empty() {
			rows.push(Row::Header("no raw materials".into()));
		}
		for (i, &(block, count)) in view.convertibles.iter().enumerate() {
			rows.push(Row::Convert { i, block, count });
		}
		rows.push(Row::Header(format!(
			"WOOD {}   STONE {}",
			view.subs[0], view.subs[1]
		)));
		rows.push(Row::Header("TRANSMUTATION".into()));
		for (i, &item) in CRAFTABLE.iter().enumerate() {
			let r = craft_recipe(item).expect("CRAFTABLE rows have recipes");
			rows.push(Row::Craft {
				i,
				item,
				cost: r.cost,
				mat: match r.material {
					Material::Wood => "WOOD",
					Material::Stone => "STONE",
				},
				ok: view.affordable(i),
			});
		}
		rows.push(Row::Header("CLICK = 1   SHIFT-CLICK = 10".into()));
		rows
	}

	/// Emit the screen: dim, panel, the sections (holdings with convert
	/// buttons, sub-material readouts, craft rows). Two passes over one
	/// row stream — measure (panel sizes to its widest row and summed
	/// heights, scale applied exactly once), then emit. The panel
	/// bottom-anchors above the hotbar so the bar never overlaps it.
	/// `daylight` fades the full-screen dim only — the panel keeps its
	/// backdrop.
	pub fn build(&self, ui: &mut Ui<'_, InvId>, size: (u32, u32), view: &AlchView, daylight: f32) {
		let s = ui.style.scale;
		let glyph = ui.style.glyph;
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			widgets::scaled_dim(SCREEN_DIM, daylight),
		);
		let rows = Self::rows(view);
		let pad = PAD * s;
		let text_of = |row: &Row| -> String {
			match row {
				Row::Title => "ALCHEMY".into(),
				Row::Header(t) => t.clone(),
				Row::Convert { count, .. } => format!("{count}"),
				Row::Craft { cost, mat, .. } => format!("{cost} {mat}"),
			}
		};
		// Measure: widest row and summed advances.
		let (mut width, mut height) = (0.0f32, 0.0f32);
		for row in &rows {
			let text_w = ui.style.measure(&text_of(row));
			let (w, h) = match row {
				Row::Title | Row::Header(_) => (text_w, glyph + HDR_GAP * s),
				Row::Convert { .. } | Row::Craft { .. } => (
					(ROW_ICON + ICON_TEXT_GAP + TEXT_BTN_GAP + BTN_W) * s + text_w,
					ROW_H * s,
				),
			};
			width = width.max(w);
			height += h;
		}
		let panel_w = (width + 2.0 * pad).min(size.0 as f32 - 2.0 * pad);
		let panel_h = height + 2.0 * pad;
		let bar_top = hud::row_grid(size, s).origin.1;
		let panel = Rect::new(
			(size.0 as f32 - panel_w) / 2.0,
			(bar_top - GAP_ABOVE_BAR * s - panel_h).max(pad),
			panel_w,
			panel_h,
		);
		widgets::panel(ui.dl, Layer::Backdrop, panel, PANEL_BG);

		// Emit.
		let x = panel.x + pad;
		let mut y = panel.y + pad;
		let (cx, cy) = self.cursor;
		for row in &rows {
			let text = text_of(row);
			match *row {
				Row::Title => {
					widgets::label(ui.dl, Layer::Content, x, y, glyph, WHITE, &text);
					y += glyph + HDR_GAP * s;
				}
				Row::Header(_) => {
					widgets::label(ui.dl, Layer::Content, x, y, glyph, DIM, &text);
					y += glyph + HDR_GAP * s;
				}
				Row::Convert { .. } | Row::Craft { .. } => {
					let (block, actionable, id, btn_text, text_color) = match *row {
						Row::Convert { i, block, count } => {
							(block, count > 0, InvId::Convert(i as u8), "EXTRACT", WHITE)
						}
						Row::Craft { i, item, ok, .. } => (
							item,
							ok,
							InvId::Craft(i as u8),
							"TRANSMUTE",
							if ok { WHITE } else { DIM },
						),
						_ => unreachable!(),
					};
					let row_rect = Rect::new(x, y, panel.w - 2.0 * pad, ROW_H * s);
					if actionable && row_rect.contains(cx, cy) {
						ui.dl.quad(Layer::Backdrop, row_rect, widgets::ROW_HILIGHT);
					}
					let icon = ROW_ICON * s;
					let (kind, tint) =
						hud::item_icon(digblock_core::hotbar::HotbarItem::Block(block));
					hud::draw_icon(
						ui.dl,
						Layer::Icons,
						Rect::new(x, y + (ROW_H * s - icon) / 2.0, icon, icon),
						kind,
						tint,
					);
					widgets::label(
						ui.dl,
						Layer::Content,
						x + icon + ICON_TEXT_GAP * s,
						y + (ROW_H * s - glyph) / 2.0,
						glyph,
						text_color,
						&text,
					);
					let btn = Rect::new(
						row_rect.x + row_rect.w - BTN_W * s,
						y + (ROW_H - BTN_H) / 2.0 * s,
						BTN_W * s,
						BTN_H * s,
					);
					widgets::button(ui, id, btn, btn_text, actionable, btn.contains(cx, cy));
					y += ROW_H * s;
				}
			}
		}
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
		screen.build(&mut ui, SIZE, &view, 1.0);
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

	fn full_inv() -> Inventory {
		let mut inv = Inventory::default();
		inv.add(B::Wood, 5);
		inv.add(B::Stone, 11);
		inv.add_sub(digblock_core::block::Material::Wood, 54);
		inv
	}

	/// The panel's PANEL_BG quad at a given scale/window.
	fn panel_rect(inv: &Inventory, size: (u32, u32), scale: f32) -> Rect {
		let view = AlchView::build(inv);
		let mut screen = AlchScreen::default();
		screen.open();
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: Style::new(scale),
		};
		screen.build(&mut ui, size, &view, 1.0);
		let (cmds, _) = dl.cmds(Layer::Backdrop);
		cmds.iter()
			.filter_map(|c| match c {
				crate::ui::draw::Cmd::Quad { rect, color } if *color == PANEL_BG => Some(*rect),
				_ => None,
			})
			.next()
			.expect("panel quad")
	}

	/// The old screen centered vertically and ran under the hotbar; the
	/// redesign bottom-anchors above it.
	#[test]
	fn panel_sits_above_the_hotbar() {
		let panel = panel_rect(&full_inv(), SIZE, 1.0);
		let bar_top = hud::row_grid(SIZE, 1.0).origin.1;
		assert!(
			panel.y + panel.h <= bar_top - GAP_ABOVE_BAR + 0.5,
			"panel bottom {} vs bar top {bar_top}",
			panel.y + panel.h
		);
	}

	/// The old panel width multiplied an already-clamped pixel width by
	/// the scale; the redesign applies scale exactly once.
	#[test]
	fn panel_scales_once() {
		let one = panel_rect(&full_inv(), SIZE, 1.0);
		let two = panel_rect(&full_inv(), (2560, 1600), 2.0);
		let ratio = two.w / one.w;
		assert!(
			(1.8..=2.2).contains(&ratio),
			"2x window/scale ≈ 2x panel width, got {ratio} ({} vs {})",
			one.w,
			two.w
		);
	}

	/// Rows were 26px tall with 48px icons stamped into them; now every
	/// button row is disjoint from its neighbors.
	#[test]
	fn rows_do_not_overlap() {
		let inv = full_inv();
		let (_s, hits, _view, _style) = rig(&inv);
		let mut rects: Vec<Rect> = [
			hits.rect_of(InvId::Convert(0)),
			hits.rect_of(InvId::Convert(1)),
			hits.rect_of(InvId::Craft(0)),
		]
		.into_iter()
		.flatten()
		.collect();
		assert!(rects.len() >= 3, "expected actionable rows");
		rects.sort_by(|a, b| a.y.total_cmp(&b.y));
		for pair in rects.windows(2) {
			assert!(
				pair[0].y + pair[0].h <= pair[1].y + 0.5,
				"buttons overlap: {pair:?}"
			);
		}
	}

	/// Hovering an affordable row paints the shared highlight; moving
	/// the cursor away removes it.
	#[test]
	fn hover_highlights_only_under_the_cursor() {
		let inv = full_inv();
		let (mut screen, hits, view, style) = rig(&inv);
		let target = hits.rect_of(InvId::Craft(0)).expect("affordable craft");
		let hilight_count = |screen: &AlchScreen| {
			let mut dl = DrawList::default();
			let mut h2 = HitMap::default();
			let mut ui = Ui {
				dl: &mut dl,
				hits: &mut h2,
				style,
			};
			screen.build(&mut ui, SIZE, &view, 1.0);
			dl.cmds(Layer::Backdrop)
				.0
				.iter()
				.filter(|c| {
					matches!(
						c,
						crate::ui::draw::Cmd::Quad { color, .. }
							if *color == widgets::ROW_HILIGHT
					)
				})
				.count()
		};
		screen.handle_input(
			&InputEvent::CursorMoved {
				x: target.x + target.w / 2.0,
				y: target.y + target.h / 2.0,
			},
			&hits,
			&style,
			&view,
		);
		// Row highlight + button highlight under the cursor.
		assert_eq!(hilight_count(&screen), 2, "hovered row + button");
		screen.handle_input(
			&InputEvent::CursorMoved { x: 1.0, y: 1.0 },
			&hits,
			&style,
			&view,
		);
		assert_eq!(hilight_count(&screen), 0, "nothing hovered");
	}
}
