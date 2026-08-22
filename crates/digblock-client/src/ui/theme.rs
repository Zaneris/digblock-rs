//! The shared look: every spacing token and panel colour the screens draw with.
//! One module, because a screen that invents its own padding — or worse, its own
//! text size — reads as a different program bolted onto the same window.
//!
//! Two rules hold everything together:
//!
//! 1. **There is one text size per frame**, [`super::Style::glyph`], derived
//!    from the UI scale by [`super::scale::glyph_px`]. Nothing here scales text.
//!    The only sanctioned exception is [`super::scale::FIXED_GLYPH_PX`] (the
//!    debug overlay, a diagnostic rather than UI), and
//!    `screens::every_screen_draws_text_at_one_size` pins that.
//! 2. **Every constant here is a logical size** — multiply by `Style::scale`
//!    exactly once, at the point of use.

use super::scale;

/// Panel backdrop. Every screen's panel, menu included: the two used to
/// differ (a grey-lifted panel on the in-game screens, a flat black one
/// on the menu) under separate `PANEL_BG` constants, and the mismatch
/// showed as a visibly different transparency when you tabbed between
/// them. The alpha is the one that reads better under text, and the grey
/// lift is what keeps [`FILL_BG`] slots and buttons distinguishable from
/// the panel they sit on.
pub const PANEL_BG: u32 = 0xC0101010;

/// Full-screen dim laid under a panel, so the screen behind it recedes.
/// Faded by the world's own darkness through
/// [`super::widgets::scaled_dim`], and — like [`PANEL_BG`] — the same on
/// every screen; the menu used to lay a heavier dim than the in-game
/// screens did.
pub const SCREEN_DIM: u32 = 0xA0000000;

/// The standard translucent fill: item slots, buttons, and the HUD's
/// backdrops behind loose text (chat lines, the debug panel). Lighter
/// than [`PANEL_BG`] on purpose — it is meant to sit *on* a panel and
/// still be visible.
pub const FILL_BG: u32 = 0x90000000;

/// Inset between a panel's backdrop edge and its content box.
pub const PAD: f32 = 12.0;

/// A plain text row: menu rows, section headers, titles. Text centers
/// in it, so this is leading as much as it is height.
pub const ROW_H: f32 = 28.0;

/// A row carrying an item icon and an action button — taller than a
/// text row because the icon and button set the floor, not the text.
pub const ACTION_ROW_H: f32 = 46.0;

/// Breathing room between adjacent action rows. Text rows don't take
/// one: [`ROW_H`] already carries their leading.
pub const ROW_GAP: f32 = 6.0;

/// A blank band separating two sections of one panel.
pub const SECTION_GAP: f32 = 18.0;

/// Clearance a bottom-anchored panel keeps above the hotbar.
pub const GAP_ABOVE_BAR: f32 = 24.0;

/// Clearance a bottom-anchored panel keeps below the top of the window
/// when [`fit_scale`] has run out of room.
pub const TOP_MARGIN: f32 = 12.0;

/// A chip is an icon (a `Style::glyph` square — the one size text has)
/// and a number: this is the gap between the two …
pub const CHIP_GAP: f32 = 4.0;
/// … and this the gap between one chip and the next in a run.
pub const CHIP_SEP: f32 = 12.0;
/// Clearance between a hovered thing and the tooltip naming it.
pub const TOOLTIP_GAP: f32 = 6.0;
/// The ink of a chip's number when the holdings cannot cover it — the
/// one way a price says "not yet" now that it has no words.
pub const SHORT_INK: u32 = 0xFF5A5AFF;

/// Panels never shrink below this row width — a short screen would
/// otherwise read as a narrow column, and would resize as its rows
/// changed.
pub const MIN_PANEL_W: f32 = 560.0;

