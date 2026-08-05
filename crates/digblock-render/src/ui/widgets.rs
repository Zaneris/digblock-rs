//! Small composable widgets: pure functions that emit draw commands and
//! register hit regions in one pass. Nothing here holds state — screens
//! own their state machines and call these per frame.

use super::draw::{DrawList, Layer};
use super::hit::HitMap;
use super::{scale, Rect};

// Shared palette (0xAABBGGRR, matching overlay.rs).
pub const WHITE: u32 = 0xFFFFFFFF;
pub const DIM: u32 = 0xFFB0B0B0;
pub const SLOT_BG: u32 = 0x90000000;
const SEL_BORDER: u32 = 0xE0FFFFFF;

/// Per-frame style: the UI scale and its derived (integer-snapped)
/// glyph size.
#[derive(Clone, Copy)]
pub struct Style {
	pub scale: f32,
	pub glyph: f32,
}

impl Style {
	pub fn new(ui_scale: f32) -> Self {
		Self {
			scale: ui_scale,
			glyph: scale::glyph_px(ui_scale),
		}
	}
}

/// The bundle widgets draw through: commands, hit regions, style.
pub struct Ui<'a, Id> {
	pub dl: &'a mut DrawList,
	pub hits: &'a mut HitMap<Id>,
	pub style: Style,
}

/// A panel backdrop (no hit region — panels aren't interactive).
pub fn panel(dl: &mut DrawList, layer: Layer, rect: Rect, color: u32) {
	dl.quad(layer, rect, color);
}

pub fn label(dl: &mut DrawList, layer: Layer, x: f32, y: f32, px: f32, color: u32, text: &str) {
	dl.text(layer, x, y, px, color, text);
}

/// An item slot: backdrop quad + optional centered icon + optional
/// selection border, hit-testable under `id`. Count text is the
/// caller's job (the hotbar pins its size; other screens scale).
pub fn slot<Id: Copy + PartialEq>(
	ui: &mut Ui<'_, Id>,
	id: Id,
	rect: Rect,
	icon: Option<(u16, u32)>,
	icon_px: f32,
	selected: bool,
) {
	ui.dl.quad(Layer::Backdrop, rect, SLOT_BG);
	if selected {
		let t = (2.0 * ui.style.scale).round().max(1.0);
		let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
		let dl = &mut *ui.dl;
		dl.quad(
			Layer::Backdrop,
			Rect::new(x - t, y - t, w + 2.0 * t, t),
			SEL_BORDER,
		);
		dl.quad(
			Layer::Backdrop,
			Rect::new(x - t, y + h, w + 2.0 * t, t),
			SEL_BORDER,
		);
		dl.quad(Layer::Backdrop, Rect::new(x - t, y, t, h), SEL_BORDER);
		dl.quad(Layer::Backdrop, Rect::new(x + w, y, t, h), SEL_BORDER);
	}
	if let Some((tex_layer, tint)) = icon {
		ui.dl.icon(
			Layer::Icons,
			rect.centered(icon_px, icon_px),
			tex_layer,
			tint,
		);
	}
	ui.hits.push(rect, id);
}

/// Stack-count badge in a slot's bottom-right corner: pinned at
/// [`scale::FIXED_GLYPH_PX`] (small by design), "99+" past two digits,
/// with a one-font-pixel drop shadow so digits stay readable on light
/// icons (snow, sand).
pub fn count_badge(dl: &mut DrawList, layer: Layer, slot: Rect, count: u32, color: u32) {
	const SHADOW: u32 = 0xE0000000;
	let px = scale::FIXED_GLYPH_PX;
	let chars = if count > 99 {
		3.0
	} else {
		(count.max(1)).ilog10() as f32 + 1.0
	};
	let x = slot.x + slot.w - 2.0 - chars * px;
	let y = slot.y + slot.h - px - 2.0;
	let off = px / 8.0; // one pixel of the 8px font
	if count > 99 {
		dl.text(layer, x + off, y + off, px, SHADOW, "99+");
		dl.text(layer, x, y, px, color, "99+");
	} else {
		dl.text_fmt(layer, x + off, y + off, px, SHADOW, format_args!("{count}"));
		dl.text_fmt(layer, x, y, px, color, format_args!("{count}"));
	}
}

/// One menu-style row's content, rendered by [`row`].
pub struct RowContent<'a> {
	pub text: &'a str,
	/// Right-aligned colored text (e.g. a ping column).
	pub right: Option<(&'a str, u32)>,
	/// Slider fill fraction 0..=1; the fill takes the row's color.
	pub bar: Option<f32>,
	pub color: u32,
	/// Keyboard/hover selection — computed by the screen, drawn here.
	pub highlight: bool,
}

