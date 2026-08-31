//! The tutorial's HUD: a top-centered prompt panel (bottom-center is
//! the toasts', the corners are the touch cluster's) and the pulsing
//! outline that points at an anchored element. Free functions over the
//! DrawList, hud.rs style; the panel rect is ONE pure function shared by
//! drawing and the skip claim, the `corner_button_rect` discipline.

use super::hud::SafeArea;
use super::widgets::{Style, WHITE};
use super::{Layer, Rect, theme};

/// Muted ink for the panel's chrome (the step counter, the skip hint).
const CHROME: u32 = 0xFFB8B8B8;
/// The highlight outline's stroke, in device-independent px.
const OUTLINE: f32 = 3.0;

/// The panel's rect for this frame's text — top-centered, SafeArea
/// aware. Shared by the draw pass and the skip claim so they can never
/// disagree about where the panel is.
pub(crate) fn panel_rect(style: &Style, size: (u32, u32), insets: &SafeArea, text: &str) -> Rect {
	let scale = style.scale;
	let line = crate::font::face(style.face).line_h(style.glyph);
	let pad = theme::PAD * scale;
	let w = style.measure(text) + 2.0 * pad;
	let h = line * 2.2;
	Rect::new(
		(size.0 as f32 - w) / 2.0,
		insets.top + theme::TOP_MARGIN * scale,
		w,
		h,
	)
}

/// Emit the panel: the step line, "N / M", the skip hint, and a thin
/// progress bar for the accumulating steps. `celebrating` flashes the
/// line brighter for the beat after a step lands.
#[allow(clippy::too_many_arguments)]
pub(crate) fn panel(
	dl: &mut super::DrawList,
	style: &Style,
	size: (u32, u32),
	insets: &SafeArea,
	view: &crate::tutorial::TutorialView,
	text: &str,
	skip_hint: &str,
) {
	let scale = style.scale;
	let px = style.glyph;
	let line = crate::font::face(style.face).line_h(px);
	let pad = theme::PAD * scale;
	let rect = panel_rect(style, size, insets, text);
	dl.quad(Layer::Backdrop, rect, theme::FILL_BG);
	let ink = if view.celebrating { WHITE } else { 0xFFE8E8E8 };
	dl.text(
		Layer::Content,
		rect.x + pad,
		rect.y + (line - px) / 2.0,
		px,
		ink,
		text,
	);
	// Chrome under the line: "N / M" left, the skip hint right, smaller.
	let small = px * 0.8;
	let count = format!("{} / {}", view.index + 1, view.total);
	let chrome_y = rect.y + line + (line * 1.2 - line) / 2.0;
	dl.text(
		Layer::Content,
		rect.x + pad,
		chrome_y,
		small,
		CHROME,
		&count,
	);
	let hint_w = style.measure(skip_hint) * 0.8;
	dl.text(
		Layer::Content,
		rect.x + rect.w - pad - hint_w,
		chrome_y,
		small,
		CHROME,
		skip_hint,
	);
	// The accumulating steps show how far along the walk/turn is.
	if let Some(p) = view.progress {
		let count_w = style.measure(&count) * 0.8;
		let track = Rect::new(
			rect.x + pad + count_w + theme::CHIP_SEP * scale,
			chrome_y + small * 0.35,
			(rect.w - 2.0 * pad - count_w - hint_w - 2.0 * theme::CHIP_SEP * scale).max(0.0),
			small * 0.3,
		);
		if track.w > 0.0 {
			dl.quad(Layer::Backdrop, track, theme::PANEL_BG);
			let fill = Rect::new(track.x, track.y, track.w * p.clamp(0.0, 1.0), track.h);
			dl.quad(Layer::Content, fill, CHROME);
		}
	}
}

/// One collapse of the attention ring, seconds.
const PULSE_SECS: f32 = 1.1;
/// How far outside the target the ring spawns, dip px.
const PULSE_PAD: f32 = 16.0;

/// Emit one rectangular outline of stroke `s` hugging `rect`'s outside.
fn outline(dl: &mut super::DrawList, rect: Rect, s: f32, color: u32) {
	let (x, y, w, h) = (rect.x - s, rect.y - s, rect.w + 2.0 * s, rect.h + 2.0 * s);
	dl.quad(Layer::Ghost, Rect::new(x, y, w, s), color);
	dl.quad(Layer::Ghost, Rect::new(x, y + h - s, w, s), color);
	dl.quad(Layer::Ghost, Rect::new(x, y + s, s, h - 2.0 * s), color);
	dl.quad(
		Layer::Ghost,
		Rect::new(x + w - s, y + s, s, h - 2.0 * s),
		color,
	);
}

/// The anchored element's highlight, on the Ghost layer so it reads over
/// every screen: a steady outline marks the target, and a second ring
/// spawns outside it and collapses inward onto it, brightening as it
/// lands — motion the eye catches where a stationary blink was missed.
pub(crate) fn highlight(dl: &mut super::DrawList, style: &Style, rect: Rect, time_secs: f32) {
	use digblock_core::ease;
	let s = OUTLINE * style.scale;
	outline(dl, rect, s, 0xFFFF_D75A);
	let p = (time_secs / PULSE_SECS).fract();
	// Fast approach, slow landing; the alpha ramp keeps the far ring
	// faint so the loop reads as arriving, not blinking.
	let gap = (1.0 - ease::out_cubic(p)) * PULSE_PAD * style.scale;
	let a = ease::lerp(0x50 as f32, 0xFF as f32, p) as u32;
	let ring = Rect::new(
		rect.x - gap,
		rect.y - gap,
		rect.w + 2.0 * gap,
		rect.h + 2.0 * gap,
	);
	outline(dl, ring, s * 0.75, (a << 24) | 0x00FF_D75A);
}
