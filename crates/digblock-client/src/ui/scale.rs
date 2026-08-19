//! UI scale: device scale factor × window-size factor × the user's
//! preference. Rects and text both scale continuously — glyphs are
//! rasterized at whatever whole pixel size comes out, so there is no
//! integer-multiple ladder to snap to any more.

/// Reference text box height, in device px, at scale 1.0. The
/// pre-scaling design size, kept so a default desktop window looks the
/// same as it always has.
const BASE_TEXT_PX: f32 = 16.0;

/// Ceiling on the whole UI scale. Bounds the largest glyph the atlas can
/// be asked to cache — `font::atlas` pins that the worst case still fits
/// the page.
pub const UI_SCALE_CAP: f32 = 3.0;

/// The widest a window's own proportions may push the UI before the
/// user's preference multiplies in. Distinct from [`UI_SCALE_MAX`],
/// which bounds the PREFERENCE: the two coincide today and mean
/// different things, so a retune of either must not drag the other.
pub const WINDOW_FACTOR_CAP: f32 = 1.5;

/// Floor on the DERIVED scale (display density × window size), below
/// which a default-preference UI stops being comfortably readable. Also
/// the bottom rung of [`super::theme::fit_scale`]'s ladder. The user's
/// own preference multiplies through this, so the final scale can sit
/// below it deliberately.
pub const UI_SCALE_FLOOR: f32 = 0.75;

/// Interface-scale slider: bounds and step on the user's multiplier over
/// the derived scale. Lives here rather than in `settings.rs` (which
/// re-exports it) because [`ui_scale`] is what gives the numbers meaning.
///
/// Every notch changes something: `ui_scale` quantizes only the derived
/// half, then applies the preference exactly. A step matching that
/// quantization was the old contract — it made the slider coarse for no
/// benefit once the preference stopped being quantized with everything
/// else.
pub const UI_SCALE_MIN: f32 = 0.70;
pub const UI_SCALE_MAX: f32 = 1.50;
pub const UI_SCALE_STEP: f32 = 0.05;

/// Whole-UI scale: the display's own scale factor × a window-size factor × the
/// user's preference. Reference: the design's own logical window at 1× with a
/// neutral preference is exactly the pre-scaling design (scale 1.0).
///
/// `scale_factor` is the OS's own setting, straight from the window — a display
/// at 125% arrives here as 1.25 and is applied as-is, which is why the slider
/// reads a *multiplier* and not an absolute size.
///
/// Only the derived half (`sf × win`) is quantized, onto a coarse grid: that
/// half moves continuously as a window is dragged or crosses onto another
/// display, and unquantized it shimmers. The user's preference is a deliberate
/// discrete choice, so it is applied exactly.
pub fn ui_scale(size: (u32, u32), scale_factor: f32, user: f32) -> f32 {
	let sf = scale_factor.max(0.5);
	let (lw, lh) = (size.0 as f32 / sf, size.1 as f32 / sf);
	let win = (lw / 1280.0)
		.min(lh / 800.0)
		.clamp(UI_SCALE_FLOOR, WINDOW_FACTOR_CAP);
	let user = if user.is_finite() {
		user.clamp(UI_SCALE_MIN, UI_SCALE_MAX)
	} else {
		1.0
	};
	let derived = ((sf * win) * 4.0).round().max(UI_SCALE_FLOOR * 4.0) / 4.0;
	(derived * user).min(UI_SCALE_CAP)
}

/// On-screen text box height at `scale`. Whole pixels: glyphs are
/// rasterized at exactly this size and blitted 1:1, which is what keeps
/// them crisp.
pub fn glyph_px(scale: f32) -> f32 {
	(BASE_TEXT_PX * scale).round().max(MIN_GLYPH_PX)
}

/// The debug overlay's pinned text size. It is a diagnostic, not UI: it
/// wants a fixed column budget and a stable line height regardless of
/// what the player set. Nothing the player reads uses this — hotbar
/// stack counts did until they turned out to just look half-size.
pub const FIXED_GLYPH_PX: f32 = BASE_TEXT_PX;

/// The smallest glyph worth rasterizing: below this the hinting
/// collapses and the run stops being readable at any scale.
const MIN_GLYPH_PX: f32 = 8.0;

