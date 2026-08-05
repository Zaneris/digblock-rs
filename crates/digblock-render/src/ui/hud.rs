//! In-game HUD emission: the hotbar row, built from widgets. Pure — the
//! engine calls this each frame with its `HotbarView`; the same pass
//! registers the `Bar(i)` hit rects the inventory screen uses as drop
//! targets, so the on-screen bar is always the truth.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{HotbarItem, HOTBAR_SLOTS};

use super::draw::Layer;
use super::widgets::{self, GridSpec, Ui, DIM, WHITE};
use crate::hotbar::{HotbarView, SlotView};

// Logical bases, × ui_scale.
pub const SLOT_PX: f32 = 56.0;
pub const SLOT_GAP: f32 = 4.0;
pub const ICON_PX: f32 = 48.0;
pub const BOTTOM_MARGIN: f32 = 12.0;

/// Zero-count slots stay visible but greyed (sticky slots never shift).
const GREY_TINT: u32 = 0xFF60_6060;
const FULL_TINT: u32 = 0xFFFF_FFFF;

/// What a 3D item icon renders as (see [`super::shaped_icon`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconKind {
	/// Plain block: a three-face dimetric cube.
	Cube(BlockType),
	/// Stairs/doors: the baked shape template, projected.
	Shaped(BlockType),
	/// The water tool: a little bucket with water inside.
	WaterTool,
}

/// The aim crosshair (hidden while any screen owns the cursor).
pub fn crosshair(dl: &mut super::DrawList, size: (u32, u32), scale: f32) {
	const COLOR: u32 = 0xC0FFFFFF;
	let arm = 9.0 * scale; // half-length in px
	let thick = 2.0 * scale;
	let (cx, cy) = (size.0 as f32 / 2.0, size.1 as f32 / 2.0);
	dl.quad(
		Layer::Backdrop,
		super::Rect::new(cx - arm, cy - thick / 2.0, arm * 2.0, thick),
		COLOR,
	);
	dl.quad(
		Layer::Backdrop,
		super::Rect::new(cx - thick / 2.0, cy - arm, thick, arm * 2.0),
		COLOR,
	);
}

/// Icon (kind, tint) for a hotbar item — the ghost drawn under the
/// cursor mid-drag uses the same visuals as the slots.
pub fn item_icon(item: HotbarItem) -> (IconKind, u32) {
	match item {
		HotbarItem::WaterTool => (IconKind::WaterTool, FULL_TINT),
		HotbarItem::Block(b) if b.props().shape.is_some() => (IconKind::Shaped(b), FULL_TINT),
		HotbarItem::Block(b) => (IconKind::Cube(b), FULL_TINT),
	}
}

/// Draw one 3D item icon into `rect` (every icon path goes through the
/// dimetric projector — one consistent visual language).
pub fn draw_icon(
	dl: &mut super::DrawList,
	layer: Layer,
	rect: super::Rect,
	kind: IconKind,
	tint: u32,
) {
	super::shaped_icon::emit(dl, layer, rect, kind, tint);
}

/// The hotbar row's grid: bottom-centered. Shared with the inventory
/// screen so its panel columns align with the bar.
pub fn row_grid(size: (u32, u32), scale: f32) -> GridSpec {
	let (cell, gap) = (SLOT_PX * scale, SLOT_GAP * scale);
	let total_w = HOTBAR_SLOTS as f32 * cell + (HOTBAR_SLOTS as f32 - 1.0) * gap;
	GridSpec {
		origin: (
			(size.0 as f32 - total_w) / 2.0,
			size.1 as f32 - BOTTOM_MARGIN * scale - cell,
		),
		cols: HOTBAR_SLOTS,
		cell,
		gap,
	}
}