/// A selectable text row: optional highlight backdrop, left text,
/// right-aligned column, and slider bar (geometry via [`slider_rect`],
/// so hit-testing shares it). No hit region — menu-style screens hit
/// their rows by id through their own maps.
pub fn row(dl: &mut DrawList, style: &Style, layer: Layer, rect: Rect, c: RowContent<'_>) {
	const ROW_HILIGHT: u32 = 0x60FFFFFF;
	const BAR_TRACK: u32 = 0x40FFFFFF;
	let glyph = style.glyph;
	let pad_x = scale::text_pad_x(style.scale);
	let text_y = rect.y + (rect.h - glyph) / 2.0;
	if c.highlight {
		dl.quad(layer, rect, ROW_HILIGHT);
	}
	dl.text(layer, rect.x + pad_x, text_y, glyph, c.color, c.text);
	if let Some((right, color)) = c.right {
		let rx = rect.x + rect.w - pad_x - right.len() as f32 * glyph;
		dl.text(layer, rx, text_y, glyph, color, right);
	}
	if let Some(frac) = c.bar {
		let bar = slider_rect(rect, style.scale);
		dl.quad(layer, bar, BAR_TRACK);
		dl.quad(
			layer,
			Rect::new(bar.x, bar.y, bar.w * frac.clamp(0.0, 1.0), bar.h),
			c.color,
		);
	}
}

/// Slider-bar logical width — screens measuring row content use it too.
pub const SLIDER_BAR_W: f32 = 200.0;

/// Slider-bar geometry within a row (right-aligned, centered
/// vertically) — one function serves rendering and hit-testing.
pub fn slider_rect(row: Rect, scale: f32) -> Rect {
	const BAR_H: f32 = 12.0;
	let (bw, bh) = (SLIDER_BAR_W * scale, BAR_H * scale);
	Rect::new(
		row.x + row.w - crate::ui::scale::text_pad_x(scale) - bw,
		row.y + (row.h - bh) / 2.0,
		bw,
		bh,
	)
}

/// Pure grid geometry, shared by any slot-grid screen (hotbar row,
/// inventory grid, future item screens).
#[derive(Clone, Copy)]
pub struct GridSpec {
	pub origin: (f32, f32),
	pub cols: usize,
	pub cell: f32,
	pub gap: f32,
}

impl GridSpec {
	pub fn rect(&self, i: usize) -> Rect {
		let (col, row) = (i % self.cols, i / self.cols);
		Rect::new(
			self.origin.0 + col as f32 * (self.cell + self.gap),
			self.origin.1 + row as f32 * (self.cell + self.gap),
			self.cell,
			self.cell,
		)
	}

	/// Total (width, height) of `n` cells laid out in this grid.
	pub fn extent(&self, n: usize) -> (f32, f32) {
		if n == 0 {
			return (0.0, 0.0);
		}
		let cols = self.cols.min(n);
		let rows = n.div_ceil(self.cols);
		(
			cols as f32 * (self.cell + self.gap) - self.gap,
			rows as f32 * (self.cell + self.gap) - self.gap,
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::draw::Cmd;

	#[test]
	fn slot_emits_backdrop_icon_and_hit() {
		let mut dl = DrawList::default();
		let mut hits: HitMap<u8> = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: Style::new(1.0),
		};
		let rect = Rect::new(100.0, 200.0, 56.0, 56.0);
		slot(&mut ui, 3u8, rect, Some((5, WHITE)), 48.0, true);
		// Backdrop: slot bg + 4 border quads.
		assert_eq!(dl.cmds(Layer::Backdrop).0.len(), 5);
		// Icon centered in the slot.
		match dl.cmds(Layer::Icons).0 {
			[Cmd::Icon {
				rect: r,
				tex_layer: 5,
				..
			}] => {
				assert_eq!((r.x, r.y, r.w, r.h), (104.0, 204.0, 48.0, 48.0));
			}
			_ => panic!("expected one icon"),
		}
		assert_eq!(hits.hit(120.0, 220.0), Some(3));
	}

	#[test]
	fn grid_spec_wraps_and_measures() {
		let g = GridSpec {
			origin: (10.0, 20.0),
			cols: 3,
			cell: 50.0,
			gap: 4.0,
		};
		assert_eq!(g.rect(0), Rect::new(10.0, 20.0, 50.0, 50.0));
		assert_eq!(g.rect(2).x, 10.0 + 2.0 * 54.0);
		assert_eq!(
			g.rect(3),
			Rect::new(10.0, 74.0, 50.0, 50.0),
			"wraps to row 2"
		);
		assert_eq!(g.extent(3), (158.0, 50.0));
		assert_eq!(g.extent(4), (158.0, 104.0));
		assert_eq!(g.extent(1), (50.0, 50.0));
		assert_eq!(g.extent(0), (0.0, 0.0));
	}
}