/// Horizontal inset between a row's edge and its text; shared by the
/// menu's measurement pass and the overlay's rendering.
pub fn text_pad_x(scale: f32) -> f32 {
	8.0 * scale
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reference_window_is_unit_scale() {
		assert_eq!(ui_scale((1280, 800), 1.0, 1.0), 1.0);
		// The pre-scaling default window sits just under the reference
		// height: still a sane near-1 step, never below the floor.
		assert!(ui_scale((1280, 720), 1.0, 1.0) >= UI_SCALE_FLOOR);
	}

	#[test]
	fn retina_doubles_and_quantizes() {
		let s = ui_scale((2560, 1600), 2.0, 1.0); // same logical window, 2× glass
		assert_eq!(s, 2.0);
		// The DERIVED half is what's quantized: at the default preference
		// every result lands on that grid, never below the floor, never
		// above the cap.
		for w in (640..=3840).step_by(160) {
			for &sf in &[1.0f32, 1.5, 2.0, 3.0] {
				let s = ui_scale((w, w * 5 / 8), sf, 1.0);
				assert!((s * 4.0).fract().abs() < 1e-4, "{s} not a quarter step");
				assert!(
					(UI_SCALE_FLOOR..=UI_SCALE_CAP).contains(&s),
					"{s} out of range"
				);
			}
		}
	}

	#[test]
	fn window_factor_caps_both_ways() {
		// A tiny window can't shrink the UI below the floor...
		assert_eq!(ui_scale((640, 400), 1.0, 1.0), UI_SCALE_FLOOR);
		// ...and a huge one caps the window factor instead of growing on.
		assert_eq!(ui_scale((3840, 2400), 1.0, 1.0), WINDOW_FACTOR_CAP);
	}

	#[test]
	fn user_preference_multiplies_and_is_bounded() {
		let base = ui_scale((1280, 800), 1.0, 1.0);
		assert_eq!(
			ui_scale((1280, 800), 1.0, UI_SCALE_MAX),
			UI_SCALE_MAX * base
		);
		assert_eq!(
			ui_scale((1280, 800), 1.0, UI_SCALE_MIN),
			UI_SCALE_MIN * base
		);
		// A dense display at max preference is clamped, not unbounded.
		assert_eq!(ui_scale((2560, 1600), 3.0, UI_SCALE_MAX), UI_SCALE_CAP);
		// Out-of-range and garbage preferences clamp rather than escape.
		assert_eq!(ui_scale((1280, 800), 1.0, f32::NAN), base);
		assert_eq!(ui_scale((1280, 800), 1.0, 0.0), UI_SCALE_MIN * base);
		assert_eq!(ui_scale((1280, 800), 1.0, 99.0), UI_SCALE_MAX * base);
	}

	/// The point of a fine step: every notch of the slider has to change
	/// something. It used to be quantized away with the derived scale —
	/// at the reference window several adjacent notches all resolved to
	/// the same scale — which is why the step matched that grid before.
	#[test]
	fn every_slider_notch_changes_the_scale() {
		let notches = ((UI_SCALE_MAX - UI_SCALE_MIN) / UI_SCALE_STEP).round() as i32;
		for (size, sf) in [((1280u32, 800u32), 1.0f32), ((2560, 1600), 2.0)] {
			let mut prev = f32::NEG_INFINITY;
			for i in 0..=notches {
				let user = UI_SCALE_MIN + i as f32 * UI_SCALE_STEP;
				let s = ui_scale(size, sf, user);
				assert!(s > prev, "notch {user} did not grow the scale ({s})");
				prev = s;
			}
		}
	}

	// The ladder is continuous now, not the bitmap font's multiples of its
	// cell — that coarseness was a Nearest-sampler artifact and is what
	// made a UI-scale slider pointless before. Asserted as the properties
	// the blitter depends on rather than a table of products: a product
	// only restates the multiply, and all of them move together the day
	// the base size is retuned.
	#[test]
	fn text_size_tracks_scale_in_whole_pixels() {
		let mut prev = 0.0f32;
		for i in 0..=12 {
			let px = glyph_px(i as f32 / 4.0);
			assert_eq!(px, px.round(), "{px} is not a whole pixel");
			assert!(px >= MIN_GLYPH_PX, "{px} is under the floor");
			assert!(px >= prev, "{px} shrank as the scale grew");
			prev = px;
		}
		// The floor bites at the bottom and nowhere near unit scale.
		assert_eq!(glyph_px(0.0), MIN_GLYPH_PX);
		assert!(glyph_px(1.0) > MIN_GLYPH_PX);
		// A fixed-size glyph is the unit-scale one, wherever it is drawn.
		assert_eq!(FIXED_GLYPH_PX, glyph_px(1.0));
	}
}
