//! Small composable widgets: pure functions that emit draw commands and
//! register hit regions in one pass. Nothing here holds state — screens
//! own their state machines and call these per frame.

use super::draw::{DrawList, Layer};
use super::hit::HitMap;
use super::{Rect, scale};

// Shared palette (0xAABBGGRR, matching overlay.rs).
pub const WHITE: u32 = 0xFFFFFFFF;
pub const DIM: u32 = 0xFFB0B0B0;
/// Slot/button fill — [`super::theme::FILL_BG`], named for its most
/// common use.
pub const SLOT_BG: u32 = super::theme::FILL_BG;
/// Keyboard/hover highlight fill, shared by menu rows and buttons.
pub const ROW_HILIGHT: u32 = 0x60FFFFFF;
/// Track behind slider bars, progress bars, and the tab strip's rule —
/// one alpha for every "inactive groove" in the UI.
pub const BAR_TRACK: u32 = 0x40FFFFFF;
const SEL_BORDER: u32 = 0xE0FFFFFF;
const SEL_BORDER_WEAK: u32 = 0x80FFFFFF;

/// Selection-ring weight for [`slot`]: `Strong` marks the thing an
/// action will hit (the hotbar's active slot, a chosen member tile, an
/// armed extraction slot); `Weak` marks enclosing context (the selected
/// family whose members are on show). Two weights, one containment
/// hierarchy — never two meanings at one weight on the same screen.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SlotRing {
	None,
	Weak,
	Strong,
}

/// Fade a full-screen dim with the world's own darkness so night menus
/// aren't double-darkened: scales the alpha byte by `daylight` (1 noon →
/// 0 deep night) with a floor so panels still separate from the sky.
/// Panel backdrops keep their fixed color — only the dim scales.
pub fn scaled_dim(base: u32, daylight: f32) -> u32 {
	let f = 0.25 + 0.75 * daylight.clamp(0.0, 1.0);
	let a = ((base >> 24) as f32 * f) as u32;
	a << 24 | (base & 0x00FF_FFFF)
}

/// Per-frame style: the UI scale, its derived text box height, and which
/// face to measure and draw with.
#[derive(Clone, Copy)]
pub struct Style {
	pub scale: f32,
	pub glyph: f32,
	pub face: crate::font::FaceId,
}

impl Style {
	pub fn new(ui_scale: f32) -> Self {
		Self {
			scale: ui_scale,
			glyph: scale::glyph_px(ui_scale),
			face: crate::font::FaceId::Ui,
		}
	}

