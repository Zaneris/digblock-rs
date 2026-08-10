//! In-game HUD emission: the hotbar row, built from widgets. Pure — the
//! engine calls this each frame with its `HotbarView`; the same pass
//! registers the `Bar(i)` hit rects the inventory screen uses as drop
//! targets, so the on-screen bar is always the truth.

use digblock_core::block::BlockType;
use digblock_core::hotbar::{HotbarItem, HOTBAR_SLOTS};

use super::draw::Layer;
use super::widgets::{self, GridSpec, Ui, WHITE};
use crate::hotbar::{HotbarView, SlotView};

// Logical bases, × ui_scale.
pub const SLOT_PX: f32 = 56.0;
pub const SLOT_GAP: f32 = 4.0;
pub const ICON_PX: f32 = 48.0;
pub const BOTTOM_MARGIN: f32 = 12.0;

const FULL_TINT: u32 = 0xFFFF_FFFF;

/// What a 3D item icon renders as (see [`super::shaped_icon`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconKind {
	/// Plain block: a three-face dimetric cube.
	Cube(BlockType),
	/// Stairs/doors: the baked shape template, projected.
	Shaped(BlockType),
	/// The water tool: a little wooden bucket, with a source in it or
	/// not. The charge is drawn, not tinted — "have I got water?" is the
	/// question the bar has to answer at a glance.
	WaterTool { full: bool },
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
///
/// `tool_full` is the water tool's charge, which is player state rather
/// than part of the slot assignment, so it has to arrive alongside the
/// item; every other item ignores it.
pub fn item_icon(item: HotbarItem, tool_full: bool) -> (IconKind, u32) {
	match item {
		HotbarItem::WaterTool => (IconKind::WaterTool { full: tool_full }, FULL_TINT),
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
/// hit rects under `id_of(i)`) plus stack counts, drawn at the frame's
/// text size like the rest of the UI.
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
			SlotView::Tool { full } => Some(item_icon(HotbarItem::WaterTool, full)),
			SlotView::Block { block, .. } => Some(item_icon(HotbarItem::Block(block), false)),
		};
		// The HUD bar has no cursor: rings only, never hover.
		let ring = if i == view.selected {
			widgets::SlotRing::Strong
		} else {
			widgets::SlotRing::None
		};
		widgets::slot(ui, id_of(i), rect, icon, icon_px, ring, false);
		if let SlotView::Block { count, .. } = view.slots[i] {
			widgets::count_badge(ui.dl, Layer::Content, &ui.style, rect, count, WHITE);
		}
	}
}

/// §6's faded tool rail: the owned tools OTHER than the active one,
/// drawn as small reduced-alpha icons just right of the tool slot, in
/// the roster's own deterministic order. A READOUT, not a target — no
/// hit rects; tapping 9 on the selected tool slot is the interaction.
/// With a single owned tool nothing draws at all.
pub fn tool_rail(
	dl: &mut super::DrawList,
	style: &super::widgets::Style,
	size: (u32, u32),
	owned: &[digblock_core::block::BlockType],
	active: digblock_core::block::BlockType,
	bucket_full: bool,
) {
	use digblock_core::block::BlockType;
	if owned.len() < 2 {
		return;
	}
	const RAIL_TINT: u32 = 0x70FF_FFFF;
	let scale = style.scale;
	let grid = row_grid(size, scale);
	let cell = 28.0 * scale;
	let gap = 6.0 * scale;
	let mut x = grid.origin.0 + grid.extent(HOTBAR_SLOTS).0 + SLOT_GAP * scale * 2.0;
	let y = grid.origin.1 + grid.cell - cell;
	for &tool in owned.iter().filter(|&&t| t != active) {
		let kind = match tool {
			BlockType::Bucket => IconKind::WaterTool { full: bucket_full },
			// Future tools without authored icons read as their cube.
			other => IconKind::Cube(other),
		};
		draw_icon(
			dl,
			Layer::Icons,
			super::Rect::new(x, y, cell, cell),
			kind,
			RAIL_TINT,
		);
		x += cell + gap;
	}
}

