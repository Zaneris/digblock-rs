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

/// Stack-count badge in a slot's bottom-right corner, saturating at "99+", with a drop shadow so
/// digits stay readable on light icons (snow, sand). Drawn at the frame's one text size like
/// everything else, not pinned at [`scale::FIXED_GLYPH_PX`] — a pinned badge reads as half-size
/// text beside a scaled-up UI on a dense display.
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

/// An amount of something with an icon: essence, feedstock, a fare. The
/// ONLY way an amount of essence reaches the screen — never as a word,
/// because a name is a tooltip's job and a price that has to fit in a
/// sentence ends up dropped when the sentence does not.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Chip {
	pub kind: super::hud::IconKind,
	pub tint: u32,
	pub n: u32,
	/// The number's colour — [`super::theme::SHORT_INK`] when the holdings
	/// cannot cover it.
	pub ink: u32,
}

/// A `u32` written into a caller's stack buffer. The amount on a chip is measured through
/// `Face::run` like any other string — never from a digit count, which tells you nothing about a
/// proportional face — but a heap `String` per chip is pure per-frame churn, and a screen sizes
/// itself around its own chips several times before drawing them once.
fn digits(n: u32, buf: &mut [u8; 10]) -> &str {
	let mut i = buf.len();
	let mut n = n;
	loop {
		i -= 1;
		buf[i] = b'0' + (n % 10) as u8;
		n /= 10;
		if n == 0 {
			break;
		}
	}
	std::str::from_utf8(&buf[i..]).expect("decimal digits are ASCII")
}

/// The one placement walk behind [`chip_layout`], [`chips_extent`] and [`chips`]: a rect per chip
/// spanning its icon and number, wrapping past `max_w` onto the next [`super::theme::ROW_H`] row.
/// Shared rather than re-derived so a run measured for a panel's width and the run drawn into it
/// cannot disagree, and so the callers that only need an extent never build a list to fold it.
fn walk_chips(style: &Style, x: f32, y: f32, max_w: f32, chips: &[Chip], mut f: impl FnMut(Rect)) {
	let glyph = style.glyph;
	let row_h = super::theme::ROW_H * style.scale;
	let gap = super::theme::CHIP_GAP * style.scale;
	let sep = super::theme::CHIP_SEP * style.scale;
	let (mut cx, mut cy) = (x, y);
	let mut buf = [0u8; 10];
	for c in chips {
		let w = glyph + gap + style.measure(digits(c.n, &mut buf));
		if cx > x && cx + w - x > max_w {
			cx = x;
			cy += row_h;
		}
		f(Rect::new(cx, cy, w, row_h));
		cx += w + sep;
	}
}

/// The width a chip's number takes — what a page reserving a column of
/// amounts measures its widest row by.
pub fn amount_w(style: &Style, n: u32) -> f32 {
	let mut buf = [0u8; 10];
	style.measure(digits(n, &mut buf))
}

/// The span of a row's two fare columns — the icon and the `amount_w`
/// reserved for its number — as [`row`] lays them out from the right
/// edge. The width a page sizes a fare row by, and the box it hit-tests
/// for hover.
pub fn fare_cols_w(style: &Style, amount_w: f32) -> f32 {
	style.glyph + super::theme::CHIP_GAP * style.scale + amount_w
}

/// The run of `chips` laid out from `(x, y)`, as [`walk_chips`] places
/// them — the measure half of [`chips`], and what a screen hit-tests for
/// hover.
pub fn chip_layout(style: &Style, x: f32, y: f32, max_w: f32, chips: &[Chip]) -> Vec<Rect> {
	let mut out = Vec::with_capacity(chips.len());
	walk_chips(style, x, y, max_w, chips, |r| out.push(r));
	out
}

/// The box `chips` occupies inside a `max_w` budget, as
/// [`chip_layout`] places them. The measure a caller with a FIXED span
/// needs: how tall the run becomes once it wraps, which is the height
/// the row holding it has to reserve.
pub fn chips_extent(style: &Style, max_w: f32, chips: &[Chip]) -> (f32, f32) {
	let (mut w, mut h) = (0.0f32, 0.0f32);
	walk_chips(style, 0.0, 0.0, max_w, chips, |r| {
		w = w.max(r.x + r.w);
		h = h.max(r.y + r.h);
	});
	(w, h)
}

/// Width of `chips` in one unwrapped run — what a panel sizes itself by.
pub fn chips_w(style: &Style, chips: &[Chip]) -> f32 {
	chips_extent(style, f32::INFINITY, chips).0
}