/// Panel geometry from a content row's extent: a row `w` wide and `h`
/// tall, padded. The one place `PAD` becomes pixels.
pub fn panel_of(row_x: f32, row_y: f32, row_w: f32, row_h: f32, scale: f32) -> super::Rect {
	let pad = PAD * scale;
	super::Rect::new(
		row_x - pad,
		row_y - pad,
		row_w + 2.0 * pad,
		row_h + 2.0 * pad,
	)
}

/// Width a text row needs: the text plus the row widget's own inset on
/// both sides. Screens size their panels by folding this over their
/// rows — never by counting characters, which a proportional face makes
/// meaningless.
pub fn text_row_w(style: &super::Style, text: &str) -> f32 {
	2.0 * scale::text_pad_x(style.scale) + style.measure(text)
}

/// Step `scale` down the fit ladder until the layout fits `budget`, and return
/// the scale to draw at.
///
/// `needed(scale)` re-measures the whole layout at a candidate scale — it has to
/// be re-measured rather than divided, because `glyph_px` rounds to whole pixels
/// and the layout's text is not a linear function of the scale.
///
/// Stepping is the point. Multiplying the style by `budget / needed` produces
/// text at some arbitrary fraction of the size the menu beside it draws at,
/// which is exactly the mismatch this module exists to prevent. `STEP` is the
/// same increment the derived half of [`scale::ui_scale`] moves in, so a
/// shrunken panel is the page one smaller window would have drawn. Bottoms out
/// at [`FIT_FLOOR`], which is LOWER than the interface scale's own floor on
/// purpose — see that constant.
pub fn fit_scale(scale: f32, budget: f32, needed: impl Fn(f32) -> f32) -> f32 {
	const STEP: f32 = 0.25;
	let mut s = scale;
	while needed(s) > budget && s > FIT_FLOOR {
		s = (s - STEP).max(FIT_FLOOR);
	}
	s
}

/// How far the fit ladder may step, as distinct from [`scale::UI_SCALE_FLOOR`],
/// the smallest interface scale a player may CHOOSE.
///
/// The two floors answer different questions. The interface floor is a taste
/// limit: below it text is unpleasantly small, so the slider does not go there.
/// This one is a last resort, and its alternative is not "slightly bigger text"
/// but a page whose bottom half is behind the hotbar. A cramped page you can
/// read all of beats a comfortable one you cannot reach the bottom of.
///
/// **This is as low as the ladder can usefully go**, and that is arithmetic
/// rather than taste: [`scale::glyph_px`] clamps at a whole readable pixel size
/// which this rung already sits on, so a rung below takes a quarter off every
/// box while the TEXT stays exactly where it was — boxes shrinking around fixed
/// glyphs, and text-derived widths frozen while the panel keeps narrowing. What
/// a page that still does not fit needs is fewer ROWS, which is why `InvScreen`
/// reflows.
pub const FIT_FLOOR: f32 = 0.5;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fit_walks_down_to_the_first_rung_that_fits() {
		// A layout whose cost is linear in the scale, so each rung asks
		// for that rung's multiple of the base.
		let needed = |s: f32| 400.0 * s;
		assert_eq!(fit_scale(1.75, 1000.0, needed), 1.75, "already fits");
		assert_eq!(fit_scale(1.75, 620.0, needed), 1.5);
		assert_eq!(fit_scale(1.75, 450.0, needed), 1.0);
		// Every rung is a scale `ui_scale` itself can produce.
		for budget in [300.0f32, 450.0, 620.0, 800.0] {
			let s = fit_scale(1.75, budget, needed);
			assert!((s * 4.0).fract().abs() < 1e-4, "{s} is off the ladder");
			assert!(s >= scale::UI_SCALE_FLOOR);
		}
	}

	#[test]
	fn an_impossible_budget_bottoms_out_instead_of_looping() {
		assert_eq!(fit_scale(3.0, 0.0, |s| 400.0 * s), FIT_FLOOR);
		assert_eq!(fit_scale(0.75, -1.0, |s| 400.0 * s), FIT_FLOOR);
	}
}
