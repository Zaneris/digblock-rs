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

/// Floor on the whole UI scale: three quarter-steps, below which text
/// stops being comfortably readable. Also the bottom rung of
/// [`super::theme::fit_scale`]'s ladder.
pub const UI_SCALE_FLOOR: f32 = 0.75;

/// Whole-UI scale, quantized to quarter steps so live-resize walks
/// stable increments instead of shimmering. Reference: a 1280×800
/// logical window at 1× with the user preference at 1.0 is exactly the
/// pre-scaling design (scale 1.0); bigger windows, denser displays and a
/// higher preference all grow the UI from there.
pub fn ui_scale(size: (u32, u32), scale_factor: f32, user: f32) -> f32 {
	let sf = scale_factor.max(0.5);
	let (lw, lh) = (size.0 as f32 / sf, size.1 as f32 / sf);
	let win = (lw / 1280.0).min(lh / 800.0).clamp(UI_SCALE_FLOOR, 1.5);
	let user = if user.is_finite() { user.max(0.1) } else { 1.0 };
	(((sf * win * user) * 4.0).round().max(UI_SCALE_FLOOR * 4.0) / 4.0).min(UI_SCALE_CAP)
}

/// On-screen text box height at `scale`. Whole pixels: glyphs are
/// rasterized at exactly this size and blitted 1:1, which is what keeps
/// them crisp.
pub fn glyph_px(scale: f32) -> f32 {
	(BASE_TEXT_PX * scale).round().max(8.0)
}

/// Surfaces pinned to the pre-scaling size: hotbar stack counts (small
/// by design, per explicit request) and the debug panel (a diagnostic,
/// not UI).
pub const FIXED_GLYPH_PX: f32 = BASE_TEXT_PX;

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
		// The pre-scaling default window (1280×720) sits just under the
		// reference height: still a sane near-1 step, never below 0.75.
		assert!(ui_scale((1280, 720), 1.0, 1.0) >= 0.75);
	}

	#[test]
	fn retina_doubles_and_quantizes() {
		let s = ui_scale((2560, 1600), 2.0, 1.0); // same logical window, 2× glass
		assert_eq!(s, 2.0);
		// Quarter-step quantization: scan window sizes, every result is
		// a multiple of 0.25, never below the floor, never above the cap.
		for w in (640..=3840).step_by(160) {
			for &sf in &[1.0f32, 1.5, 2.0, 3.0] {
				for &u in &[0.75f32, 1.0, 1.5, 2.0] {
					let s = ui_scale((w, w * 5 / 8), sf, u);
					assert!((s * 4.0).fract().abs() < 1e-4, "{s} not a quarter step");
					assert!(
						(UI_SCALE_FLOOR..=UI_SCALE_CAP).contains(&s),
						"{s} out of range"
					);
				}
			}
		}
	}

	#[test]
	fn window_factor_caps_both_ways() {
		// A tiny window can't shrink the UI below 0.75×...
		assert_eq!(ui_scale((640, 400), 1.0, 1.0), 0.75);
		// ...and a huge 1× display caps at 1.5× the device factor.
		assert_eq!(ui_scale((3840, 2400), 1.0, 1.0), 1.5);
	}

	#[test]
	fn user_preference_multiplies_and_is_bounded() {
		let base = ui_scale((1280, 800), 1.0, 1.0);
		assert_eq!(ui_scale((1280, 800), 1.0, 2.0), 2.0 * base);
		assert_eq!(ui_scale((1280, 800), 1.0, 0.75), 0.75 * base);
		// A 2× retina window at max preference is clamped, not unbounded.
		assert_eq!(ui_scale((2560, 1600), 2.0, 2.0), UI_SCALE_CAP);
		// Garbage in never produces garbage scale.
		assert_eq!(ui_scale((1280, 800), 1.0, f32::NAN), base);
		assert!(ui_scale((1280, 800), 1.0, 0.0) >= 0.75);
	}

	// The ladder is continuous now (12/16/20/24…), not the bitmap font's
	// 8/16/24/32 — that coarseness was a Nearest-sampler artifact and is
	// what made a UI-scale slider pointless before.
	#[test]
	fn text_size_tracks_scale_in_whole_pixels() {
		assert_eq!(glyph_px(1.0), 16.0);
		assert_eq!(glyph_px(1.25), 20.0);
		assert_eq!(glyph_px(1.5), 24.0);
		assert_eq!(glyph_px(2.0), 32.0);
		assert_eq!(glyph_px(0.75), 12.0);
		assert_eq!(glyph_px(0.25), 8.0); // floor
		assert_eq!(FIXED_GLYPH_PX, 16.0);
		for i in 3..=12 {
			let px = glyph_px(i as f32 / 4.0);
			assert_eq!(px, px.round(), "{px} is not a whole pixel");
		}
	}
}