/// Draw `chips` as [`chip_layout`] places them: the icon on
/// [`Layer::Icons`], the number beside it in the chip's ink at the
/// frame's one text size, both centred in the row. Returns the rects.
pub fn chips(
	dl: &mut DrawList,
	style: &Style,
	layer: Layer,
	x: f32,
	y: f32,
	max_w: f32,
	chips: &[Chip],
) -> Vec<Rect> {
	let glyph = style.glyph;
	let gap = super::theme::CHIP_GAP * style.scale;
	let rects = chip_layout(style, x, y, max_w, chips);
	for (c, r) in chips.iter().zip(&rects) {
		let top = r.y + (r.h - glyph) / 2.0;
		super::hud::draw_icon(
			dl,
			Layer::Icons,
			Rect::new(r.x, top, glyph, glyph),
			c.kind,
			c.tint,
		);
		dl.text_fmt(
			layer,
			r.x + glyph + gap,
			top,
			glyph,
			c.ink,
			format_args!("{}", c.n),
		);
	}
	rects
}

/// The name of whatever the cursor is on: a [`super::theme::PANEL_BG`] box on [`Layer::Ghost`] —
/// above everything, the drag ghost included — at the frame's one text size, above its anchor and
/// inside the window, flipping below when above would leave it. The screen that knows what is
/// hovered emits it LAST, so it covers what it names. A name WIDER than the window pins to the
/// left edge and lets its tail run off the right: the start of a name is what identifies it.
pub fn tooltip(dl: &mut DrawList, style: &Style, size: (u32, u32), anchor: Rect, name: &str) {
	let text = name.to_uppercase();
	let glyph = style.glyph;
	let w = super::theme::text_row_w(style, &text);
	let h = crate::font::face(style.face).line_h(glyph);
	let gap = super::theme::TOOLTIP_GAP * style.scale;
	let (sw, sh) = (size.0 as f32, size.1 as f32);
	let x = if w >= sw {
		0.0
	} else {
		(anchor.x + (anchor.w - w) / 2.0).clamp(0.0, sw - w)
	};
	let above = anchor.y - gap - h;
	let y = if above >= 0.0 {
		above
	} else {
		(anchor.y + anchor.h + gap).min((sh - h).max(0.0))
	};
	let rect = Rect::new(x, y, w, h);
	dl.quad(Layer::Ghost, rect, super::theme::PANEL_BG);
	dl.text(
		Layer::Ghost,
		rect.x + scale::text_pad_x(style.scale),
		rect.y + (h - glyph) / 2.0,
		glyph,
		WHITE,
		&text,
	);
}

/// What a row's right-hand column holds.
#[derive(Clone, Copy)]
pub enum RowRight<'a> {
	/// Coloured text (a ping, a hint, a state).
	Text(&'a str, u32),
	/// An amount with an icon (a fare), drawn as two columns: the number
	/// right-aligned inside `amount_w` — the widest amount on the page, so
	/// every row's icon lands on one vertical — and the icon just left of it.
	Chip { chip: Chip, amount_w: f32 },
}

/// One menu-style row's content, rendered by [`row`].
pub struct RowContent<'a> {
	pub text: &'a str,
	/// Right-aligned column.
	pub right: Option<RowRight<'a>>,
	/// Slider fill fraction 0..=1; the fill takes the row's color.
	pub bar: Option<f32>,
	pub color: u32,
	/// Keyboard/hover selection — computed by the screen, drawn here.
	pub highlight: bool,
	/// Columns after the text, each at a fixed offset from the text's origin so a run of rows
	/// lines up; the caller measures the widest entry of each column and hands every row the
	/// same offsets. Sized into a panel by [`cells_w`].
	pub cells: &'a [Cell<'a>],
	/// Draw a text caret at the end of `text` (blink is the caller's).
	pub caret: bool,
}

/// Width of a text caret in a `px`-tall text box. Matches the chat
/// input's block caret.
pub fn caret_w(px: f32) -> f32 {
	px * 0.5
}

/// One column entry on a row: text drawn at `x` past the row's text origin.
pub struct Cell<'a> {
	pub x: f32,
	pub text: &'a str,
	pub color: u32,
}