/// Emit the hotbar: slot widgets (backdrops, icons, selection border,
/// hit rects under `id_of(i)`) plus stack counts. Counts stay at
/// [`super::scale::FIXED_GLYPH_PX`] regardless of scale — small by
/// design — with the count badge's drop shadow.
pub fn hotbar<Id: Copy + PartialEq>(
	ui: &mut Ui<'_, Id>,
	size: (u32, u32),
	view: &HotbarView,
	id_of: impl Fn(usize) -> Id,
) {
	let grid = row_grid(size, ui.style.scale);
	let icon_px = ICON_PX * ui.style.scale;
	for i in 0..HOTBAR_SLOTS {
		let rect = grid.rect(i);
		let icon = match view.slots[i] {
			SlotView::Empty => None,
			SlotView::Tool => Some(item_icon(HotbarItem::WaterTool)),
			SlotView::Block { block, count } => {
				let (kind, _) = item_icon(HotbarItem::Block(block));
				Some((kind, if count == 0 { GREY_TINT } else { FULL_TINT }))
			}
		};
		widgets::slot(ui, id_of(i), rect, icon, icon_px, i == view.selected);
		if let SlotView::Block { count, .. } = view.slots[i] {
			let color = if count == 0 { DIM } else { WHITE };
			widgets::count_badge(ui.dl, Layer::Content, rect, count, color);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::draw::{Cmd, DrawList};
	use crate::ui::hit::HitMap;
	use crate::ui::scale::FIXED_GLYPH_PX;
	use crate::ui::widgets::Style;
	use digblock_core::hotbar::Hotbar;
	use digblock_core::inventory::Inventory;

	const SIZE: (u32, u32) = (1280, 720);

	fn sample_view() -> HotbarView {
		let mut hotbar = Hotbar::new();
		hotbar.auto_assign(BlockType::Dirt);
		hotbar.auto_assign(BlockType::Stone);
		let mut inv = Inventory::default();
		inv.add(BlockType::Dirt, 3);
		HotbarView::build(&hotbar, &inv)
	}

	fn emit(scale: f32) -> (DrawList, HitMap<usize>) {
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: Style::new(scale),
		};
		hotbar(&mut ui, SIZE, &sample_view(), |i| i);
		(dl, hits)
	}

	#[test]
	fn row_is_bottom_centered_and_scales() {
		for scale in [1.0f32, 2.0] {
			let (_, hits) = emit(scale);
			let first = hits.rect_of(0).unwrap();
			let last = hits.rect_of(HOTBAR_SLOTS - 1).unwrap();
			let left = first.x;
			let right = SIZE.0 as f32 - (last.x + last.w);
			assert!((left - right).abs() < 0.5, "centered at {scale}x");
			assert_eq!(first.w, SLOT_PX * scale, "slot size scales");
			assert_eq!(
				first.y + first.h,
				SIZE.1 as f32 - BOTTOM_MARGIN * scale,
				"bottom anchor scales"
			);
		}
	}

	#[test]
	fn counts_stay_fixed_size_while_slots_scale() {
		const SHADOW: u32 = 0xE0000000;
		let (dl, _) = emit(2.0);
		let (cmds, arena) = dl.cmds(Layer::Content);
		let texts: Vec<(&str, f32, u32)> = cmds
			.iter()
			.filter_map(|c| match c {
				Cmd::Text {
					px, range, color, ..
				} => Some((&arena[range.0 as usize..range.1 as usize], *px, *color)),
				_ => None,
			})
			.collect();
		// Dirt count 3 and Stone count 0 print (tool and empties don't),
		// each as a shadow + digit pair.
		let (shadows, counts): (Vec<_>, Vec<_>) = texts.iter().partition(|(_, _, c)| *c == SHADOW);
		assert_eq!(counts.len(), 2);
		assert_eq!(shadows.len(), 2, "every count carries a drop shadow");
		for (text, px, _) in &texts {
			assert_eq!(*px, FIXED_GLYPH_PX, "count {text} must not scale");
		}
		assert!(counts.iter().any(|(t, _, _)| *t == "3"));
		assert!(counts.iter().any(|(t, _, _)| *t == "0"));
	}

	#[test]
	fn icons_and_selection_ride_the_right_layers() {
		let (dl, _) = emit(1.0);
		// Dirt cube (3 faces), Stone cube greyed (3), tool bucket (7).
		let icons = dl.cmds(Layer::Icons).0;
		assert_eq!(icons.len(), 13);
		assert!(icons.iter().all(|c| matches!(c, Cmd::IconQuad { .. })));
		// Backdrop: 9 slot bgs + 4 selection border quads.
		assert_eq!(dl.cmds(Layer::Backdrop).0.len(), HOTBAR_SLOTS + 4);
		// The greyed stone cube: its top face carries GREY_TINT verbatim
		// (top shade factor is 1.0), and no full-tint face.
		let stone: Vec<u32> = icons[3..6]
			.iter()
			.map(|c| match c {
				Cmd::IconQuad { tint, .. } => *tint,
				_ => unreachable!(),
			})
			.collect();
		assert!(stone.contains(&GREY_TINT), "greyed top face: {stone:?}");
		assert!(!stone.contains(&FULL_TINT));
	}
}