	/// Width of `text` as this style would draw it. The one way screens
	/// size themselves to their content — text is proportional, so a
	/// character count tells you nothing.
	pub fn measure(&self, text: &str) -> f32 {
		crate::font::face(self.face).measure(text, self.glyph)
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

/// An item slot: backdrop quad + hover wash + optional centered icon +
/// selection ring, hit-testable under `id`. Count text is the caller's
/// job (the hotbar pins its size; other screens scale). Hover uses the
/// same [`ROW_HILIGHT`] wash as [`button`], so "the pointer is on
/// something clickable" reads identically everywhere.
pub fn slot<Id: Copy + PartialEq>(
	ui: &mut Ui<'_, Id>,
	id: Id,
	rect: Rect,
	icon: Option<(super::hud::IconKind, u32)>,
	icon_px: f32,
	ring: SlotRing,
	hovered: bool,
) {
	ui.dl.quad(Layer::Backdrop, rect, SLOT_BG);
	if hovered {
		ui.dl.quad(Layer::Backdrop, rect, ROW_HILIGHT);
	}
	let border = match ring {
		SlotRing::None => None,
		SlotRing::Weak => Some(SEL_BORDER_WEAK),
		SlotRing::Strong => Some(SEL_BORDER),
	};
	if let Some(color) = border {
		let t = (2.0 * ui.style.scale).round().max(1.0);
		let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
		let dl = &mut *ui.dl;
		dl.quad(
			Layer::Backdrop,
			Rect::new(x - t, y - t, w + 2.0 * t, t),
			color,
		);
		dl.quad(
			Layer::Backdrop,
			Rect::new(x - t, y + h, w + 2.0 * t, t),
			color,
		);
		dl.quad(Layer::Backdrop, Rect::new(x - t, y, t, h), color);
		dl.quad(Layer::Backdrop, Rect::new(x + w, y, t, h), color);
	}
	if let Some((kind, tint)) = icon {
		super::hud::draw_icon(
			ui.dl,
			Layer::Icons,
			rect.centered(icon_px, icon_px),
			kind,
			tint,
		);
	}
	ui.hits.push(rect, id);
}

/// A clickable text button: backdrop, centered label, hit rect under
/// `id`. `enabled: false` dims the label and still registers no hit —
/// disabled buttons are honest pixels, not traps. `hovered` paints the
/// shared highlight (only meaningful for enabled buttons).
pub fn button<Id: Copy + PartialEq>(
	ui: &mut Ui<'_, Id>,
	id: Id,
	rect: Rect,
	text: &str,
	enabled: bool,
	hovered: bool,
) {
	ui.dl.quad(Layer::Backdrop, rect, SLOT_BG);
	if enabled && hovered {
		ui.dl.quad(Layer::Backdrop, rect, ROW_HILIGHT);
	}
	let px = ui.style.glyph;
	let w = ui.style.measure(text);
	let color = if enabled { WHITE } else { DIM };
	ui.dl.text(
		Layer::Content,
		rect.x + (rect.w - w) / 2.0,
		rect.y + (rect.h - px) / 2.0,
		px,
		color,
		text,
	);
	if enabled {
		ui.hits.push(rect, id);
	}
}

/// Stack-count badge in a slot's bottom-right corner, saturating at
/// "99+", with a drop shadow so digits stay readable on light icons
/// (snow, sand).
///
/// Drawn at the frame's text size like everything else. It used to be
/// pinned at [`scale::FIXED_GLYPH_PX`] — a holdover from the bitmap font,
/// whose sizes snapped to multiples of its cell, so "one step down" was
/// the only way to get a small badge. With a real face there is no ladder
/// to step down, and a pinned badge just read as half-size text beside a
/// scaled-up UI on a dense display.
pub fn count_badge(dl: &mut DrawList, layer: Layer, style: &Style, slot: Rect, count: u32, c: u32) {
	const SHADOW: u32 = 0xE0000000;
	let px = style.glyph;
	let inset = 2.0 * style.scale;
	let mut buf = [0u8; 3];
	let text: &str = if count > 99 {
		"99+"
	} else {
		// Right-aligning off a digit count would assume tabular figures;
		// measure what we're actually about to draw instead.
		let n = count.min(99);
		let len = if n >= 10 {
			buf[0] = b'0' + (n / 10) as u8;
			buf[1] = b'0' + (n % 10) as u8;
			2
		} else {
			buf[0] = b'0' + n as u8;
			1
		};
		std::str::from_utf8(&buf[..len]).unwrap_or("?")
	};
	let x = slot.x + slot.w - inset - crate::font::ui().measure(text, px);
	let y = slot.y + slot.h - px - inset;
	let off = (px / 16.0).round().max(1.0);
	dl.text(layer, x + off, y + off, px, SHADOW, text);
	dl.text(layer, x, y, px, c, text);
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
	/// Draw a text caret at the end of `text` (blink is the caller's).
	pub caret: bool,
}

/// Width of a text caret in a `px`-tall text box. Matches the chat
/// input's block caret.
pub fn caret_w(px: f32) -> f32 {
	px * 0.5
}

/// A selectable text row: optional highlight backdrop, left text,
/// right-aligned column, and slider bar (geometry via [`slider_rect`],
/// so hit-testing shares it). No hit region — menu-style screens hit
/// their rows by id through their own maps.
pub fn row(dl: &mut DrawList, style: &Style, layer: Layer, rect: Rect, c: RowContent<'_>) {
	let glyph = style.glyph;
	let pad_x = scale::text_pad_x(style.scale);
	let text_y = rect.y + (rect.h - glyph) / 2.0;
	if c.highlight {
		dl.quad(layer, rect, ROW_HILIGHT);
	}
	dl.text(layer, rect.x + pad_x, text_y, glyph, c.color, c.text);
	if c.caret {
		// A blinking caret has to be its own quad: swapping a '_' in and
		// out of the row string would change the string's width every
		// blink, and the menu sizes its panel to its widest row.
		let cx = rect.x + pad_x + style.measure(c.text);
		dl.quad(layer, Rect::new(cx, text_y, caret_w(glyph), glyph), c.color);
	}
	if let Some((right, color)) = c.right {
		let rx = rect.x + rect.w - pad_x - style.measure(right);
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
		let kind = super::super::hud::IconKind::Cube(digblock_core::block::BlockType::Dirt);
		slot(
			&mut ui,
			3u8,
			rect,
			Some((kind, WHITE)),
			48.0,
			SlotRing::Strong,
			false,
		);
		// Backdrop: the slot fill plus the ring's border quads.
		assert_eq!(dl.cmds(Layer::Backdrop).0.len(), 5);
		// A cube icon: three projected faces, all inside the icon rect.
		let icons = dl.cmds(Layer::Icons).0;
		assert_eq!(icons.len(), 3);
		for c in icons {
			match c {
				Cmd::IconQuad { corners, .. } => {
					for p in corners {
						assert!(p[0] >= 104.0 - 0.01 && p[0] <= 152.0 + 0.01);
						assert!(p[1] >= 204.0 - 0.01 && p[1] <= 252.0 + 0.01);
					}
				}
				_ => panic!("expected projected icon quads"),
			}
		}
		assert_eq!(hits.hit(120.0, 220.0), Some(3));
	}

	/// The three ring weights and the hover wash are distinct quad
	/// budgets: no ring is a bare fill, weak and strong add the ring's
	/// borders, and hover adds one wash under either.
	#[test]
	fn slot_rings_and_hover_have_distinct_quads() {
		let quads = |ring: SlotRing, hovered: bool| {
			let mut dl = DrawList::default();
			let mut hits: HitMap<u8> = HitMap::default();
			let mut ui = Ui {
				dl: &mut dl,
				hits: &mut hits,
				style: Style::new(1.0),
			};
			slot(
				&mut ui,
				1u8,
				Rect::new(0.0, 0.0, 56.0, 56.0),
				None,
				48.0,
				ring,
				hovered,
			);
			dl.cmds(Layer::Backdrop).0.len()
		};
		assert_eq!(quads(SlotRing::None, false), 1);
		assert_eq!(quads(SlotRing::None, true), 2, "hover wash");
		assert_eq!(quads(SlotRing::Weak, false), 5);
		assert_eq!(quads(SlotRing::Strong, true), 6, "wash + ring");
	}

	#[test]
	fn button_hover_highlights_only_when_enabled() {
		let count_quads = |enabled: bool, hovered: bool| {
			let mut dl = DrawList::default();
			let mut hits: HitMap<u8> = HitMap::default();
			let mut ui = Ui {
				dl: &mut dl,
				hits: &mut hits,
				style: Style::new(1.0),
			};
			let rect = Rect::new(10.0, 10.0, 120.0, 32.0);
			button(&mut ui, 1u8, rect, "GO", enabled, hovered);
			(dl.cmds(Layer::Backdrop).0.len(), hits.hit(20.0, 20.0))
		};
		assert_eq!(count_quads(true, false), (1, Some(1)));
		assert_eq!(count_quads(true, true), (2, Some(1)), "highlight quad");
		assert_eq!(
			count_quads(false, true),
			(1, None),
			"disabled: no highlight, no hit"
		);
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