/// How far past the text origin a row's cells reach — what a panel sized to its widest row
/// takes instead of the text's own width when the cells run past it.
pub fn cells_w(style: &Style, cells: &[Cell<'_>]) -> f32 {
	cells
		.iter()
		.map(|c| c.x + style.measure(c.text))
		.fold(0.0, f32::max)
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
		// The wash has to sit UNDER the row's own icon: a chip draws in `Layer::Icons`, which
		// the whole of `Layer::Content` covers, so a wash on the caller's layer greys the icon
		// and leaves its number crisp — one row reading as two states. [`slot`] and [`button`]
		// wash on `Layer::Backdrop` for the same reason, and every screen panels there
		// (`every_screen_dims_and_panels_the_same_way`), so the wash lands over its panel.
		dl.quad(Layer::Backdrop, rect, ROW_HILIGHT);
	}
	dl.text(layer, rect.x + pad_x, text_y, glyph, c.color, c.text);
	for cell in c.cells {
		dl.text(
			layer,
			rect.x + pad_x + cell.x,
			text_y,
			glyph,
			cell.color,
			cell.text,
		);
	}
	if c.caret {
		// A blinking caret has to be its own quad: swapping a '_' in and
		// out of the row string would change the string's width every
		// blink, and the menu sizes its panel to its widest row.
		let cx = rect.x + pad_x + style.measure(c.text);
		dl.quad(layer, Rect::new(cx, text_y, caret_w(glyph), glyph), c.color);
	}
	match c.right {
		Some(RowRight::Text(right, color)) => {
			let rx = rect.x + rect.w - pad_x - style.measure(right);
			dl.text(layer, rx, text_y, glyph, color, right);
		}
		Some(RowRight::Chip { chip, amount_w }) => {
			let right = rect.x + rect.w - pad_x;
			let icon_x = right - fare_cols_w(style, amount_w);
			super::hud::draw_icon(
				dl,
				Layer::Icons,
				Rect::new(icon_x, text_y, glyph, glyph),
				chip.kind,
				chip.tint,
			);
			dl.text_fmt(
				layer,
				right - self::amount_w(style, chip.n),
				text_y,
				glyph,
				chip.ink,
				format_args!("{}", chip.n),
			);
		}
		None => {}
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

	/// A chip is MEASURED through [`digits`] and DRAWN through `text_fmt`'s `{}`, so the two have
	/// to render a `u32` the same way or every chip is laid out to a width its number does not
	/// fill. Swept across the decade boundaries and both ends of the range rather than spot-checked,
	/// since what would break is a carry — and counted, so a sweep that stopped early fails.
	#[test]
	fn a_chips_measured_digits_are_the_ones_it_draws() {
		let mut buf = [0u8; 10];
		let mut checked = 0u32;
		// Every power of ten a u32 holds, and the value either side of it.
		let edges = (0..10).flat_map(|p: u32| {
			let d = 10u32.pow(p);
			[d - 1, d, d + 1]
		});
		let extremes = [0, u32::MAX - 1, u32::MAX];
		for n in edges.chain(extremes).chain(0..2048) {
			assert_eq!(digits(n, &mut buf), n.to_string(), "{n}");
			checked += 1;
		}
		assert_eq!(
			checked,
			(30 + extremes.len() + 2048) as u32,
			"the sweep covered what it meant to"
		);
	}

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

	/// The layout is the measure: every chip has a rect, chips run left to
	/// right without overlapping, the icon is the frame's glyph, and a
	/// second row appears exactly when the run is wider than its budget.
	#[test]
	fn chips_lay_out_left_to_right_and_wrap_only_past_max_w() {
		use crate::ui::hud::IconKind;
		use digblock_core::block::Essence;
		let style = Style::new(1.0);
		let run: Vec<Chip> = Essence::ALL
			.iter()
			.enumerate()
			.map(|(i, &e)| Chip {
				kind: IconKind::Essence(e),
				tint: WHITE,
				n: 7 * (i as u32 + 1),
				ink: WHITE,
			})
			.collect();
		let wide = chips_w(&style, &run);
		let rects = chip_layout(&style, 30.0, 40.0, f32::INFINITY, &run);
		assert_eq!(rects.len(), run.len());
		for (i, r) in rects.iter().enumerate() {
			assert_eq!(r.y, 40.0, "one row when nothing wraps");
			assert!(r.w > style.glyph, "a chip is wider than its icon");
			if i > 0 {
				let prev = rects[i - 1];
				assert!(r.x >= prev.x + prev.w, "chip {i} overlaps its neighbour");
			}
		}
		assert_eq!(
			rects.last().unwrap().x + rects.last().unwrap().w - 30.0,
			wide
		);
		let wrapped = chip_layout(&style, 30.0, 40.0, wide - 1.0, &run);
		assert!(
			wrapped.iter().any(|r| r.y > 40.0),
			"a run wider than its budget wraps"
		);
		assert!(
			wrapped
				.iter()
				.all(|r| r.x + r.w - 30.0 <= wide - 1.0 || r.x == 30.0)
		);
		// Emit agrees with measure: one icon per chip, as big as a glyph.
		let mut dl = DrawList::default();
		let drawn = chips(
			&mut dl,
			&style,
			Layer::Content,
			30.0,
			40.0,
			f32::INFINITY,
			&run,
		);
		assert_eq!(drawn, rects);
		let (cmds, _) = dl.cmds(Layer::Icons);
		let icons: Vec<Rect> = cmds
			.iter()
			.filter_map(|c| match c {
				Cmd::Icon { rect, .. } => Some(*rect),
				_ => None,
			})
			.collect();
		assert_eq!(icons.len(), run.len());
		assert!(
			icons
				.iter()
				.all(|r| r.w == style.glyph && r.h == style.glyph)
		);
	}

	/// Whatever it is anchored to, the name stays inside the window — a tooltip off the top flips
	/// below, one off a side slides in — and the TEXT rides with the box instead of being clamped
	/// on its own. Swept over every name a tooltip can carry (blocks and essences, the whole
	/// vocabulary) at the screen sweeps' scales, in windows small enough for the clamp to bite. A
	/// name wider than the window is decided rather than clamped: pinned left, tail off the right.
	#[test]
	fn a_tooltip_never_leaves_the_window() {
		use digblock_core::block::{BlockType, Essence};
		let names: Vec<String> = BlockType::ALL
			.iter()
			.map(|b| b.name())
			.chain(Essence::ALL.iter().map(|e| e.name().to_string()))
			.collect();
		let mut checked = 0;
		let mut overflowed = 0;
		for size in [(300u32, 200u32), (640, 1136), (1280, 800)] {
			let (sw, sh) = (size.0 as f32, size.1 as f32);
			for &s in &[0.7f32, 0.75, 1.0, 1.7, 1.75, 2.5, scale::UI_SCALE_CAP] {
				let style = Style::new(s);
				let corners = [
					Rect::new(0.0, 0.0, 20.0, 20.0),
					Rect::new(sw - 20.0, 0.0, 20.0, 20.0),
					Rect::new(0.0, sh - 20.0, 20.0, 20.0),
					Rect::new(sw - 20.0, sh - 20.0, 20.0, 20.0),
				];
				for name in &names {
					for anchor in corners {
						let mut dl = DrawList::default();
						tooltip(&mut dl, &style, size, anchor, name);
						let (cmds, arena) = dl.cmds(Layer::Ghost);
						let (mut boxes, mut texts) = (Vec::new(), Vec::new());
						for c in cmds {
							match c {
								Cmd::Quad { rect, .. } => boxes.push(*rect),
								Cmd::Text {
									x, y, px, range, ..
								} => texts.push((
									*x,
									*y,
									*px,
									&arena[range.0 as usize..range.1 as usize],
								)),
								_ => panic!("a tooltip is a box and a name"),
							}
						}
						let rect = match boxes[..] {
							[r] => r,
							_ => panic!("one box per name"),
						};
						let (tx, ty, px, text) = match texts[..] {
							[t] => t,
							_ => panic!("one name per box"),
						};
						let where_ = format!("{name:?} at {size:?}/{s} on {anchor:?}");
						assert_eq!(text, name.to_uppercase(), "{where_}");
						// Vertically the box always fits: it flips below
						// its anchor rather than leaving the window.
						assert!(rect.y >= 0.0 && rect.y + rect.h <= sh, "{where_}: {rect:?}");
						// The text rides INSIDE the box, at the frame's own
						// size — never clamped independently of it.
						assert_eq!(px, style.glyph, "{where_}");
						assert_eq!(tx, rect.x + scale::text_pad_x(s), "{where_}");
						assert!(ty >= rect.y && ty + px <= rect.y + rect.h, "{where_}");
						assert!(
							tx + style.measure(text) <= rect.x + rect.w + 0.01,
							"{where_}: the name outgrew its box"
						);
						if rect.w <= sw {
							assert!(rect.x >= 0.0 && rect.x + rect.w <= sw, "{where_}: {rect:?}");
						} else {
							// Wider than the window: pinned left, tail off
							// the right — the decided case, not a clamp
							// degenerating.
							assert_eq!(rect.x, 0.0, "{where_}: not pinned left");
							overflowed += 1;
						}
						checked += 1;
					}
				}
			}
		}
		assert_eq!(checked, names.len() * 4 * 7 * 3, "every name, every pose");
		assert!(
			overflowed > 0,
			"no name outgrew any window — the wider-than-window case is untested"
		);
	}
}