/// One coalescing XP toast: rapid awards to the same skill accumulate
/// into a single "+N SKILL" line instead of a scrolling storm. Engine
/// state — [`xp_toasts`] just draws whatever is live.
pub struct XpToast {
	pub skill: digblock_core::skills::Skill,
	pub amount: u64,
	/// Seconds left; refreshed to [`XP_TOAST_TTL`] by each new award.
	pub ttl: f32,
}

pub const XP_TOAST_TTL: f32 = 2.0;
/// The fade window at the end of a toast's life.
const XP_TOAST_FADE: f32 = 0.5;

/// Scale a 0xAABBGGRR color's alpha byte.
fn faded(color: u32, f: f32) -> u32 {
	let a = ((color >> 24) as f32 * f.clamp(0.0, 1.0)) as u32;
	a << 24 | (color & 0x00FF_FFFF)
}

/// Draw the live XP toasts, stacked upward above the hotbar, centered
/// like the bar itself. Text rides the frame's size; the backdrop is
/// sized from the face's line height (ink legitimately overflows the
/// nominal box — accents above, descenders below).
pub fn xp_toasts(
	dl: &mut super::DrawList,
	style: &super::widgets::Style,
	size: (u32, u32),
	toasts: &[XpToast],
) {
	let scale = style.scale;
	let px = style.glyph;
	let line = crate::font::face(style.face).line_h(px);
	let pad_x = super::scale::text_pad_x(scale);
	let gap = 4.0 * scale;
	let mut bottom = row_grid(size, scale).origin.1 - super::theme::GAP_ABOVE_BAR * scale;
	for toast in toasts.iter().rev() {
		let fade = (toast.ttl / XP_TOAST_FADE).clamp(0.0, 1.0);
		let text = format!("+{} {}", toast.amount, toast.skill.name());
		let w = style.measure(&text) + 2.0 * pad_x;
		let rect = super::Rect::new((size.0 as f32 - w) / 2.0, bottom - line, w, line);
		dl.quad(Layer::Backdrop, rect, faded(super::theme::FILL_BG, fade));
		dl.text(
			Layer::Content,
			rect.x + pad_x,
			rect.y + (line - px) / 2.0,
			px,
			faded(WHITE, fade),
			&text,
		);
		bottom = rect.y - gap;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::draw::{Cmd, DrawList};
	use crate::ui::hit::HitMap;
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
		inv.add(BlockType::Stone, 12);
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

	/// Counts used to be pinned at `scale::FIXED_GLYPH_PX` while the slots
	/// around them scaled, so on a dense display a 32px UI carried 16px
	/// stack numbers. They ride the frame's text size now.
	#[test]
	fn counts_scale_with_the_slots() {
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
		// Dirt count 3 and Stone count 12 print (tool and empties don't),
		// each as a shadow + digit pair.
		let (shadows, counts): (Vec<_>, Vec<_>) = texts.iter().partition(|(_, _, c)| *c == SHADOW);
		assert_eq!(counts.len(), 2);
		assert_eq!(shadows.len(), 2, "every count carries a drop shadow");
		let want = Style::new(2.0).glyph;
		for (text, px, _) in &texts {
			assert_eq!(*px, want, "count {text} should ride the frame's size");
		}
		assert!(counts.iter().any(|(t, _, _)| *t == "3"));
		assert!(counts.iter().any(|(t, _, _)| *t == "12"));
	}

	#[test]
	fn icons_and_selection_ride_the_right_layers() {
		let (dl, _) = emit(1.0);
		// Two cubes at three visible faces each, then the tool bucket.
		let icons = dl.cmds(Layer::Icons).0;
		assert!(icons.iter().all(|c| matches!(c, Cmd::IconQuad { .. })));
		assert!(icons.len() > 6, "two cubes plus a bucket");
		// Backdrop: 9 slot bgs + 4 selection border quads.
		assert_eq!(dl.cmds(Layer::Backdrop).0.len(), HOTBAR_SLOTS + 4);
		// Every stocked slot draws at full tint — the grey zero-count
		// state is gone; an exhausted type leaves the bar entirely.
		let dirt: Vec<u32> = icons[..3]
			.iter()
			.map(|c| match c {
				Cmd::IconQuad { tint, .. } => *tint,
				_ => unreachable!(),
			})
			.collect();
		assert!(dirt.contains(&FULL_TINT), "top face is untinted: {dirt:?}");
	}
}
